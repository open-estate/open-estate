//! Single-purpose linked permit pipeline (`Stage-1` fetchers -> `Stage-2` lifecycle extractors).
//!
//! Eliminates false roof-replacement fabrication and solar-PV misclassification by separating
//! raw municipal/aggregator permit retrieval (`DOMAIN_PERMITS`) from domain-specific extraction
//! (`DOMAIN_PERMIT_INTEL`: Roof, Solar PV, MEP, Remodel/Addition, Code Enforcement).

use crate::db::shared_cache_store::compute_sha256_hex;
use crate::db::DbStore;
use crate::extensions::adapter::{BoxFuture, DataExtensionAdapter};
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain,
    CodeEnforcementIntelProto, DriverKind, ExtensionEnvelopeProto, ExtensionManifestProto,
    ExtensionQueryProto, GeoScopeProto, MepIntelProto, PermitRecordProto, PermitsHistoryProto,
    ProvenanceTier, RemodelIntelProto, RoofIntelProto, SolarIntelProto, SourceAttributionProto,
};
use crate::extensions::registry::ExtensionHub;
use crate::extensions::shared_cache::{
    dataset_ids, derive_geo_index_columns, derive_scope_key, get_shared_dataset_proto,
    put_shared_dataset_proto, shared_v1, ShareScope, SharedExtensionCache,
};
use crate::property::deep_research::fetch_municipal_open_data_stage1;
use crate::property::permits_condition::{classify_permit_category, normalize_permit_record};
use crate::property::public_records::{parse_street, street_keyword_of, with_data_point_anchor};
use crate::shared::{round_n, EnvConfig, HttpClient, SourceItem};
use anyhow::{anyhow, Result};
use chrono::Datelike;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

pub const CAMPBELL_ADDRESSES_LAYER: &str =
    "https://gis.campbellca.gov/arcgis/rest/services/BaseFeatureLayers/Addresses/MapServer/0";
pub const CAMPBELL_MGO_BASE: &str = "https://gis.campbellca.gov/arcgis/rest/services/MGO";

pub const CAMPBELL_BUILDING_LAYERS: &[(&str, &str)] = &[
    (
        "Permits_Service_Building_2026/0",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2026/MapServer/0",
    ),
    (
        "Permits_Service_Building_2025/0",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2025/MapServer/0",
    ),
    (
        "Permits_Service_Building_2024/0",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2024/MapServer/0",
    ),
    (
        "Permits_Service_Building_2023/0",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2023/MapServer/0",
    ),
    (
        "Permits_Service_Building_2022/1",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2022/MapServer/1",
    ),
    (
        "Permits_Service_Building_2021/2",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2021/MapServer/2",
    ),
    (
        "Permits_Service_Building_2020/3",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2020/MapServer/3",
    ),
    (
        "Permits_Service_Building_2019_1990/4",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Building_2019_1990/MapServer/4",
    ),
];

pub const CAMPBELL_PLANNING_LAYERS: &[(&str, &str)] = &[
    (
        "Permits_Service_Planning_2019_2025/0",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Planning_2019_2025/MapServer/0",
    ),
    (
        "Permits_Service_Planning_2019_2024/0",
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Planning_2019_2024/MapServer/0",
    ),
];

pub const CAMPBELL_CODE_ENF_LAYERS: &[(&str, bool, &str)] = &[
    (
        "Permits_Service_Code_Enforcement_Open/0",
        true,
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Code_Enforcement_Open/MapServer/0",
    ),
    (
        "Permits_Service_Code_Enforcement_Closed/1",
        false,
        "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Code_Enforcement_Closed/MapServer/1",
    ),
];

static RE_SOLAR_KW: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(\d+(?:\.\d+)?)\s*(?:kw|kwp|kwac|kwdc)\b").unwrap());
static RE_SOLAR_WATTS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(\d{3,6}(?:\.\d+)?)\s*(?:w|watts?)\b").unwrap());
static RE_SOLAR_PANELS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(\d{1,3})\s*(?:panels?|modules?)\b").unwrap());
static RE_RANGE_PAIR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{1,6})\s*[-–]\s*(\d{1,6})\b").unwrap());

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CampbellAddressPoint {
    pub apn: Option<String>,
    pub full_address: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Default)]
pub struct CampbellMgoBundle {
    pub permits: Vec<Value>,
    pub code_items: Vec<Value>,
    pub code_checked: bool,
    pub queried_layers: Vec<String>,
    pub sources: Vec<SourceItem>,
}

fn esc_sql(s: &str) -> String {
    s.replace('\'', "''")
}

fn json_str(v: Option<&Value>) -> Option<String> {
    let raw = match v? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if raw.is_empty() || raw.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(raw)
    }
}

fn parse_arcgis_date(val: Option<&Value>) -> Option<String> {
    let v = val?;
    if let Some(ms) = v.as_i64() {
        if ms > 100_000_000 {
            if let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) {
                return Some(dt.format("%Y-%m-%d").to_string());
            }
        }
    }
    let s = v.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if s.len() >= 10 && s.as_bytes().get(4) == Some(&b'-') {
        return Some(s[..10].to_string());
    }
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 8 {
        Some(format!(
            "{}-{}-{}",
            &digits[0..4],
            &digits[4..6],
            &digits[6..8]
        ))
    } else if s.len() >= 4 {
        Some(s.to_string())
    } else {
        None
    }
}

/// Extracts `(kw, panel_count)` cleanly from solar PV descriptions such as
/// `"Roof Mounted PV System 2.870 kW (7 Panels)"`.
pub fn extract_solar_specs(text: &str) -> (Option<f64>, Option<u32>) {
    let kw = RE_SOLAR_KW
        .captures(text)
        .and_then(|c| c.get(1)?.as_str().parse::<f64>().ok())
        .or_else(|| {
            RE_SOLAR_WATTS
                .captures(text)
                .and_then(|c| c.get(1)?.as_str().parse::<f64>().ok())
                .map(|w| w / 1000.0)
        })
        .filter(|&k| k > 0.05 && k < 500.0)
        .map(|k| round_n(k, 3));
    let panels = RE_SOLAR_PANELS
        .captures(text)
        .and_then(|c| c.get(1)?.as_str().parse::<u32>().ok())
        .filter(|&p| p > 0 && p < 2000);
    (kw, panels)
}

/// Matches a Campbell MGO or municipal street address against `(house_number, street_keyword)`,
/// supporting multi-unit/building strings like `"114 SYNTHPLATFORM WAY -126 BLDG 3"` and `"114-128 SYNTHPLATFORM WAY"`
/// while rejecting prefix false positives like `"1260 SYNTHPLATFORM WAY"` or `"BLDG 126"`.
pub fn campbell_address_matches(raw_addr: &str, target_number: &str, target_street_kw: &str) -> bool {
    let raw_up = raw_addr.trim().to_ascii_uppercase();
    let num_trim = target_number.trim().to_ascii_uppercase();
    let kw_trim = target_street_kw.trim().to_ascii_uppercase();
    if raw_up.is_empty() || num_trim.is_empty() || kw_trim.is_empty() {
        return false;
    }
    let has_street = kw_trim
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .all(|tok| raw_up.contains(tok));
    if !has_street {
        return false;
    }

    let tokens: Vec<String> = raw_up
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    let unit_prefixes = [
        "BLDG", "BUILDING", "UNIT", "APT", "STE", "SUITE", "LOT", "FL", "FLOOR", "RM", "ROOM",
    ];
    for (idx, tok) in tokens.iter().enumerate() {
        if tok == &num_trim {
            let preceded_by_unit = idx > 0 && unit_prefixes.contains(&tokens[idx - 1].as_str());
            if !preceded_by_unit {
                return true;
            }
        }
    }

    if let Ok(target_n) = num_trim.parse::<i64>() {
        for cap in RE_RANGE_PAIR.captures_iter(&raw_up) {
            if let (Ok(a), Ok(b)) = (cap[1].parse::<i64>(), cap[2].parse::<i64>()) {
                if a <= b && (b - a) <= 40 && target_n >= a && target_n <= b {
                    return true;
                }
            }
        }
    }
    false
}

/// Builds the Campbell `Addresses/MapServer/0` SQL `WHERE` clause (`HouseNumber` is
/// `esriFieldTypeSmallInteger`, so validated integer street numbers must not be single-quoted).
fn build_campbell_address_where(number: &str, kw: &str) -> Option<String> {
    let num_trim = number.trim();
    let kw_trim = kw.trim();
    if num_trim.is_empty() || kw_trim.is_empty() {
        return None;
    }
    let digits: String = num_trim
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let house_clause = match digits.parse::<i16>().ok().filter(|&n| n > 0) {
        Some(n) => format!("HouseNumber={n}"),
        None => format!(
            "UPPER(FullAddress) LIKE '{} %'",
            esc_sql(&num_trim.to_ascii_uppercase())
        ),
    };
    let kw_esc = esc_sql(&kw_trim.to_ascii_uppercase());
    Some(format!(
        "{house_clause} AND (UPPER(StreetName) LIKE '%{kw_esc}%' OR UPPER(FullAddress) LIKE '%{kw_esc}%')"
    ))
}

/// Queries City of Campbell `BaseFeatureLayers/Addresses/MapServer/0` to resolve the exact
/// subject `APN` and parcel address coordinates before falling back to parcel spatial queries.
pub async fn fetch_campbell_address_point(
    http: &HttpClient,
    address: &str,
) -> Option<CampbellAddressPoint> {
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let where_c = build_campbell_address_where(&number, &kw)?;
    let res = http
        .arcgis_query(
            CAMPBELL_ADDRESSES_LAYER,
            &[
                ("where", where_c),
                (
                    "outFields",
                    "APN,FullAddress,HouseNumber,StreetName,StreetType,StreetNameFull,UnitNumber,BuildingNumber,Status"
                        .to_string(),
                ),
                ("returnGeometry", "true".to_string()),
                ("outSR", "4326".to_string()),
            ],
            5000,
        )
        .await
        .ok()?;
    let feats = res.get("features").and_then(|v| v.as_array())?;
    for f in feats {
        let Some(attrs) = f.get("attributes") else {
            continue;
        };
        let full_addr = json_str(attrs.get("FullAddress")).unwrap_or_default();
        if !full_addr.is_empty() && !campbell_address_matches(&full_addr, &number, &kw) {
            continue;
        }
        let geom = f.get("geometry")?;
        let (Some(lon), Some(lat)) = (
            geom.get("x").and_then(|v| v.as_f64()),
            geom.get("y").and_then(|v| v.as_f64()),
        ) else {
            continue;
        };
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        let apn = json_str(attrs.get("APN")).map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        });
        return Some(CampbellAddressPoint {
            apn: apn.filter(|s| !s.is_empty()),
            full_address: full_addr,
            lat,
            lon,
        });
    }
    None
}

/// Resolves distinct street addresses registered to `apn_digits` in Campbell `Addresses/MapServer/0`
/// (used to discover parent subdivision lot situs such as `320 SYNTHVIRGINIA AVE` from `PREV_APN`).
pub async fn fetch_campbell_addresses_for_apn(http: &HttpClient, apn_digits: &str) -> Vec<String> {
    let clean: String = apn_digits
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if clean.is_empty() {
        return Vec::new();
    }
    let where_c = format!("APN='{}'", esc_sql(&clean));
    let Ok(res) = http
        .arcgis_query(
            CAMPBELL_ADDRESSES_LAYER,
            &[
                ("where", where_c),
                ("outFields", "FullAddress".to_string()),
                ("returnGeometry", "false".to_string()),
            ],
            4000,
        )
        .await
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(arr) = res.get("features").and_then(|v| v.as_array()) {
        for f in arr {
            if let Some(a) = f
                .get("attributes")
                .and_then(|at| json_str(at.get("FullAddress")))
            {
                let up = a.to_ascii_uppercase();
                if seen.insert(up.clone()) {
                    out.push(up);
                }
            }
        }
    }
    out
}

fn build_mgo_where(targets: &[(&str, &str)]) -> Option<String> {
    let mut clauses = Vec::new();
    for (num, kw) in targets {
        if num.is_empty() || kw.is_empty() {
            continue;
        }
        let n = esc_sql(&num.to_ascii_uppercase());
        let k = esc_sql(&kw.to_ascii_uppercase());
        clauses.push(format!(
            "((UPPER(Address) LIKE '%{n}%' AND UPPER(Address) LIKE '%{k}%') OR (UPPER(AddressGIS) LIKE '%{n}%' AND UPPER(AddressGIS) LIKE '%{k}%'))"
        ));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" OR "))
    }
}

/// Fetches ALL City of Campbell MGO Building (`2025/0` incl. 2026 + `2024/0`..`2019_1990/4`),
/// Planning (`2019_2025/0`, `2019_2024/0`), and Code Enforcement (`Open/0`, `Closed/1`) layers
/// with strict PII exclusion and subject-vs-parent-lot separation.
pub async fn fetch_campbell_mgo_bundle(
    http: &HttpClient,
    address: &str,
    parent_addresses: &[String],
) -> CampbellMgoBundle {
    fetch_campbell_mgo_bundle_with_env(http, None, address, parent_addresses).await
}

/// Variant of [`fetch_campbell_mgo_bundle`] honoring an optional `CAMPBELL_MGO_BASE_URL`
/// override in `EnvConfig` for staging/unavailability verification.
pub async fn fetch_campbell_mgo_bundle_with_env(
    http: &HttpClient,
    env: Option<&EnvConfig>,
    address: &str,
    parent_addresses: &[String],
) -> CampbellMgoBundle {
    let mut bundle = CampbellMgoBundle::default();
    let (subj_num, subj_core) = parse_street(address);
    let subj_kw = street_keyword_of(&subj_core);
    if subj_num.is_empty() || subj_kw.is_empty() {
        return bundle;
    }

    let mut parent_pairs: Vec<(String, String, String)> = Vec::new();
    for pa in parent_addresses {
        let (pn, pc) = parse_street(pa);
        let pkw = street_keyword_of(&pc);
        if !pn.is_empty() && !pkw.is_empty() && !(pn == subj_num && pkw == subj_kw) {
            parent_pairs.push((pn, pkw, pa.clone()));
        }
    }

    let mut target_refs: Vec<(&str, &str)> = vec![(&subj_num, &subj_kw)];
    for (pn, pkw, _) in &parent_pairs {
        target_refs.push((pn.as_str(), pkw.as_str()));
    }
    let Some(where_sql) = build_mgo_where(&target_refs) else {
        return bundle;
    };

    // Strictly exclude CaseManager, CMEmail, CMPhone, ProjectName, Owner*, Applicant*, Contractor*.
    let p_fields = "ProjectNumber,ApplicationNumber,Address,AddressGIS,ProjectType,DesignationType,WorkType,Status,ProjectDescription,Year,CreateDate,IssuedDate,UpdateDate";
    let base_params = [
        ("where", where_sql.clone()),
        ("outFields", p_fields.to_string()),
        ("returnGeometry", "false".to_string()),
    ];
    let base_ov = env
        .and_then(|e| e.get("CAMPBELL_MGO_BASE_URL"))
        .map(|u| u.trim_end_matches('/').to_string());
    let resolve_layer_url = |default_url: &str| -> String {
        match &base_ov {
            Some(ov) => default_url.replacen(CAMPBELL_MGO_BASE, ov, 1),
            None => default_url.to_string(),
        }
    };

    let bld_urls: Vec<(&'static str, String)> = CAMPBELL_BUILDING_LAYERS
        .iter()
        .map(|&(name, url)| (name, resolve_layer_url(url)))
        .collect();
    let pln_urls: Vec<(&'static str, String)> = CAMPBELL_PLANNING_LAYERS
        .iter()
        .map(|&(name, url)| (name, resolve_layer_url(url)))
        .collect();
    let ce_urls: Vec<(&'static str, bool, String)> = CAMPBELL_CODE_ENF_LAYERS
        .iter()
        .map(|&(name, is_open, url)| (name, is_open, resolve_layer_url(url)))
        .collect();

    let bld_futs: Vec<_> = bld_urls
        .iter()
        .map(|(name, url)| {
            let params_ref = &base_params;
            async move {
                (
                    *name,
                    url.as_str(),
                    "Building Division",
                    http.arcgis_query(url, params_ref, 4500).await,
                )
            }
        })
        .collect();
    let pln_futs: Vec<_> = pln_urls
        .iter()
        .map(|(name, url)| {
            let params_ref = &base_params;
            async move {
                (
                    *name,
                    url.as_str(),
                    "Planning Division",
                    http.arcgis_query(url, params_ref, 4000).await,
                )
            }
        })
        .collect();
    let ce_futs: Vec<_> = ce_urls
        .iter()
        .map(|(name, is_open, url)| {
            let params_ref = &base_params;
            async move {
                (
                    *name,
                    *is_open,
                    url.as_str(),
                    http.arcgis_query(url, params_ref, 4000).await,
                )
            }
        })
        .collect();

    let (bld_res, pln_res, ce_res) = tokio::join!(
        futures::future::join_all(bld_futs),
        futures::future::join_all(pln_futs),
        futures::future::join_all(ce_futs),
    );

    let now_year = chrono::Utc::now().year();
    let mut seen_permits = HashSet::new();
    for (layer_name, layer_url, div_default, res) in bld_res.into_iter().chain(pln_res) {
        let Ok(doc) = res else {
            continue;
        };
        if doc.get("error").is_some() {
            continue;
        }
        bundle.queried_layers.push(layer_name.to_string());
        let Some(feats) = doc.get("features").and_then(|v| v.as_array()) else {
            continue;
        };
        for f in feats {
            let Some(p) = f.get("attributes") else {
                continue;
            };
            let raw_addr = json_str(p.get("Address"))
                .or_else(|| json_str(p.get("AddressGIS")))
                .unwrap_or_default();
            let is_subj = campbell_address_matches(&raw_addr, &subj_num, &subj_kw);
            let parent_match = if is_subj {
                None
            } else {
                parent_pairs
                    .iter()
                    .find(|(pn, pkw, _)| campbell_address_matches(&raw_addr, pn, pkw))
                    .map(|(_, _, full)| full.clone())
            };
            if !is_subj && parent_match.is_none() {
                continue;
            }
            let Some(num) =
                json_str(p.get("ProjectNumber")).or_else(|| json_str(p.get("ApplicationNumber")))
            else {
                continue;
            };
            let dedup_key = format!(
                "{}:{}",
                num.to_ascii_uppercase(),
                if is_subj { "subject" } else { "parent_lot" }
            );
            if !seen_permits.insert(dedup_key) {
                continue;
            }
            let issue_dt = parse_arcgis_date(p.get("IssuedDate"));
            let applied_dt = parse_arcgis_date(p.get("CreateDate"));
            let dt = issue_dt
                .clone()
                .or_else(|| applied_dt.clone())
                .or_else(|| json_str(p.get("Year")));
            let where_num = format!("ProjectNumber='{}'", esc_sql(&num));
            let q_enc = urlencoding::encode(&where_num);
            let ds_url = with_data_point_anchor(
                &format!(
                    "{layer_url}/query?where={q_enc}&outFields={p_fields}&returnGeometry=false&f=html"
                ),
                None,
                Some(&num),
            );
            let portal_url = if div_default == "Planning Division" {
                "https://www.campbellca.gov/181/Planning"
            } else {
                "https://www.campbellca.gov/161/Building"
            };
            let work_type = json_str(p.get("WorkType"))
                .or_else(|| json_str(p.get("DesignationType")))
                .or_else(|| json_str(p.get("ProjectType")))
                .unwrap_or_else(|| "Municipal Permit".to_string());
            let raw_rec = json!({
                "number": num,
                "permitNumber": num,
                "applicationNumber": json_str(p.get("ApplicationNumber")),
                "address": raw_addr,
                "date": dt,
                "issueDate": issue_dt,
                "appliedDate": applied_dt,
                "year": json_str(p.get("Year")).and_then(|y| y.parse::<i64>().ok()),
                "division": json_str(p.get("ProjectType")).or_else(|| Some(div_default.to_string())),
                "workType": work_type,
                "type": work_type,
                "status": json_str(p.get("Status")).unwrap_or_else(|| "Recorded".to_string()),
                "description": json_str(p.get("ProjectDescription")),
                "scope": if is_subj { "subject" } else { "parent_lot" },
                "isParentLot": !is_subj,
                "parentLotAddress": parent_match,
                "sourceVerified": true,
                "sourceName": format!("City of Campbell {div_default} (MGO GIS)"),
                "sourceUrl": ds_url,
                "permitPortalUrl": with_data_point_anchor(portal_url, None, Some(&num)),
                "datasetUrl": ds_url,
            });
            if let Some(norm) = normalize_permit_record(
                &raw_rec,
                &format!("City of Campbell {div_default} (MGO GIS)"),
                Some(portal_url),
                now_year,
            ) {
                bundle.permits.push(norm);
            }
        }
    }

    let mut seen_ce = HashSet::new();
    for (layer_name, is_open, layer_url, res) in ce_res {
        let Ok(doc) = res else {
            continue;
        };
        if doc.get("error").is_some() {
            continue;
        }
        bundle.code_checked = true;
        bundle.queried_layers.push(layer_name.to_string());
        let Some(feats) = doc.get("features").and_then(|v| v.as_array()) else {
            continue;
        };
        for f in feats {
            let Some(p) = f.get("attributes") else {
                continue;
            };
            let raw_addr = json_str(p.get("Address"))
                .or_else(|| json_str(p.get("AddressGIS")))
                .unwrap_or_default();
            let is_subj = campbell_address_matches(&raw_addr, &subj_num, &subj_kw);
            let is_parent = !is_subj
                && parent_pairs
                    .iter()
                    .any(|(pn, pkw, _)| campbell_address_matches(&raw_addr, pn, pkw));
            if !is_subj && !is_parent {
                continue;
            }
            let Some(num) =
                json_str(p.get("ProjectNumber")).or_else(|| json_str(p.get("ApplicationNumber")))
            else {
                continue;
            };
            if !seen_ce.insert(num.clone()) {
                continue;
            }
            let where_num = format!("ProjectNumber='{}'", esc_sql(&num));
            let q_enc = urlencoding::encode(&where_num);
            let ds_url = with_data_point_anchor(
                &format!(
                    "{layer_url}/query?where={q_enc}&outFields={p_fields}&returnGeometry=false&f=html"
                ),
                None,
                Some(&num),
            );
            bundle.code_items.push(json!({
                "number": num,
                "open": is_open,
                "address": raw_addr,
                "scope": if is_subj { "subject" } else { "parent_lot" },
                "isParentLot": !is_subj,
                "date": parse_arcgis_date(p.get("CreateDate")).or_else(|| json_str(p.get("Year"))),
                "status": json_str(p.get("Status")).unwrap_or_else(|| if is_open { "Open" } else { "Closed" }.to_string()),
                "description": json_str(p.get("ProjectDescription")).or_else(|| json_str(p.get("WorkType"))),
                "sourceName": "City of Campbell Code Enforcement GIS",
                "sourceUrl": ds_url,
                "permitPortalUrl": with_data_point_anchor("https://www.campbellca.gov/199/Code-Enforcement", None, Some(&num)),
                "datasetUrl": ds_url,
            }));
        }
    }

    if !bundle.permits.is_empty() || bundle.code_checked {
        bundle.sources.push(SourceItem::new(
            "City of Campbell Building, Planning & Code Enforcement GIS (MGO)",
            "https://www.campbellca.gov/161/Building",
        ));
    }
    bundle
}

/// Two-step Shovels v2 API lookup:
/// 1) `GET /v2/addresses/search?q=<address>` -> resolve `geo_id`
/// 2) `GET /v2/permits/search?geo_id=<geo_id>&permit_from=1990-01-01&size=50`
pub async fn fetch_shovels_v2_permits(
    http: &HttpClient,
    env: &EnvConfig,
    address: &str,
    portal_url: Option<&str>,
) -> (Vec<Value>, bool, Option<String>) {
    let Some(api_key) = env.get("SHOVELS_API_KEY") else {
        return (Vec::new(), false, None);
    };
    let raw_base = env
        .get("SHOVELS_API_BASE_URL")
        .unwrap_or("https://api.shovels.ai/v2")
        .trim_end_matches('/');
    let v2_base = if raw_base.ends_with("/v2") {
        raw_base.to_string()
    } else {
        format!("{raw_base}/v2")
    };
    let hdrs = [("X-API-Key", api_key), ("Accept", "application/json")];
    let q_addr = urlencoding::encode(address.trim());
    let addr_url = format!("{v2_base}/addresses/search?q={q_addr}");
    let geo_id = match http.get_json_opts(&addr_url, 6000, 1, 86400, Some(&hdrs)).await {
        Ok(resp) => {
            let items = resp
                .get("items")
                .or_else(|| resp.get("data"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            items.into_iter().find_map(|item| {
                json_str(item.get("geo_id")).or_else(|| json_str(item.get("id")))
            })
        }
        Err(_) => {
            return (
                Vec::new(),
                false,
                Some(
                    "Shovels address resolution unavailable or timed out — fell back to municipal GIS & open-data permit records."
                        .into(),
                ),
            );
        }
    };
    let Some(gid) = geo_id else {
        return (Vec::new(), true, None);
    };
    let q_gid = urlencoding::encode(&gid);
    let permits_url = format!("{v2_base}/permits/search?geo_id={q_gid}&permit_from=1990-01-01&size=50");
    match http
        .get_json_opts(&permits_url, 7500, 1, 86400, Some(&hdrs))
        .await
    {
        Ok(resp) => {
            let arr = resp
                .get("items")
                .or_else(|| resp.get("permits"))
                .or_else(|| resp.get("data"))
                .and_then(|v| v.as_array())
                .or_else(|| resp.as_array())
                .cloned()
                .unwrap_or_default();
            let now_year = chrono::Utc::now().year();
            let out: Vec<Value> = arr
                .iter()
                .filter_map(|item| {
                    normalize_permit_record(
                        item,
                        "Shovels 50-State Building Permits API",
                        portal_url,
                        now_year,
                    )
                })
                .collect();
            (out, true, None)
        }
        Err(_) => (
            Vec::new(),
            false,
            Some(
                "Shovels building permit API unavailable or timed out — fell back to municipal GIS & open-data permit records."
                    .into(),
            ),
        ),
    }
}

pub fn permit_value_to_proto(v: &Value) -> PermitRecordProto {
    let raw_type = v
        .get("rawType")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let desc = v
        .get("description")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let category = {
        let existing = v.get("category").and_then(|x| x.as_str()).unwrap_or("");
        let reclassified = classify_permit_category(&raw_type, &desc);
        if existing.is_empty()
            || (existing == "reroof" && reclassified != "reroof")
            || (existing == "other" && reclassified != "other")
        {
            reclassified.to_string()
        } else {
            existing.to_string()
        }
    };
    let is_parent = v
        .get("isParentLot")
        .and_then(|x| x.as_bool())
        .unwrap_or_else(|| v.get("scope").and_then(|x| x.as_str()) == Some("parent_lot"));
    let finaled = v
        .get("finaled")
        .and_then(|x| x.as_bool())
        .unwrap_or_else(|| v.get("finalDate").and_then(|x| x.as_str()).is_some());
    PermitRecordProto {
        permit_number: v
            .get("permitNumber")
            .or_else(|| v.get("number"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        category,
        raw_type,
        description: desc,
        address: v
            .get("address")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        status: v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        final_date: v
            .get("finalDate")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        issue_date: v
            .get("issueDate")
            .or_else(|| v.get("date"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        applied_date: v
            .get("appliedDate")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        year: v.get("year").and_then(|x| x.as_i64()).map(|y| y as i32),
        finaled,
        verified: v.get("verified").and_then(|x| x.as_bool()).unwrap_or(false),
        source_verified: v
            .get("sourceVerified")
            .and_then(|x| x.as_bool())
            .unwrap_or_else(|| v.get("verified").and_then(|x| x.as_bool()).unwrap_or(false)),
        is_parent_lot: is_parent,
        scope: if is_parent {
            "parent_lot".to_string()
        } else {
            "subject".to_string()
        },
        solar_kw: v.get("solarKw").and_then(|x| x.as_f64()),
        solar_panels: v
            .get("solarPanels")
            .and_then(|x| x.as_u64())
            .map(|n| n as u32),
        valuation_usd: v.get("jobValueUSD").and_then(|x| x.as_i64()),
        contractor: v
            .get("contractorName")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        source: v
            .get("sourceName")
            .or_else(|| v.get("source"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        source_url: v
            .get("sourceUrl")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        apn: v
            .get("apn")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn effective_category(p: &Value) -> &'static str {
    let raw_type = p.get("rawType").and_then(|v| v.as_str()).unwrap_or("");
    let desc = p.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let existing = p.get("category").and_then(|v| v.as_str()).unwrap_or("");
    let reclassified = classify_permit_category(raw_type, desc);
    if existing.is_empty() || (existing == "reroof" && reclassified != "reroof") {
        reclassified
    } else if existing == "other" && reclassified != "other" {
        reclassified
    } else {
        match existing {
            "reroof" => "reroof",
            "new_construction" => "new_construction",
            "solar" => "solar",
            "hvac" => "hvac",
            "water_heater" => "water_heater",
            "electrical" => "electrical",
            "plumbing" => "plumbing",
            "remodel" | "remodel_addition" => "remodel",
            "addition" => "addition",
            "adu" => "adu",
            "seismic" | "seismic_foundation" => "seismic",
            "foundation" => "foundation",
            "pool" => "pool",
            "windows" => "windows",
            "demolition" => "demolition",
            "planning" => "planning",
            "code_enforcement" => "code_enforcement",
            _ => reclassified,
        }
    }
}

fn is_subject_permit(p: &Value) -> bool {
    !p.get("isParentLot")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| p.get("scope").and_then(|v| v.as_str()) == Some("parent_lot"))
}

fn is_permit_finaled(p: &Value) -> bool {
    if let Some(b) = p.get("finaled").and_then(|v| v.as_bool()) {
        return b;
    }
    if p.get("finalDate")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    let st = p
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    st.contains("final") || st.contains("complete") || st == "closed"
}

fn is_permit_issued(p: &Value) -> bool {
    if let Some(b) = p.get("issued").and_then(|v| v.as_bool()) {
        return b;
    }
    if p.get("issueDate")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    let st = p
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    st.contains("issued") || st.contains("active") || st.contains("approved")
}

/// Stage-2 Roof Lifecycle Extractor (`DOMAIN_PERMIT_INTEL`): strictly separates verified finaled
/// subject reroof permits, issued unfinaled reroofs, new-construction permits, solar PV exclusions,
/// and missing-year unknowns without inventing fake years (e.g. `1985` or `now - 19`).
pub fn extract_roof_lifecycle(
    permits: &[Value],
    year_built: Option<i32>,
    _eff_year_built: Option<i32>,
    roof_material: Option<&str>,
    now_year: i32,
) -> RoofIntelProto {
    let mut verified_reroof_year: Option<i32> = None;
    let mut issued_reroof_year: Option<i32> = None;
    let mut new_construction_year: Option<i32> = None;
    let mut matching = Vec::new();
    let mut new_build_label: Option<String> = None;

    for p in permits {
        if !is_subject_permit(p) {
            continue;
        }
        let cat = effective_category(p);
        let yr = p
            .get("year")
            .and_then(|v| v.as_i64())
            .map(|y| y as i32)
            .filter(|&y| y >= 1940 && y <= now_year + 1);
        let src_ok = p
            .get("sourceVerified")
            .or_else(|| p.get("verified"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if cat == "reroof" {
            matching.push(permit_value_to_proto(p));
            if is_permit_finaled(p) && p.get("verified").and_then(|v| v.as_bool()).unwrap_or(false) {
                if let Some(y) = yr {
                    verified_reroof_year = Some(verified_reroof_year.map_or(y, |c| c.max(y)));
                }
            } else if src_ok && is_permit_issued(p) {
                if let Some(y) = yr {
                    issued_reroof_year = Some(issued_reroof_year.map_or(y, |c| c.max(y)));
                }
            }
        } else if cat == "new_construction" {
            matching.push(permit_value_to_proto(p));
            if let Some(y) = yr {
                if new_construction_year.map(|c| y >= c).unwrap_or(true) {
                    new_construction_year = Some(y);
                    new_build_label = p
                        .get("permitNumber")
                        .and_then(|v| v.as_str())
                        .map(|n| {
                            let st = p
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Recorded");
                            format!("{n} ({y}, {st})")
                        });
                }
            }
        }
    }

    let valid_yb = year_built.filter(|&y| y >= 1700 && y <= now_year + 1);
    let reroof_verified = verified_reroof_year.is_some();

    let (verification_status, effective_year, effective_year_source, note) =
        if let Some(vy) = verified_reroof_year {
            (
                "verified_reroof".to_string(),
                Some(vy),
                "verified_reroof_permit".to_string(),
                format!("Finaled reroof permit verified ({vy})."),
            )
        } else if let Some(iy) = issued_reroof_year {
            (
                "unverified".to_string(),
                Some(iy),
                "issued_reroof_permit".to_string(),
                format!(
                    "Reroof permit issued ({iy}) without recorded final inspection; confirm finaled inspection card."
                ),
            )
        } else if let Some(ny) = new_construction_year {
            (
                "new_construction".to_string(),
                Some(ny),
                "new_construction_permit".to_string(),
                format!(
                    "New construction permit {}; original roof — no separate reroof permit on file.",
                    new_build_label.unwrap_or_else(|| format!("({ny})"))
                ),
            )
        } else if let Some(yb) = valid_yb {
            (
                "unverified".to_string(),
                Some(yb),
                "year_built_unverified".to_string(),
                format!(
                    "No verified reroof permit found in checked records (built {yb}); confirm roof age & inspection certification."
                ),
            )
        } else {
            (
                "unverified".to_string(),
                None,
                "unknown".to_string(),
                "No verified reroof permit found and year built is unavailable; obtain roof age & certification."
                    .to_string(),
            )
        };

    let clean_mat = roof_material
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let material_source = if clean_mat.is_some() {
        "recorded".to_string()
    } else {
        "unknown".to_string()
    };

    RoofIntelProto {
        verification_status,
        replacement_date_verified: reroof_verified,
        reroof_verified,
        verified_reroof_year,
        issued_reroof_year,
        new_construction_permit_year: new_construction_year,
        effective_year,
        effective_year_source,
        material: clean_mat,
        material_source,
        note,
        matching_permits: matching,
    }
}

pub fn roof_intel_to_json(r: &RoofIntelProto) -> Value {
    json!({
        "verificationStatus": r.verification_status,
        "replacementDateVerified": r.replacement_date_verified,
        "reroofVerified": r.reroof_verified,
        "permitVerified": r.reroof_verified,
        "verifiedReroofYear": r.verified_reroof_year,
        "issuedReroofYear": r.issued_reroof_year,
        "newConstructionPermitYear": r.new_construction_permit_year,
        "effectiveYear": r.effective_year,
        "installedOrEffectiveYear": r.effective_year,
        "effectiveYearSource": r.effective_year_source,
        "material": r.material,
        "materialSource": r.material_source,
        "note": r.note,
    })
}

/// Stage-2 Solar PV Extractor (`DOMAIN_PERMIT_INTEL`): extracts subject-parcel PV permits, total
/// system kW, and panel counts for downstream utility/solar offset modeling.
pub fn extract_solar_pv(permits: &[Value]) -> SolarIntelProto {
    let mut solar_permits = Vec::new();
    let mut total_kw = 0.0_f64;
    let mut kw_seen = false;
    let mut panel_sum = 0_u32;
    let mut panels_seen = false;

    for p in permits {
        if !is_subject_permit(p) || effective_category(p) != "solar" {
            continue;
        }
        let rec = permit_value_to_proto(p);
        if let Some(kw) = rec.solar_kw.or_else(|| {
            let txt = format!("{} {}", rec.raw_type, rec.description);
            extract_solar_specs(&txt).0
        }) {
            total_kw += kw;
            kw_seen = true;
        }
        if let Some(cnt) = rec.solar_panels.or_else(|| {
            let txt = format!("{} {}", rec.raw_type, rec.description);
            extract_solar_specs(&txt).1
        }) {
            panel_sum += cnt;
            panels_seen = true;
        }
        solar_permits.push(rec);
    }

    let has_solar = !solar_permits.is_empty();
    let kw_opt = kw_seen.then(|| round_n(total_kw, 3));
    let panels_opt = panels_seen.then_some(panel_sum);
    let est_annual_kwh = kw_opt.map(|kw| (kw * 1450.0).round());
    let note = if has_solar {
        match (kw_opt, panels_opt) {
            (Some(kw), Some(pan)) => format!(
                "{} subject solar PV permit(s) found totaling {kw:.3} kW ({pan} panels).",
                solar_permits.len()
            ),
            (Some(kw), None) => format!(
                "{} subject solar PV permit(s) found totaling {kw:.3} kW.",
                solar_permits.len()
            ),
            _ => format!("{} subject solar PV permit(s) found.", solar_permits.len()),
        }
    } else {
        "No subject-parcel solar PV permits on file.".to_string()
    };

    SolarIntelProto {
        has_solar,
        total_kw: kw_opt,
        panel_count: panels_opt,
        estimated_annual_kwh: est_annual_kwh,
        permits: solar_permits,
        note,
    }
}

pub fn solar_intel_to_json(s: &SolarIntelProto) -> Value {
    json!({
        "hasSolar": s.has_solar,
        "totalKw": s.total_kw,
        "panelCount": s.panel_count,
        "estimatedAnnualKwh": s.estimated_annual_kwh,
        "permitCount": s.permits.len(),
        "permits": s.permits.iter().map(|p| json!({
            "permitNumber": p.permit_number,
            "status": p.status,
            "finaled": p.finaled,
            "year": p.year,
            "solarKw": p.solar_kw,
            "solarPanels": p.solar_panels,
            "description": p.description,
            "sourceUrl": p.source_url,
        })).collect::<Vec<_>>(),
        "note": s.note,
    })
}

/// Stage-2 MEP Extractor (`DOMAIN_PERMIT_INTEL`).
pub fn extract_mep_intel(permits: &[Value]) -> MepIntelProto {
    let mut matching = Vec::new();
    let (mut has_hvac, mut has_elec, mut has_plumb) = (false, false, false);
    let (mut has_ev, mut has_hp) = (false, false);
    let (mut hvac_yr, mut elec_yr, mut plumb_yr): (Option<i32>, Option<i32>, Option<i32>) =
        (None, None, None);

    for p in permits {
        if !is_subject_permit(p) {
            continue;
        }
        let cat = effective_category(p);
        let txt = format!(
            "{} {}",
            p.get("rawType").and_then(|v| v.as_str()).unwrap_or(""),
            p.get("description").and_then(|v| v.as_str()).unwrap_or("")
        )
        .to_ascii_lowercase();
        let yr = p.get("year").and_then(|v| v.as_i64()).map(|y| y as i32);
        match cat {
            "hvac" => {
                has_hvac = true;
                if txt.contains("heat pump") || txt.contains("mini-split") {
                    has_hp = true;
                }
                if let Some(y) = yr {
                    hvac_yr = Some(hvac_yr.map_or(y, |c| c.max(y)));
                }
                matching.push(permit_value_to_proto(p));
            }
            "electrical" => {
                has_elec = true;
                if txt.contains("ev charger") || txt.contains("car charger") || txt.contains("evse")
                {
                    has_ev = true;
                }
                if let Some(y) = yr {
                    elec_yr = Some(elec_yr.map_or(y, |c| c.max(y)));
                }
                matching.push(permit_value_to_proto(p));
            }
            "plumbing" | "water_heater" => {
                has_plumb = true;
                if let Some(y) = yr {
                    plumb_yr = Some(plumb_yr.map_or(y, |c| c.max(y)));
                }
                matching.push(permit_value_to_proto(p));
            }
            _ => {}
        }
    }
    let note = if matching.is_empty() {
        "No subject HVAC, electrical, or plumbing permits on file.".to_string()
    } else {
        format!("{} subject MEP system permit(s) found.", matching.len())
    };
    MepIntelProto {
        has_hvac_upgrade: has_hvac,
        has_electrical_upgrade: has_elec,
        has_plumbing_upgrade: has_plumb,
        has_ev_charger: has_ev,
        has_heat_pump: has_hp,
        latest_hvac_year: hvac_yr,
        latest_electrical_year: elec_yr,
        latest_plumbing_year: plumb_yr,
        permits: matching,
        note,
    }
}

/// Stage-2 Remodel / Addition / ADU / Seismic Extractor (`DOMAIN_PERMIT_INTEL`).
pub fn extract_remodel_intel(permits: &[Value]) -> RemodelIntelProto {
    let mut matching = Vec::new();
    let (mut has_rem, mut has_adu, mut has_seis, mut has_pool) = (false, false, false, false);
    let mut latest_yr: Option<i32> = None;
    let mut total_val = 0_i64;
    let mut any_val = false;

    for p in permits {
        if !is_subject_permit(p) {
            continue;
        }
        let cat = effective_category(p);
        if !matches!(
            cat,
            "remodel" | "addition" | "remodel_addition" | "adu" | "seismic" | "foundation" | "pool"
        ) {
            continue;
        }
        match cat {
            "adu" => has_adu = true,
            "seismic" | "foundation" => has_seis = true,
            "pool" => has_pool = true,
            _ => has_rem = true,
        }
        if let Some(y) = p.get("year").and_then(|v| v.as_i64()).map(|y| y as i32) {
            latest_yr = Some(latest_yr.map_or(y, |c| c.max(y)));
        }
        if let Some(v) = p.get("jobValueUSD").and_then(|x| x.as_i64()) {
            total_val += v;
            any_val = true;
        }
        matching.push(permit_value_to_proto(p));
    }
    let note = if matching.is_empty() {
        "No remodel, addition, ADU, or seismic permits on file.".to_string()
    } else {
        format!(
            "{} remodel/addition/ADU/structural permit(s) found.",
            matching.len()
        )
    };
    RemodelIntelProto {
        has_remodel_or_addition: has_rem,
        has_adu_permit: has_adu,
        has_seismic_foundation: has_seis,
        has_pool_permit: has_pool,
        latest_remodel_year: latest_yr,
        total_declared_valuation_usd: any_val.then_some(total_val),
        permits: matching,
        note,
    }
}

/// Stage-2 Code Enforcement Extractor (`DOMAIN_PERMIT_INTEL`).
pub fn extract_code_enforcement_intel(cases: &[Value]) -> CodeEnforcementIntelProto {
    let mut open_count = 0_u32;
    let mut closed_count = 0_u32;
    let mut out_cases = Vec::new();
    for c in cases {
        let is_open = c.get("open").and_then(|v| v.as_bool()).unwrap_or_else(|| {
            let st = c
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            st.contains("open") || st.contains("active") || st.contains("violation")
        });
        if is_open {
            open_count += 1;
        } else {
            closed_count += 1;
        }
        let mut rec = permit_value_to_proto(c);
        if rec.category == "other" {
            rec.category = "code_enforcement".to_string();
        }
        out_cases.push(rec);
    }
    let note = if out_cases.is_empty() {
        "No code enforcement cases found for this address.".to_string()
    } else {
        format!("{open_count} open, {closed_count} closed code enforcement case(s).")
    };
    CodeEnforcementIntelProto {
        open_count,
        closed_count,
        has_open_violation: open_count > 0,
        cases: out_cases,
        note,
    }
}

fn permits_from_upstream(upstream: &[ExtensionEnvelopeProto]) -> Vec<Value> {
    let mut out = Vec::new();
    for env in upstream {
        if let Some(Payload::PermitsHistory(hist)) = &env.payload {
            for p in &hist.permits {
                out.push(json!({
                    "permitNumber": p.permit_number,
                    "category": p.category,
                    "rawType": p.raw_type,
                    "description": p.description,
                    "address": p.address,
                    "status": p.status,
                    "finalDate": if p.final_date.is_empty() { Value::Null } else { json!(p.final_date) },
                    "issueDate": if p.issue_date.is_empty() { Value::Null } else { json!(p.issue_date) },
                    "appliedDate": if p.applied_date.is_empty() { Value::Null } else { json!(p.applied_date) },
                    "year": p.year,
                    "finaled": p.finaled,
                    "verified": p.verified,
                    "sourceVerified": p.source_verified,
                    "isParentLot": p.is_parent_lot,
                    "scope": p.scope,
                    "solarKw": p.solar_kw,
                    "solarPanels": p.solar_panels,
                    "jobValueUSD": p.valuation_usd,
                    "contractorName": p.contractor,
                    "sourceName": p.source,
                    "sourceUrl": p.source_url,
                    "apn": p.apn,
                }));
            }
        }
    }
    out
}

fn build_stage1_manifest(
    id: &str,
    name: &str,
    description: &str,
    scope: GeoScopeProto,
    priority: i32,
) -> ExtensionManifestProto {
    ExtensionManifestProto {
        extension_id: id.to_string(),
        name: name.to_string(),
        version: "1.0.0".to_string(),
        description: description.to_string(),
        domain: CapabilityDomain::DomainPermits as i32,
        driver: DriverKind::DriverNativeAdapter as i32,
        geo_scope: Some(scope),
        priority,
        enabled: true,
        builtin: true,
        proto_message_type: "openestate.extension.v1.PermitsHistoryProto".to_string(),
        default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
        cache_ttl_secs: 86400,
        ..Default::default()
    }
}

fn build_stage2_manifest(
    id: &str,
    name: &str,
    description: &str,
    proto_message_type: &str,
    priority: i32,
) -> ExtensionManifestProto {
    ExtensionManifestProto {
        extension_id: id.to_string(),
        name: name.to_string(),
        version: "1.0.0".to_string(),
        description: description.to_string(),
        domain: CapabilityDomain::DomainPermitIntel as i32,
        driver: DriverKind::DriverNativeAdapter as i32,
        geo_scope: Some(GeoScopeProto {
            nationwide: true,
            ..Default::default()
        }),
        priority,
        enabled: true,
        builtin: true,
        proto_message_type: proto_message_type.to_string(),
        default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
        depends_on: vec![
            "campbell_mgo_permits".into(),
            "municipal_open_data_permits".into(),
            "shovels_permits".into(),
        ],
        input_from_domains: vec![CapabilityDomain::DomainPermits as i32],
        upstream_require_mode: "any".to_string(),
        ..Default::default()
    }
}

fn wrap_envelope(
    manifest: &ExtensionManifestProto,
    source_name: &str,
    source_url: &str,
    geo_spec: i32,
    latency_ms: u32,
    payload: Payload,
) -> ExtensionEnvelopeProto {
    ExtensionEnvelopeProto {
        extension_id: manifest.extension_id.clone(),
        domain: manifest.domain,
        priority: manifest.priority,
        geo_specificity: geo_spec,
        attribution: Some(SourceAttributionProto {
            extension_id: manifest.extension_id.clone(),
            source_name: source_name.to_string(),
            source_url: source_url.to_string(),
            provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
            fetched_at_rfc3339: chrono::Utc::now().to_rfc3339(),
            schedule_or_version: None,
            notes: None,
        }),
        latency_ms,
        cached: false,
        payload: Some(payload),
    }
}

/// Stage-1 native adapter querying City of Campbell MGO Building/Planning/Code Enforcement GIS.
pub struct CampbellMgoPermitsAdapter;

impl DataExtensionAdapter for CampbellMgoPermitsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_stage1_manifest(
            "campbell_mgo_permits",
            "City of Campbell MGO Building, Planning & Code Enforcement GIS",
            "Queries Campbell ArcGIS Building (2025/2026..1990), Planning, and Code Enforcement layers.",
            GeoScopeProto {
                states: vec!["CA".into()],
                counties: vec!["06085".into()],
                cities: vec!["campbell".into()],
                zip_codes: vec!["95008".into(), "95009".into(), "95011".into()],
                ..Default::default()
            },
            450,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let is_campbell = query.city.eq_ignore_ascii_case("campbell")
                || matches!(query.zip.as_str(), "95008" | "95009" | "95011");
            if !is_campbell || query.address.trim().is_empty() {
                return Ok(Vec::new());
            }
            let t0 = std::time::Instant::now();
            let mut parent_addrs = Vec::new();
            if let Some(pa) = query.parent_address.as_deref().filter(|s| !s.trim().is_empty()) {
                parent_addrs.push(pa.trim().to_string());
            }
            let bundle = fetch_campbell_mgo_bundle_with_env(
                http,
                Some(env),
                &query.address,
                &parent_addrs,
            )
            .await;
            if bundle.queried_layers.is_empty() && !bundle.code_checked && bundle.permits.is_empty()
            {
                return Err(anyhow!(
                    "CAMPBELL_MGO_UNAVAILABLE: municipal GIS permit layers could not be reached"
                ));
            }
            let mut records: Vec<PermitRecordProto> =
                bundle.permits.iter().map(permit_value_to_proto).collect();
            let open_ce = bundle
                .code_items
                .iter()
                .filter(|c| c.get("open").and_then(|v| v.as_bool()) == Some(true))
                .count() as i32;
            let closed_ce = (bundle.code_items.len() as i32) - open_ce;
            for c in &bundle.code_items {
                let mut rec = permit_value_to_proto(c);
                rec.category = "code_enforcement".to_string();
                records.push(rec);
            }
            let mut city_q = query.clone();
            if city_q.city.trim().is_empty() {
                city_q.city = "Campbell".to_string();
            }
            if city_q.state.trim().is_empty() {
                city_q.state = "CA".to_string();
            }
            persist_city_permit_dataset_page(
                &city_q,
                "City of Campbell MGO GIS",
                "https://www.campbellca.gov/161/Building",
                &records,
                !bundle.queried_layers.is_empty(),
            )
            .await;
            let m = self.manifest();
            Ok(vec![wrap_envelope(
                &m,
                "City of Campbell Building & Planning GIS (MGO)",
                "https://www.campbellca.gov/161/Building",
                500,
                t0.elapsed().as_millis().min(u32::MAX as u128) as u32,
                Payload::PermitsHistory(PermitsHistoryProto {
                    permits: records,
                    sources_checked: vec!["campbell_mgo_gis".into()],
                    portal_name: "City of Campbell MGO GIS".into(),
                    portal_url: "https://www.campbellca.gov/161/Building".into(),
                    coverage_complete: !bundle.queried_layers.is_empty(),
                    queried_layers: bundle.queried_layers,
                    open_code_enforcement_count: Some(open_ce),
                    closed_code_enforcement_count: Some(closed_ce),
                    notes: String::new(),
                }),
            )])
        })
    }
}

/// Stage-1 native adapter querying municipal Socrata/ArcGIS open-data permit portals.
pub struct MunicipalOpenDataPermitsAdapter;

impl DataExtensionAdapter for MunicipalOpenDataPermitsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_stage1_manifest(
            "municipal_open_data_permits",
            "Municipal Socrata & Open-Data Building Permits",
            "Queries municipal open-data building permit & code-violation endpoints across supported US cities.",
            GeoScopeProto {
                nationwide: true,
                ..Default::default()
            },
            350,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let t0 = std::time::Instant::now();
            let geo = json!({
                "displayAddress": query.address,
                "matchedAddress": query.address,
                "city": query.city,
                "place": query.city,
                "state": query.state,
                "zip": query.zip,
                "countyFips": query.county_fips,
                "lat": query.lat,
                "lon": query.lon,
            });
            let facts = json!({ "address": query.address, "streetAddress": query.street });
            let (raw_permits, raw_violations, sources, portal_name) =
                fetch_municipal_open_data_stage1(http, env, &geo, &facts).await;
            if env.get("MUNICIPAL_OPENDATA_BASE_URL").is_some()
                && portal_name.is_some()
                && raw_permits.is_empty()
                && raw_violations.is_empty()
                && sources.is_empty()
            {
                return Err(anyhow!(
                    "MUNICIPAL_OPENDATA_UNAVAILABLE: municipal open-data endpoint returned no usable response"
                ));
            }
            if raw_permits.is_empty() && raw_violations.is_empty() {
                return Ok(Vec::new());
            }
            let now_year = chrono::Utc::now().year();
            let mut records: Vec<PermitRecordProto> = raw_permits
                .iter()
                .filter_map(|p| normalize_permit_record(p, "Municipal Open Data", None, now_year))
                .map(|v| permit_value_to_proto(&v))
                .collect();
            for v in &raw_violations {
                let mut rec = permit_value_to_proto(v);
                rec.category = "code_enforcement".to_string();
                records.push(rec);
            }
            let pname = portal_name.unwrap_or_else(|| "Municipal Open Data Portal".to_string());
            let m = self.manifest();
            Ok(vec![wrap_envelope(
                &m,
                &pname,
                "",
                400,
                t0.elapsed().as_millis().min(u32::MAX as u128) as u32,
                Payload::PermitsHistory(PermitsHistoryProto {
                    permits: records,
                    sources_checked: vec![pname.clone()],
                    portal_name: pname.clone(),
                    portal_url: String::new(),
                    coverage_complete: true,
                    queried_layers: Vec::new(),
                    open_code_enforcement_count: None,
                    closed_code_enforcement_count: None,
                    notes: String::new(),
                }),
            )])
        })
    }
}

/// Stage-1 native adapter querying Shovels v2 (`addresses/search` -> `geo_id` -> `permits/search`).
pub struct ShovelsPermitsAdapter;

impl DataExtensionAdapter for ShovelsPermitsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        let mut m = build_stage1_manifest(
            "shovels_permits",
            "Shovels v2 Nationwide Building Permits",
            "Resolves address geo_id via Shovels v2 and fetches standardized permit & contractor history.",
            GeoScopeProto {
                nationwide: true,
                ..Default::default()
            },
            300,
        );
        m.auth = Some(AuthSpecProto {
            auth_type: "api_key_header".into(),
            env_var: "SHOVELS_API_KEY".into(),
            header_name: "X-API-Key".into(),
            optional_key: true,
            ..Default::default()
        });
        m
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if env.get("SHOVELS_API_KEY").is_none() {
                return Err(anyhow!("AUTH_MISSING: SHOVELS_API_KEY is not configured"));
            }
            let t0 = std::time::Instant::now();
            let (permits, ok, warn_opt) =
                fetch_shovels_v2_permits(http, env, &query.address, None).await;
            if !ok {
                return Err(anyhow!(
                    "SHOVELS_UNAVAILABLE: {}",
                    warn_opt.unwrap_or_else(|| "Shovels v2 permit endpoint unavailable".into())
                ));
            }
            if permits.is_empty() {
                return Ok(Vec::new());
            }
            let records: Vec<PermitRecordProto> =
                permits.iter().map(permit_value_to_proto).collect();
            let m = self.manifest();
            Ok(vec![wrap_envelope(
                &m,
                "Shovels v2 Building Permits API",
                "https://www.shovels.ai/",
                100,
                t0.elapsed().as_millis().min(u32::MAX as u128) as u32,
                Payload::PermitsHistory(PermitsHistoryProto {
                    permits: records,
                    sources_checked: vec!["shovels_v2".into()],
                    portal_name: "Shovels v2 API".into(),
                    portal_url: "https://www.shovels.ai/".into(),
                    coverage_complete: true,
                    queried_layers: Vec::new(),
                    open_code_enforcement_count: None,
                    closed_code_enforcement_count: None,
                    notes: String::new(),
                }),
            )])
        })
    }
}

/// Stage-2 Roof Lifecycle Extractor (`depends_on` Stage-1 permit fetchers).
pub struct PermitRoofExtractorAdapter;

impl DataExtensionAdapter for PermitRoofExtractorAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_stage2_manifest(
            "permit_roof_extractor",
            "Permit Roof Lifecycle Extractor",
            "Extracts verified reroof vs new-construction vs unverified roof lifecycle strictly excluding Solar PV.",
            "openestate.extension.v1.RoofIntelProto",
            500,
        )
    }

    fn execute_sync(&self, query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        let upstream_permits = permits_from_upstream(&query.upstream_envelopes);
        let yb = query.year_built.map(|y| y as i32);
        let roof = extract_roof_lifecycle(&upstream_permits, yb, None, None, chrono::Utc::now().year());
        let m = self.manifest();
        Ok(vec![wrap_envelope(
            &m,
            "Linked Stage-2 Permit Roof Lifecycle Extractor",
            "",
            500,
            0,
            Payload::RoofIntel(roof),
        )])
    }
}

/// Stage-2 Solar PV Extractor (`depends_on` Stage-1 permit fetchers).
pub struct PermitSolarExtractorAdapter;

impl DataExtensionAdapter for PermitSolarExtractorAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_stage2_manifest(
            "permit_solar_extractor",
            "Permit Solar PV Extractor",
            "Extracts rooftop solar PV kW & panel counts from Stage-1 building permits.",
            "openestate.extension.v1.SolarIntelProto",
            480,
        )
    }

    fn execute_sync(&self, query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        let upstream_permits = permits_from_upstream(&query.upstream_envelopes);
        let solar = extract_solar_pv(&upstream_permits);
        let m = self.manifest();
        Ok(vec![wrap_envelope(
            &m,
            "Linked Stage-2 Solar PV Permit Extractor",
            "",
            500,
            0,
            Payload::SolarIntel(solar),
        )])
    }
}

/// Stage-2 MEP Systems Extractor (`depends_on` Stage-1 permit fetchers).
pub struct PermitMepExtractorAdapter;

impl DataExtensionAdapter for PermitMepExtractorAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_stage2_manifest(
            "permit_mep_extractor",
            "Permit HVAC, Electrical & Plumbing Extractor",
            "Extracts HVAC, heat pump, EV charger, electrical panel, and plumbing upgrade permits.",
            "openestate.extension.v1.MepIntelProto",
            460,
        )
    }

    fn execute_sync(&self, query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        let upstream_permits = permits_from_upstream(&query.upstream_envelopes);
        let mep = extract_mep_intel(&upstream_permits);
        let m = self.manifest();
        Ok(vec![wrap_envelope(
            &m,
            "Linked Stage-2 MEP Permit Extractor",
            "",
            500,
            0,
            Payload::MepIntel(mep),
        )])
    }
}

/// Stage-2 Remodel / Addition / ADU / Seismic Extractor (`depends_on` Stage-1 permit fetchers).
pub struct PermitRemodelExtractorAdapter;

impl DataExtensionAdapter for PermitRemodelExtractorAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_stage2_manifest(
            "permit_remodel_extractor",
            "Permit Remodel, ADU & Structural Extractor",
            "Extracts remodel, addition, ADU, seismic/foundation, and pool permits.",
            "openestate.extension.v1.RemodelIntelProto",
            440,
        )
    }

    fn execute_sync(&self, query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        let upstream_permits = permits_from_upstream(&query.upstream_envelopes);
        let remodel = extract_remodel_intel(&upstream_permits);
        let m = self.manifest();
        Ok(vec![wrap_envelope(
            &m,
            "Linked Stage-2 Remodel & ADU Extractor",
            "",
            500,
            0,
            Payload::RemodelIntel(remodel),
        )])
    }
}

/// Stage-2 Code Enforcement Extractor (`depends_on` Stage-1 permit fetchers).
pub struct PermitCodeEnforcementExtractorAdapter;

impl DataExtensionAdapter for PermitCodeEnforcementExtractorAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_stage2_manifest(
            "permit_code_enforcement_extractor",
            "Permit Code Enforcement Extractor",
            "Extracts open & closed municipal code-enforcement cases from Stage-1 permit feeds.",
            "openestate.extension.v1.CodeEnforcementIntelProto",
            420,
        )
    }

    fn execute_sync(&self, query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        let upstream_cases: Vec<Value> = permits_from_upstream(&query.upstream_envelopes)
            .into_iter()
            .filter(|p| effective_category(p) == "code_enforcement")
            .collect();
        let ce = extract_code_enforcement_intel(&upstream_cases);
        let m = self.manifest();
        Ok(vec![wrap_envelope(
            &m,
            "Linked Stage-2 Code Enforcement Extractor",
            "",
            500,
            0,
            Payload::CodeEnforcementIntel(ce),
        )])
    }
}

/// Persists a resolved municipal building permit page as a typed
/// `openestate.shared.v1.CityPermitDatasetPageProto` record in `shared_data_records`
/// (`dataset_ids::CITY_PERMIT_DATASET`, `ShareScope::City`, 24h TTL) via an explicit [`DbStore`].
pub(crate) async fn persist_city_permit_dataset_page_with_db(
    db: &DbStore,
    query: &ExtensionQueryProto,
    portal_name: &str,
    portal_url: &str,
    page_cursor: &str,
    permits: &[PermitRecordProto],
    complete: bool,
) -> Result<()> {
    if permits.is_empty() {
        return Ok(());
    }
    let Some(scope_key) = derive_scope_key(ShareScope::City, query) else {
        return Ok(());
    };
    let page = shared_v1::CityPermitDatasetPageProto {
        city: query.city.trim().to_string(),
        state: query.state.trim().to_ascii_uppercase(),
        portal_name: portal_name.to_string(),
        portal_url: portal_url.to_string(),
        page_cursor: page_cursor.to_string(),
        permits: permits.to_vec(),
        complete,
        fetched_at_rfc3339: chrono::Utc::now().to_rfc3339(),
    };
    let geo = derive_geo_index_columns(query);
    let prov = json!({
        "portalName": portal_name,
        "portalUrl": portal_url,
        "scopeKey": scope_key,
        "permitCount": permits.len(),
    })
    .to_string();
    put_shared_dataset_proto(
        Some(SharedExtensionCache::global()),
        db,
        dataset_ids::CITY_PERMIT_DATASET,
        ShareScope::City,
        &scope_key,
        page_cursor,
        geo,
        &page,
        None,
        Some(prov),
        None,
        86_400,
    )
    .await?;
    Ok(())
}

/// Computes the canonical `CityPermitDatasetPageProto` page cursor key for a parcel/street slice.
pub(crate) fn city_permit_page_cursor(portal_url: &str, query: &ExtensionQueryProto) -> String {
    compute_sha256_hex(
        format!(
            "{}|{}|{}",
            portal_url.trim(),
            query.street.trim().to_ascii_uppercase(),
            query.address.trim().to_ascii_uppercase()
        )
        .as_bytes(),
    )
}

/// Persists resolved city permit pages to `shared_data_records` when an active [`DbStore`] is bound.
pub(crate) async fn persist_city_permit_dataset_page(
    query: &ExtensionQueryProto,
    portal_name: &str,
    portal_url: &str,
    permits: &[PermitRecordProto],
    complete: bool,
) {
    if permits.is_empty() {
        return;
    }
    let Some(db) = ExtensionHub::global().active_db() else {
        return;
    };
    let cursor = city_permit_page_cursor(portal_url, query);
    if let Err(err) = persist_city_permit_dataset_page_with_db(
        &db,
        query,
        portal_name,
        portal_url,
        &cursor,
        permits,
        complete,
    )
    .await
    {
        tracing::warn!("permit_pipeline: failed writing CITY_PERMIT_DATASET ({err})");
    }
}

/// Reads back a typed `CityPermitDatasetPageProto` record from `shared_data_records`.
#[allow(dead_code)]
pub(crate) async fn read_city_permit_dataset_page(
    db: &DbStore,
    query: &ExtensionQueryProto,
    page_cursor: &str,
) -> Result<Option<shared_v1::CityPermitDatasetPageProto>> {
    let Some(scope_key) = derive_scope_key(ShareScope::City, query) else {
        return Ok(None);
    };
    let hit = get_shared_dataset_proto::<shared_v1::CityPermitDatasetPageProto>(
        Some(SharedExtensionCache::global()),
        db,
        dataset_ids::CITY_PERMIT_DATASET,
        ShareScope::City,
        &scope_key,
        page_cursor,
    )
    .await?;
    Ok(hit.map(|(_, page)| page))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campbell_address_matching_handles_unit_ranges_without_partial_prefix_false_hits() {
        assert!(campbell_address_matches(
            "126 SYNTHPLATFORM WAY",
            "126",
            "SYNTHPLATFORM"
        ));
        assert!(campbell_address_matches(
            "114 SYNTHPLATFORM WAY -126 BLDG 3",
            "126",
            "SYNTHPLATFORM"
        ));
        assert!(campbell_address_matches(
            "114-128 SYNTHPLATFORM WAY",
            "126",
            "SYNTHPLATFORM"
        ));
        assert!(!campbell_address_matches(
            "1260 SYNTHPLATFORM WAY",
            "126",
            "SYNTHPLATFORM"
        ));
        assert!(!campbell_address_matches(
            "114 SYNTHPLATFORM WAY -116 BLDG 3",
            "126",
            "SYNTHPLATFORM"
        ));
        assert!(!campbell_address_matches(
            "120 SYNTHPLATFORM WAY BLDG 126",
            "126",
            "SYNTHPLATFORM"
        ));
    }

    #[test]
    fn stage2_extractors_exclude_solar_pv_from_reroof_and_never_fabricate_1985() {
        let permits = vec![
            json!({
                "permitNumber": "BLD-2026-694",
                "category": "reroof",
                "rawType": "Residential - Photovoltaic (Automated)",
                "description": "Roof Mounted PV System 2.870 kW (7 Panels)",
                "status": "Permit Issued",
                "issueDate": "2026-02-27",
                "year": 2026,
                "finaled": false,
                "verified": false,
                "sourceVerified": true,
                "isParentLot": false,
                "solarKw": 2.87,
                "solarPanels": 7,
            }),
            json!({
                "permitNumber": "BLD-2026-52",
                "category": "new_construction",
                "rawType": "New Townhomes",
                "description": "4-Unit Townhome Building",
                "status": "Permit Issued",
                "issueDate": "2026-01-12",
                "year": 2026,
                "finaled": false,
                "verified": false,
                "sourceVerified": true,
                "isParentLot": false,
            }),
        ];

        let roof = extract_roof_lifecycle(&permits, None, None, None, 2026);
        assert_eq!(roof.verification_status, "new_construction");
        assert!(!roof.replacement_date_verified);
        assert!(!roof.reroof_verified);
        assert_eq!(roof.verified_reroof_year, None);
        assert_eq!(roof.new_construction_permit_year, Some(2026));
        assert_eq!(roof.effective_year, Some(2026));
        assert_eq!(roof.effective_year_source, "new_construction_permit");
        assert_eq!(roof.material, None);
        assert_eq!(roof.material_source, "unknown");

        let empty_roof = extract_roof_lifecycle(&[], None, None, None, 2026);
        assert_eq!(empty_roof.verification_status, "unverified");
        assert_eq!(empty_roof.effective_year, None);
        assert_eq!(empty_roof.effective_year_source, "unknown");

        let solar = extract_solar_pv(&permits);
        assert!(solar.has_solar);
        assert_eq!(solar.total_kw, Some(2.87));
        assert_eq!(solar.panel_count, Some(7));
        assert_eq!(solar.permits.len(), 1);
        assert_eq!(solar.permits[0].category, "solar");
    }

    #[test]
    fn stage1_to_stage2_dag_envelopes_wire_cleanly() {
        let stage1_env = ExtensionEnvelopeProto {
            extension_id: "campbell_mgo_permits".into(),
            domain: CapabilityDomain::DomainPermits as i32,
            priority: 450,
            geo_specificity: 500,
            payload: Some(Payload::PermitsHistory(PermitsHistoryProto {
                permits: vec![
                    PermitRecordProto {
                        permit_number: "BLD-2026-694".into(),
                        category: "solar".into(),
                        raw_type: "Residential - Photovoltaic (Automated)".into(),
                        description: "Roof Mounted PV System 2.870 kW (7 Panels)".into(),
                        status: "Permit Issued".into(),
                        year: Some(2026),
                        finaled: false,
                        verified: false,
                        source_verified: true,
                        is_parent_lot: false,
                        scope: "subject".into(),
                        solar_kw: Some(2.87),
                        solar_panels: Some(7),
                        ..Default::default()
                    },
                    PermitRecordProto {
                        permit_number: "BLD-2025-87".into(),
                        category: "reroof".into(),
                        raw_type: "Re-Roof".into(),
                        description: "Subdivision parent lot test".into(),
                        status: "Finaled".into(),
                        year: Some(2025),
                        finaled: true,
                        verified: false,
                        source_verified: true,
                        is_parent_lot: true,
                        scope: "parent_lot".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })),
            ..Default::default()
        };

        let query = ExtensionQueryProto {
            address: "126 Synthplatform Way, Campbell, CA 95008".into(),
            ..Default::default()
        };
        let roof_adapter = PermitRoofExtractorAdapter;
        let roof_envs = roof_adapter
            .execute_with_upstream_sync(&query, &[stage1_env.clone()])
            .unwrap();
        let Some(Payload::RoofIntel(roof)) = &roof_envs[0].payload else {
            panic!("expected RoofIntel payload");
        };
        // Parent lot reroof & Solar PV must BOTH be excluded from subject roof verification!
        assert!(!roof.reroof_verified);
        assert_eq!(roof.verified_reroof_year, None);
        assert_eq!(roof.effective_year, None);

        let solar_adapter = PermitSolarExtractorAdapter;
        let solar_envs = solar_adapter
            .execute_with_upstream_sync(&query, &[stage1_env])
            .unwrap();
        let Some(Payload::SolarIntel(solar)) = &solar_envs[0].payload else {
            panic!("expected SolarIntel payload");
        };
        assert!(solar.has_solar);
        assert_eq!(solar.total_kw, Some(2.87));
        assert_eq!(solar.panel_count, Some(7));
    }

    #[tokio::test]
    async fn city_permit_dataset_persists_and_reads_via_shared_data_records() {
        let db = DbStore::connect_url(":memory:", Some("sqlite"))
            .await
            .expect("sqlite memory db");
        crate::extensions::shared_cache::seed_builtin_shared_datasets(&db)
            .await
            .expect("seed builtin shared datasets");

        let query = ExtensionQueryProto {
            address: "126 Synthplatform Way, Campbell, CA 95008".into(),
            street: "126 Synthplatform Way".into(),
            city: "Campbell".into(),
            state: "CA".into(),
            zip: "95008".into(),
            county_fips: "06085".into(),
            lat: Some(37.2872),
            lon: Some(-121.9500),
            ..Default::default()
        };
        let portal_url = "https://www.campbellca.gov/161/Building";
        let cursor = city_permit_page_cursor(portal_url, &query);
        let permits = vec![PermitRecordProto {
            permit_number: "BLD-SYNTH-2026-1".into(),
            category: "remodel_addition".into(),
            raw_type: "Tenant Improvement".into(),
            description: "Interior kitchen remodel".into(),
            status: "Finaled".into(),
            year: Some(2026),
            finaled: true,
            verified: true,
            source_verified: true,
            ..Default::default()
        }];

        persist_city_permit_dataset_page_with_db(
            &db,
            &query,
            "City of Campbell MGO GIS",
            portal_url,
            &cursor,
            &permits,
            true,
        )
        .await
        .expect("persist CITY_PERMIT_DATASET");

        let loaded = read_city_permit_dataset_page(&db, &query, &cursor)
            .await
            .expect("read CITY_PERMIT_DATASET")
            .expect("persisted CityPermitDatasetPageProto");
        assert_eq!(loaded.city, "Campbell");
        assert_eq!(loaded.state, "CA");
        assert_eq!(loaded.portal_name, "City of Campbell MGO GIS");
        assert!(loaded.complete);
        assert_eq!(loaded.permits.len(), 1);
        assert_eq!(loaded.permits[0].permit_number, "BLD-SYNTH-2026-1");
    }

    #[test]
    fn campbell_address_where_uses_unquoted_smallint_house_number() {
        let q = build_campbell_address_where("126", "synthcorridor").expect("where clause");
        assert!(q.starts_with("HouseNumber=126 AND "), "got={q}");
        assert!(!q.contains("HouseNumber='"), "got={q}");

        let overflow = build_campbell_address_where("45000", "synthcorridor").expect("overflow");
        assert!(
            overflow.starts_with("UPPER(FullAddress) LIKE '45000 %' AND "),
            "got={overflow}"
        );
    }
}
