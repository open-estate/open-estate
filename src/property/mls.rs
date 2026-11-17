// MLS & property-data connectors (RESO Web API 2.0, SimplyRETS, ATTOM) + County Assessor GIS merge.
// Zero owner PII: never requests or emits `Owner*`, `ASSESSEE`, or `Mailing*` fields.

use crate::property::public_records::{fetch_public_parcel_records, NeighborParcel};
use crate::shared::{haversine_mi, now_iso, round_n, EnvConfig, HttpClient, SourceItem};
use base64::{engine::general_purpose::STANDARD as BASE64_STD, Engine as _};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use url::Url;

const ATTOM_BASE: &str = crate::registry::endpoints::ATTOM_BASE_URL;
const SIMPLYRETS_BASE: &str = crate::registry::endpoints::SIMPLYRETS_BASE_URL;

static RE_STREET_SPLIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\d+[A-Za-z]?)\s+(.+)$").unwrap());
static RE_ST_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:St|Street|Ave|Av|Avenue|Rd|Road|Dr|Drive|Ln|Lane|Ct|Court|Blvd|Boulevard|Way|Wy|Pl|Place|Cir|Circle|Ter|Terrace|Pkwy|Parkway|Hwy|Highway)\b\.?$").unwrap()
});
static RE_CARDINAL_WORD: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(north|south|east|west)\b").unwrap());
static RE_DISTRESS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)foreclos|reo|bank|short\s*sale|probate|auction|hud|trustee|sheriff").unwrap()
});
static RE_REDFIN_HOME_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"/home/(\d+)").unwrap());

#[derive(Debug, Clone, Default)]
pub struct MlsAndRecordsOutput {
    pub facts: Map<String, Value>,
    pub avms: Vec<Value>,
    pub comps: Vec<Value>,
    pub rental_comps: Vec<Value>,
    pub nearby_listings: Vec<Value>,
    pub nearby_pending: Vec<Value>,
    pub nearby_rentals: Vec<Value>,
    pub nearby_parcels: Vec<NeighborParcel>,
    pub rent_estimate: Option<i64>,
    pub rent_range: Option<Value>,
    pub sources: Vec<SourceItem>,
    pub warnings: Vec<String>,
    pub connectors_used: Vec<String>,
}

pub fn connector_status(env: &EnvConfig) -> Vec<Value> {
    vec![
        json!({
            "id": "reso",
            "name": "RESO Web API 2.0 (MLS IDX)",
            "active": env.get("RESO_API_URL").is_some() && env.get("RESO_BEARER_TOKEN").is_some(),
            "envVars": ["RESO_API_URL", "RESO_BEARER_TOKEN"],
        }),
        json!({
            "id": "simplyrets",
            "name": "SimplyRETS MLS IDX",
            "active": env.get("SIMPLYRETS_AUTH").is_some(),
            "envVars": ["SIMPLYRETS_AUTH"],
        }),
        json!({
            "id": "attom",
            "name": "ATTOM Property & AVM",
            "active": env.get("ATTOM_API_KEY").is_some(),
            "envVars": ["ATTOM_API_KEY"],
        }),
        // [feat/insurance-risk]
        json!({
            "id": "shovels",
            "name": "Shovels 50-State Building Permits & Contractor API",
            "active": env.shovels_configured(),
            "envVars": ["SHOVELS_API_KEY"],
        }),
        json!({
            "id": "zoneomics",
            "name": "Zoneomics Nationwide Zoning, FAR & ADU/STR API",
            "active": env.zoneomics_configured(),
            "envVars": ["ZONEOMICS_API_KEY"],
        }),
        // [/feat/insurance-risk]
        json!({
            "id": "county_gis",
            "name": "County Assessor & Municipal GIS Open Data",
            "active": true,
        }),
        json!({
            "id": "greatschools",
            "name": "GreatSchools Ratings API",
            "active": env.get("GREATSCHOOLS_API_KEY").is_some(),
            "envVars": ["GREATSCHOOLS_API_KEY"],
        }),
        json!({
            "id": "schooldigger",
            "name": "SchoolDigger Rankings API",
            "active": env.get("SCHOOLDIGGER_APP_ID").is_some() && env.get("SCHOOLDIGGER_APP_KEY").is_some(),
            "envVars": ["SCHOOLDIGGER_APP_ID", "SCHOOLDIGGER_APP_KEY"],
        }),
        json!({
            "id": "google_places",
            "name": "Google Places Address Autocomplete",
            "active": env.get("GOOGLE_MAPS_API_KEY").is_some(),
            "envVars": ["GOOGLE_MAPS_API_KEY"],
        }),
        json!({
            "id": "ai_analyst",
            "name": "Open Estate Residential Property AI (Purpose-Locked)",
            "active": env.llm_configured(),
        }),
    ]
}

pub fn classify_asset_class(raw: Option<&str>) -> Option<String> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    let low = s.to_lowercase();
    if low.contains("condo")
        || low.contains("co-op")
        || low.contains("coop")
        || low.contains("apartment")
        || low.contains("flat")
        || low.contains("high-rise")
        || low.contains("mid-rise")
        || low.contains("loft")
    {
        return Some("Condo / Co-op".to_string());
    }
    if low.contains("townhouse")
        || low.contains("townhome")
        || low.contains("row")
        || low.contains("attached")
        || low.contains("pud")
        || low.contains("patio")
        || low.contains("duet")
    {
        return Some("Townhome / Attached".to_string());
    }
    if low.contains("multi")
        || low.contains("duplex")
        || low.contains("triplex")
        || low.contains("fourplex")
        || low.contains("quad")
        || low.contains("2-4")
        || low.contains("income")
    {
        return Some("2–4 Unit Multifamily".to_string());
    }
    if low.contains("commercial")
        || low.contains("retail")
        || low.contains("office")
        || low.contains("industrial")
        || low.contains("mixed")
    {
        return Some("Commercial / Mixed-Use".to_string());
    }
    if low.contains("manufactured")
        || low.contains("mobile")
        || low.contains("land")
        || low.contains("lot")
    {
        return Some("Manufactured / Land".to_string());
    }
    if low.contains("single")
        || low.contains("sfr")
        || low.contains("detached")
        || low.contains("residential")
    {
        return Some("Single-Family Detached".to_string());
    }
    Some(s.to_string())
}

pub fn merge_tax_histories(primary: &[Value], secondary: &[Value]) -> Vec<Value> {
    let mut by_year: BTreeMap<i64, Map<String, Value>> = BTreeMap::new();
    for row in secondary {
        if let Some(yr) = row.get("year").and_then(|v| v.as_i64()) {
            if let Some(obj) = row.as_object() {
                let mut m = obj.clone();
                m.insert("year".into(), json!(yr));
                by_year.insert(yr, m);
            }
        }
    }
    for row in primary {
        if let Some(yr) = row.get("year").and_then(|v| v.as_i64()) {
            if let Some(obj) = row.as_object() {
                let entry = by_year.entry(yr).or_insert_with(|| {
                    let mut m = Map::new();
                    m.insert("year".into(), json!(yr));
                    m
                });
                for (k, v) in obj {
                    if !v.is_null() {
                        entry.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
    by_year.into_values().map(Value::Object).collect()
}

struct StreetParts {
    street: String,
    street_number: String,
    street_name: String,
    city: String,
    state: String,
    zip: String,
}

fn split_street_parts(address: &str, geo: &Value) -> StreetParts {
    let street = address.split(',').next().unwrap_or("").trim().to_string();
    let (street_number, street_name) = if let Some(caps) = RE_STREET_SPLIT.captures(&street) {
        let sn = RE_ST_SUFFIX.replace(&caps[2], "").trim().to_string();
        (caps[1].to_string(), sn)
    } else {
        (String::new(), street.clone())
    };
    StreetParts {
        street,
        street_number,
        street_name,
        city: geo
            .get("city")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        state: geo
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        zip: geo
            .get("zip")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn norm_monthly_hoa(fee: Option<f64>, freq: Option<&str>) -> Option<i64> {
    let n = fee.filter(|x| x.is_finite() && *x > 0.0)?;
    let f = freq.unwrap_or("monthly").to_lowercase();
    if f.contains("quarter") {
        Some((n / 3.0).round() as i64)
    } else if f.contains("annual") || f.contains("year") {
        Some((n / 12.0).round() as i64)
    } else if f.contains("semi") {
        Some((n / 6.0).round() as i64)
    } else {
        Some(n.round() as i64)
    }
}

fn to_string_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split([',', ';'])
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

// [feat/report-ux-likes] Preserve real portal engagement fields when present in MLS payloads (never fabricates).
fn attach_optional_engagement_fields(obj: &mut Map<String, Value>, raw: &Value) {
    if let Some(fav) = raw
        .get("favoriteCount")
        .or_else(|| raw.get("FavoriteCount"))
        .or_else(|| raw.get("favorites"))
        .or_else(|| raw.get("saves"))
        .filter(|v| !v.is_null())
    {
        obj.insert("favoriteCount".into(), fav.clone());
    }
    if let Some(views) = raw
        .get("pageViewCount")
        .or_else(|| raw.get("PageViewCount"))
        .or_else(|| raw.get("viewsCount"))
        .or_else(|| raw.get("views"))
        .or_else(|| raw.get("ViewCount"))
        .filter(|v| !v.is_null())
    {
        obj.insert("pageViewCount".into(), views.clone());
    }
    if let Some(hot) = raw
        .get("hotHome")
        .or_else(|| raw.get("HotHome"))
        .or_else(|| raw.get("hotTag"))
        .filter(|v| !v.is_null())
    {
        obj.insert("hotHome".into(), hot.clone());
    }
    if let Some(url) = raw
        .get("url")
        .or_else(|| raw.get("ListingURL"))
        .or_else(|| raw.get("ListingUrl"))
        .or_else(|| raw.get("listingUrl"))
        .filter(|v| !v.is_null())
    {
        obj.insert("url".into(), url.clone());
    }
    if let Some(src) = raw
        .get("source")
        .or_else(|| raw.get("OriginatingSystemName"))
        .filter(|v| !v.is_null())
    {
        obj.insert("source".into(), src.clone());
    }
    // Virtual / 3D tour link published with the listing (RESO `VirtualTourURLUnbranded`,
    // SimplyRETS `virtualTourUrl`). The report's 3D section embeds it through the tour host's
    // own player; nothing is copied.
    if let Some(tour) = raw
        .get("VirtualTourURLUnbranded")
        .or_else(|| raw.get("VirtualTourURLBranded"))
        .or_else(|| raw.get("virtualTourUrl"))
        .or_else(|| raw.get("VirtualTourURL"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
    {
        obj.insert("virtualTourUrl".into(), Value::String(tour.to_string()));
    }
}

async fn fetch_reso(
    http: &HttpClient,
    address: &str,
    geo: &Value,
    api_url: &str,
    token: &str,
) -> Result<MlsAndRecordsOutput, String> {
    let base = api_url.trim_end_matches('/');
    let parts = split_street_parts(address, geo);
    let esc = |s: &str| s.replace('\'', "''");
    let mut clauses = Vec::new();
    if !parts.street_number.is_empty() {
        clauses.push(format!("StreetNumber eq '{}'", esc(&parts.street_number)));
    }
    if !parts.street_name.is_empty() {
        clauses.push(format!(
            "contains(tolower(StreetName), '{}')",
            esc(&parts.street_name.to_lowercase())
        ));
    }
    if !parts.zip.is_empty() {
        clauses.push(format!("PostalCode eq '{}'", esc(&parts.zip)));
    }
    let filter = if clauses.len() >= 2 {
        clauses.join(" and ")
    } else {
        format!(
            "contains(tolower(UnparsedAddress), '{}')",
            esc(&parts.street.to_lowercase())
        )
    };

    let auth_hdr = format!("Bearer {token}");
    let headers = [
        ("Authorization", auth_hdr.as_str()),
        ("Accept", "application/json"),
    ];

    let mut u1 = Url::parse(&format!("{base}/Property")).map_err(|e| e.to_string())?;
    u1.query_pairs_mut()
        .append_pair("$filter", &filter)
        .append_pair("$top", "10")
        .append_pair("$expand", "Media");

    let j = match http
        .get_json_opts(u1.as_str(), 18_000, 1, 3600, Some(&headers))
        .await
    {
        Ok(v) => v,
        Err(_) => {
            let mut u2 = Url::parse(&format!("{base}/Property")).map_err(|e| e.to_string())?;
            u2.query_pairs_mut()
                .append_pair("$filter", &filter)
                .append_pair("$top", "10");
            http.get_json_opts(u2.as_str(), 18_000, 1, 3600, Some(&headers))
                .await
                .map_err(|e| e.to_string())?
        }
    };

    let mut rows = j
        .get("value")
        .or_else(|| j.get("bundle"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return Ok(MlsAndRecordsOutput {
            warnings: vec!["RESO MLS query returned 0 matching listings.".into()],
            ..Default::default()
        });
    }

    let rank = |s: &str| {
        let low = s.to_lowercase();
        if low.contains("active") {
            3
        } else if low.contains("pending") || low.contains("contract") {
            2
        } else if low.contains("closed") || low.contains("sold") {
            1
        } else {
            0
        }
    };
    rows.sort_by(|a, b| {
        let ra = rank(
            a.get("StandardStatus")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let rb = rank(
            b.get("StandardStatus")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        rb.cmp(&ra).then_with(|| {
            let ta = a
                .get("ModificationTimestamp")
                .or_else(|| a.get("CloseDate"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let tb = b
                .get("ModificationTimestamp")
                .or_else(|| b.get("CloseDate"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            tb.cmp(ta)
        })
    });

    let p = &rows[0];
    let mut photos: Vec<String> = Vec::new();
    let mut media_docs: Vec<Value> = Vec::new();
    if let Some(arr) = p.get("Media").and_then(|m| m.as_array()) {
        for m in arr {
            let Some(url_str) = m
                .get("MediaURL")
                .or_else(|| m.get("MediaUrl"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let cat_lc = m
                .get("MediaCategory")
                .or_else(|| m.get("MediaType"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mime_lc = m
                .get("MimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let desc = m
                .get("ShortDescription")
                .or_else(|| m.get("LongDescription"))
                .or_else(|| m.get("MediaObjectID"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let url_lc = url_str.to_ascii_lowercase();
            let is_doc = cat_lc.contains("document")
                || cat_lc.contains("disclosure")
                || cat_lc.contains("supplement")
                || cat_lc.contains("flyer")
                || mime_lc.contains("pdf")
                || url_lc.ends_with(".pdf")
                || url_lc.contains(".pdf?")
                || url_lc.contains("inspection")
                || url_lc.contains("disclosure")
                || url_lc.contains("termite")
                || url_lc.contains("prelim");
            if is_doc {
                if media_docs.len() < 10 {
                    media_docs.push(json!({
                        "title": desc.unwrap_or("MLS Property Document"),
                        "category": if cat_lc.is_empty() { "Document" } else { m.get("MediaCategory").and_then(|v| v.as_str()).unwrap_or("Document") },
                        "url": url_str,
                    }));
                }
            } else if photos.len() < 12 {
                photos.push(url_str.to_string());
            }
        }
    }

    let map_listing = |row: &Value, inc_photos: bool| -> Value {
        let mut obj = json!({
            "provider": "RESO Web API (MLS)",
            "mlsNumber": row.get("ListingId").or_else(|| row.get("ListingKey")),
            "status": row.get("StandardStatus").or_else(|| row.get("MlsStatus")),
            "listPrice": row.get("ListPrice"),
            "originalListPrice": row.get("OriginalListPrice"),
            "closePrice": row.get("ClosePrice"),
            "daysOnMarket": row.get("DaysOnMarket").or_else(|| row.get("CumulativeDaysOnMarket")),
            "listDate": row.get("ListingContractDate").or_else(|| row.get("OnMarketDate")).and_then(|v| v.as_str()).map(|s| &s[..s.len().min(10)]),
            "closeDate": row.get("CloseDate").and_then(|v| v.as_str()).map(|s| &s[..s.len().min(10)]),
            "agentName": row.get("ListAgentFullName"),
            "agentLicense": row.get("ListAgentStateLicense"),
            "officeName": row.get("ListOfficeName"),
            "buyerAgentName": row.get("BuyerAgentFullName"),
            "buyerAgentLicense": row.get("BuyerAgentStateLicense"),
            "buyerOfficeName": row.get("BuyerOfficeName"),
            "disclosures": to_string_list(row.get("Disclosures")),
            "documentsAvailable": to_string_list(row.get("DocumentsAvailable")),
            "specialConditions": to_string_list(row.get("SpecialListingConditions")),
            "inclusions": to_string_list(row.get("Inclusions")),
            "exclusions": to_string_list(row.get("Exclusions")),
            "associationName": row.get("AssociationName").or_else(|| row.get("AssociationName2")),
            "associationFeeIncludes": to_string_list(row.get("AssociationFeeIncludes")),
            "associationAmenities": to_string_list(row.get("AssociationAmenities")),
            "subdivisionName": row.get("SubdivisionName"),
            "remarks": row.get("PublicRemarks"),
        });
        if inc_photos {
            obj["photos"] = json!(photos);
            if !media_docs.is_empty() {
                obj["documents"] = json!(media_docs);
            }
        }
        if let Some(m) = obj.as_object_mut() {
            attach_optional_engagement_fields(m, row);
        }
        obj
    };

    let mls_listing = map_listing(p, true);
    let past_listings: Vec<Value> = rows.iter().map(|r| map_listing(r, false)).collect();
    let mut facts = Map::new();
    if !media_docs.is_empty() {
        facts.insert("documents".into(), json!(media_docs));
    }
    if let Some(pt) = p.get("PropertySubType").or_else(|| p.get("PropertyType")) {
        if !pt.is_null() {
            facts.insert("propertyType".into(), pt.clone());
        }
    }
    if let Some(b) = p.get("BedroomsTotal").filter(|v| !v.is_null()) {
        facts.insert("beds".into(), b.clone());
    }
    if let Some(ba) = p
        .get("BathroomsTotalDecimal")
        .or_else(|| p.get("BathroomsTotalInteger"))
        .filter(|v| !v.is_null())
    {
        facts.insert("baths".into(), ba.clone());
    }
    if let Some(sq) = p
        .get("LivingArea")
        .or_else(|| p.get("BuildingAreaTotal"))
        .filter(|v| !v.is_null())
    {
        facts.insert("sqft".into(), sq.clone());
    }
    if let Some(ls) = p.get("LotSizeSquareFeet").filter(|v| !v.is_null()) {
        facts.insert("lotSqft".into(), ls.clone());
    }
    if let Some(yb) = p.get("YearBuilt").filter(|v| !v.is_null()) {
        facts.insert("yearBuilt".into(), yb.clone());
    }
    if let Some(hoa) = norm_monthly_hoa(
        p.get("AssociationFee").and_then(|v| v.as_f64()),
        p.get("AssociationFeeFrequency").and_then(|v| v.as_str()),
    ) {
        facts.insert("hoaMonthly".into(), json!(hoa));
    }
    if let Some(assoc_nm) = p
        .get("AssociationName")
        .or_else(|| p.get("AssociationName2"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        facts.insert("hoaName".into(), json!(assoc_nm));
        facts.insert("associationName".into(), json!(assoc_nm));
    }
    let assoc_inc = to_string_list(p.get("AssociationFeeIncludes"));
    if !assoc_inc.is_empty() {
        facts.insert(
            "hoa".into(),
            json!({
                "associationName": p.get("AssociationName").or_else(|| p.get("AssociationName2")),
                "includes": assoc_inc,
                "amenities": to_string_list(p.get("AssociationAmenities")),
            }),
        );
    }
    if let Some(subdiv) = p
        .get("SubdivisionName")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        facts.insert("subdivision".into(), json!(subdiv));
    }
    if let Some(apn) = p.get("ParcelNumber").filter(|v| !v.is_null()) {
        facts.insert("apn".into(), apn.clone());
    }
    facts.insert("mlsListing".into(), mls_listing.clone());
    facts.insert("pastListings".into(), json!(past_listings));

    let mls_no = mls_listing
        .get("mlsNumber")
        .and_then(|v| v.as_str())
        .unwrap_or("RESO");
    Ok(MlsAndRecordsOutput {
        facts,
        sources: vec![SourceItem::new(
            format!("MLS Listing #{mls_no} (RESO Web API)"),
            base,
        )],
        ..Default::default()
    })
}

async fn fetch_simplyrets(
    http: &HttpClient,
    address: &str,
    geo: &Value,
    auth: &str,
) -> Result<MlsAndRecordsOutput, String> {
    let parts = split_street_parts(address, geo);
    let token = BASE64_STD.encode(auth.as_bytes());
    let auth_hdr = format!("Basic {token}");
    let headers = [
        ("Authorization", auth_hdr.as_str()),
        ("Accept", "application/json"),
    ];
    let mut u = Url::parse(&format!("{SIMPLYRETS_BASE}/properties")).map_err(|e| e.to_string())?;
    u.query_pairs_mut()
        .append_pair("q", format!("{} {}", parts.street, parts.zip).trim())
        .append_pair("limit", "5");

    let j = http
        .get_json_opts(u.as_str(), 15_000, 1, 3600, Some(&headers))
        .await
        .map_err(|e| e.to_string())?;
    let Some(p) = j.as_array().and_then(|a| a.first()) else {
        return Ok(MlsAndRecordsOutput {
            warnings: vec!["SimplyRETS returned 0 matching listings.".into()],
            ..Default::default()
        });
    };
    let prop = p.get("property").cloned().unwrap_or(Value::Null);
    let mls_no = p
        .get("mlsId")
        .or_else(|| p.get("listingId"))
        .map(|v| v.to_string())
        .unwrap_or_default();
    let mut mls_listing = json!({
        "provider": "SimplyRETS MLS",
        "mlsNumber": mls_no,
        "status": p.get("mls").and_then(|m| m.get("status")),
        "listPrice": p.get("listPrice"),
        "closePrice": p.get("sales").and_then(|s| s.get("closePrice")),
        "daysOnMarket": p.get("mls").and_then(|m| m.get("daysOnMarket")),
        "remarks": p.get("remarks"),
    });
    if let Some(m) = mls_listing.as_object_mut() {
        attach_optional_engagement_fields(m, p);
    }
    let mut facts = Map::new();
    if let Some(pt) = prop.get("type").filter(|v| !v.is_null()) {
        facts.insert("propertyType".into(), pt.clone());
    }
    if let Some(b) = prop.get("bedrooms").filter(|v| !v.is_null()) {
        facts.insert("beds".into(), b.clone());
    }
    if let Some(sq) = prop.get("area").filter(|v| !v.is_null()) {
        facts.insert("sqft".into(), sq.clone());
    }
    if let Some(yb) = prop.get("yearBuilt").filter(|v| !v.is_null()) {
        facts.insert("yearBuilt".into(), yb.clone());
    }
    facts.insert("mlsListing".into(), mls_listing);
    Ok(MlsAndRecordsOutput {
        facts,
        sources: vec![SourceItem::new(
            format!("SimplyRETS MLS #{mls_no}"),
            SIMPLYRETS_BASE,
        )],
        ..Default::default()
    })
}

async fn fetch_attom(
    http: &HttpClient,
    address: &str,
    geo: &Value,
    api_key: &str,
) -> Result<MlsAndRecordsOutput, String> {
    let parts = split_street_parts(address, geo);
    let address2 = [
        parts.city.as_str(),
        parts.state.as_str(),
        parts.zip.as_str(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    let headers = [("apikey", api_key), ("Accept", "application/json")];

    let mut u_prof =
        Url::parse(&format!("{ATTOM_BASE}/property/expandedprofile")).map_err(|e| e.to_string())?;
    u_prof
        .query_pairs_mut()
        .append_pair("address1", &parts.street)
        .append_pair("address2", &address2);
    let mut u_avm =
        Url::parse(&format!("{ATTOM_BASE}/attomavm/detail")).map_err(|e| e.to_string())?;
    u_avm
        .query_pairs_mut()
        .append_pair("address1", &parts.street)
        .append_pair("address2", &address2);

    let mut u_hist =
        Url::parse(&format!("{ATTOM_BASE}/saleshistory/detail")).map_err(|e| e.to_string())?;
    u_hist
        .query_pairs_mut()
        .append_pair("address1", &parts.street)
        .append_pair("address2", &address2);

    let (r_prof, r_avm, r_hist) = tokio::join!(
        http.get_json_opts(u_prof.as_str(), 18_000, 1, 86400, Some(&headers)),
        http.get_json_opts(u_avm.as_str(), 18_000, 1, 86400, Some(&headers)),
        http.get_json_opts(u_hist.as_str(), 18_000, 1, 86400, Some(&headers))
    );

    let mut out = MlsAndRecordsOutput::default();
    if let Ok(pj) = r_prof {
        if let Some(p) = pj
            .get("property")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
        {
            if let Some(pt) = p
                .get("summary")
                .and_then(|s| s.get("propclass").or_else(|| s.get("propsubtype")))
                .filter(|v| !v.is_null())
            {
                out.facts.insert("propertyType".into(), pt.clone());
            }
            if let Some(b) = p
                .get("building")
                .and_then(|b| b.get("rooms"))
                .and_then(|r| r.get("beds"))
                .filter(|v| !v.is_null())
            {
                out.facts.insert("beds".into(), b.clone());
            }
            if let Some(ba) = p
                .get("building")
                .and_then(|b| b.get("rooms"))
                .and_then(|r| r.get("bathstotal"))
                .filter(|v| !v.is_null())
            {
                out.facts.insert("baths".into(), ba.clone());
            }
            if let Some(sq) = p
                .get("building")
                .and_then(|b| b.get("size"))
                .and_then(|s| s.get("livingsize").or_else(|| s.get("universalsize")))
                .filter(|v| !v.is_null())
            {
                out.facts.insert("sqft".into(), sq.clone());
            }
            if let Some(ls) = p
                .get("lot")
                .and_then(|l| l.get("lotsize2"))
                .filter(|v| !v.is_null())
            {
                out.facts.insert("lotSqft".into(), ls.clone());
            }
            if let Some(yb) = p
                .get("summary")
                .and_then(|s| s.get("yearbuilt"))
                .filter(|v| !v.is_null())
            {
                out.facts.insert("yearBuilt".into(), yb.clone());
            }
            if let Some(apn) = p
                .get("identifier")
                .and_then(|i| i.get("apn"))
                .filter(|v| !v.is_null())
            {
                out.facts.insert("apn".into(), apn.clone());
            }
            let sale_obj = p.get("sale");
            if let (Some(dt), Some(pr)) = (
                sale_obj
                    .and_then(|s| {
                        s.get("saleTransDate")
                            .or_else(|| s.pointer("/amount/salerecdate"))
                    })
                    .and_then(|v| v.as_str())
                    .and_then(parse_portal_date_iso),
                sale_obj
                    .and_then(|s| s.pointer("/amount/saleamt"))
                    .and_then(|v| v.as_f64())
                    .map(|x| x.round() as i64)
                    .filter(|&x| x > 0),
            ) {
                out.facts
                    .insert("lastSale".into(), json!({ "date": dt, "price": pr }));
            }
            let sale_type = sale_obj
                .and_then(|s| s.get("saleTransType"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if RE_DISTRESS.is_match(sale_type) {
                out.facts.insert(
                    "foreclosure".into(),
                    json!({
                        "status": "distressed_record",
                        "label": sale_type,
                        "source": "ATTOM County Sale Record",
                    }),
                );
            }
            out.sources.push(SourceItem::new(
                "ATTOM Property Expanded Profile",
                ATTOM_BASE,
            ));
        }
    }
    if let Ok(hj) = r_hist {
        if let Some(arr) = hj
            .get("property")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.get("salehistory"))
            .and_then(|s| s.as_array())
        {
            let mut sh = Vec::new();
            for ev in arr {
                let dt = ev
                    .get("saleTransDate")
                    .or_else(|| ev.pointer("/amount/salerecdate"))
                    .and_then(|v| v.as_str())
                    .and_then(parse_portal_date_iso);
                let Some(dt) = dt else { continue };
                let pr = ev
                    .pointer("/amount/saleamt")
                    .and_then(|v| v.as_f64())
                    .map(|x| x.round() as i64)
                    .filter(|&x| x > 0);
                let doc_no = ev
                    .pointer("/amount/saledocnum")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                let tr_type = ev
                    .pointer("/amount/saletranstype")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Sold");
                let mut row = json!({
                    "date": dt,
                    "event": "Sold",
                    "deedType": tr_type,
                    "price": pr,
                    "source": "ATTOM County Sale History",
                });
                if let Some(dn) = doc_no {
                    row["docNumber"] = json!(dn);
                }
                sh.push(row);
            }
            sh.sort_by(|a, b| {
                let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
                let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
                db.cmp(da)
            });
            if let Some(first_priced) = sh
                .iter()
                .find(|r| r.get("price").and_then(|v| v.as_i64()).unwrap_or(0) > 0)
            {
                if !out.facts.contains_key("lastSale") {
                    out.facts.insert(
                        "lastSale".into(),
                        json!({
                            "date": first_priced.get("date"),
                            "price": first_priced.get("price"),
                        }),
                    );
                }
            }
            if !sh.is_empty() {
                out.facts.insert("saleHistory".into(), Value::Array(sh));
            }
        }
    }
    if let Ok(aj) = r_avm {
        if let Some(av) = aj
            .get("property")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.get("avm"))
        {
            if let Some(val) = av
                .get("amount")
                .and_then(|a| a.get("value"))
                .and_then(|v| v.as_f64())
            {
                out.avms.push(json!({
                    "source": "ATTOM AVM",
                    "value": val.round() as i64,
                    "low": av.get("amount").and_then(|a| a.get("low")),
                    "high": av.get("amount").and_then(|a| a.get("high")),
                    "date": av.get("eventDate").and_then(|v| v.as_str()).map(|s| &s[..s.len().min(10)]).unwrap_or(&now_iso()[..10]),
                }));
                out.sources.push(SourceItem::new(
                    "ATTOM Automated Valuation Model",
                    format!("{ATTOM_BASE}/attomavm/detail"),
                ));
            }
        }
    }
    Ok(out)
}

pub async fn fetch_mls_and_records(
    http: &HttpClient,
    env: &EnvConfig,
    address: &str,
    geo: &Value,
) -> MlsAndRecordsOutput {
    Box::pin(fetch_mls_and_records_inner(http, env, address, geo)).await
}

async fn fetch_mls_and_records_inner(
    http: &HttpClient,
    env: &EnvConfig,
    address: &str,
    geo: &Value,
) -> MlsAndRecordsOutput {
    let reso_fut = Box::pin(async {
        match (env.get("RESO_API_URL"), env.get("RESO_BEARER_TOKEN")) {
            (Some(u), Some(t)) => Some(Box::pin(fetch_reso(http, address, geo, u, t)).await),
            _ => None,
        }
    });
    let sr_fut = Box::pin(async {
        match env.get("SIMPLYRETS_AUTH") {
            Some(a) => Some(Box::pin(fetch_simplyrets(http, address, geo, a)).await),
            None => None,
        }
    });
    let attom_fut = Box::pin(async {
        match env.get("ATTOM_API_KEY") {
            Some(k) => Some(Box::pin(fetch_attom(http, address, geo, k)).await),
            None => None,
        }
    });
    let gis_fut = Box::pin(async {
        let pr = fetch_public_parcel_records(http, address, geo).await;
        Ok::<MlsAndRecordsOutput, String>(MlsAndRecordsOutput {
            facts: pr.facts,
            nearby_parcels: pr.nearby_parcels,
            sources: pr.sources,
            warnings: pr.warnings,
            ..Default::default()
        })
    });
    let (reso_opt, sr_opt, attom_opt, gis_res) = tokio::join!(reso_fut, sr_fut, attom_fut, gis_fut);

    let ordered: [(&str, Option<Result<MlsAndRecordsOutput, String>>); 4] = [
        ("reso", reso_opt),
        ("simplyrets", sr_opt),
        ("attom", attom_opt),
        ("county_gis", Some(gis_res)),
    ];

    let mut combined = MlsAndRecordsOutput::default();

    for (key, res_opt) in ordered {
        let Some(res) = res_opt else {
            continue;
        };
        let v = match res {
            Ok(val) => val,
            Err(err) => {
                combined
                    .warnings
                    .push(format!("{key} connector failed: {err}"));
                continue;
            }
        };
        if !v.facts.is_empty()
            || !v.sources.is_empty()
            || !v.comps.is_empty()
            || !v.nearby_listings.is_empty()
            || !v.nearby_pending.is_empty()
        {
            combined.connectors_used.push(key.to_string());
        }
        combined.sources.extend(v.sources);
        combined.warnings.extend(v.warnings);
        if combined.nearby_parcels.is_empty() && !v.nearby_parcels.is_empty() {
            combined.nearby_parcels = v.nearby_parcels;
        }

        for (fk, fv) in v.facts {
            if fv.is_null() {
                continue;
            }
            if fk == "taxHistory" {
                if let Some(arr) = fv.as_array() {
                    let cur = combined
                        .facts
                        .get("taxHistory")
                        .and_then(|x| x.as_array())
                        .cloned()
                        .unwrap_or_default();
                    combined
                        .facts
                        .insert("taxHistory".into(), json!(merge_tax_histories(&cur, arr)));
                }
            } else if fk == "saleHistory" {
                if let Some(arr) = fv.as_array() {
                    let mut cur = combined
                        .facts
                        .get("saleHistory")
                        .and_then(|x| x.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for item in arr {
                        let dt = item.get("date").and_then(|v| v.as_str()).unwrap_or("");
                        let ym_key = &dt[..dt.len().min(7)];
                        let pr = item.get("price").and_then(|v| v.as_i64());
                        let dup = cur.iter().any(|existing| {
                            let edt = existing.get("date").and_then(|v| v.as_str()).unwrap_or("");
                            let eym = &edt[..edt.len().min(7)];
                            let epr = existing.get("price").and_then(|v| v.as_i64());
                            !ym_key.is_empty() && ym_key == eym && (pr == epr || epr.is_some())
                        });
                        if !dup {
                            cur.push(item.clone());
                        }
                    }
                    cur.sort_by(|a, b| {
                        let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
                        let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
                        db.cmp(da)
                    });
                    combined
                        .facts
                        .insert("saleHistory".into(), Value::Array(cur));
                }
            } else if fk == "lastSale" {
                let cur_has_price = combined
                    .facts
                    .get("lastSale")
                    .and_then(|x| x.get("price"))
                    .and_then(|p| p.as_i64())
                    .map(|p| p > 0)
                    .unwrap_or(false);
                if !cur_has_price {
                    combined.facts.insert(fk, fv);
                }
            } else if fk == "titleAndLiens" {
                if let Some(new_obj) = fv.as_object() {
                    let mut cur = combined
                        .facts
                        .get("titleAndLiens")
                        .and_then(|x| x.as_object())
                        .cloned()
                        .unwrap_or_default();
                    let mut transfers = cur
                        .get("transfers")
                        .and_then(|t| t.as_array())
                        .cloned()
                        .unwrap_or_default();
                    if let Some(nt) = new_obj.get("transfers").and_then(|t| t.as_array()) {
                        transfers.extend(nt.iter().cloned());
                    }
                    for (tk, tv) in new_obj {
                        if !tv.is_null() && !cur.contains_key(tk) {
                            cur.insert(tk.clone(), tv.clone());
                        }
                    }
                    cur.insert("transfers".into(), json!(transfers));
                    combined
                        .facts
                        .insert("titleAndLiens".into(), Value::Object(cur));
                }
            } else if !combined.facts.contains_key(&fk) {
                combined.facts.insert(fk, fv);
            }
        }

        combined.avms.extend(v.avms);
        if combined.comps.is_empty() && !v.comps.is_empty() {
            combined.comps = v.comps;
        }
        if combined.rental_comps.is_empty() && !v.rental_comps.is_empty() {
            combined.rental_comps = v.rental_comps;
        }
        if combined.nearby_listings.is_empty() && !v.nearby_listings.is_empty() {
            combined.nearby_listings = v.nearby_listings;
        }
        if combined.nearby_pending.is_empty() && !v.nearby_pending.is_empty() {
            combined.nearby_pending = v.nearby_pending;
        }
        if combined.nearby_rentals.is_empty() && !v.nearby_rentals.is_empty() {
            combined.nearby_rentals = v.nearby_rentals;
        }
        if combined.rent_estimate.is_none() && v.rent_estimate.is_some() {
            combined.rent_estimate = v.rent_estimate;
        }
        if combined.rent_range.is_none() && v.rent_range.is_some() {
            combined.rent_range = v.rent_range;
        }
    }

    combined
}

fn norm_street_key(raw: &str) -> String {
    let first = raw.split(',').next().unwrap_or("").trim();
    let card = RE_CARDINAL_WORD.replace_all(first, |caps: &regex::Captures| {
        caps[1]
            .chars()
            .next()
            .map(|c| c.to_ascii_lowercase().to_string())
            .unwrap_or_default()
    });
    let stripped = RE_ST_SUFFIX.replace(&card, "");
    stripped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn parse_clean_f64(s: Option<&str>) -> Option<f64> {
    let raw = s?.trim();
    if raw.is_empty() {
        return None;
    }
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == "-" {
        return None;
    }
    cleaned.parse::<f64>().ok().filter(|x| x.is_finite())
}

fn parse_clean_i64(s: Option<&str>) -> Option<i64> {
    parse_clean_f64(s).map(|v| v.round() as i64)
}

pub fn parse_public_gis_active_csv(
    csv_text: &str,
    subject_address: &str,
    geo: &Value,
    subj_lat: f64,
    subj_lon: f64,
) -> Vec<Value> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .has_headers(false)
        .trim(csv::Trim::All)
        .from_reader(csv_text.as_bytes());

    let mut headers: Vec<String> = Vec::new();
    let mut records: Vec<csv::StringRecord> = Vec::new();
    for rec in rdr.records().flatten() {
        if headers.is_empty() {
            if rec.iter().any(|c| c.trim().eq_ignore_ascii_case("address")) {
                headers = rec.iter().map(|c| c.trim().to_ascii_lowercase()).collect();
            }
            continue;
        }
        records.push(rec);
    }
    if headers.is_empty() {
        return Vec::new();
    }

    let find_col = |exact: &[&str], contains: &[&str]| -> Option<usize> {
        for &ex in exact {
            if let Some(i) = headers.iter().position(|h| h == ex) {
                return Some(i);
            }
        }
        for &ct in contains {
            if let Some(i) = headers.iter().position(|h| h.contains(ct)) {
                return Some(i);
            }
        }
        None
    };

    let c_sale_type = find_col(&["sale type"], &["sale type"]);
    let c_prop_type = find_col(&["property type"], &["property type"]);
    let c_addr = find_col(&["address", "street"], &["address"]);
    let c_city = find_col(&["city"], &[]);
    let c_state = find_col(&["state or province", "state"], &["state"]);
    let c_zip = find_col(&["zip or postal code", "zip"], &["zip", "postal"]);
    let c_price = find_col(&["price", "list price"], &["price"]);
    let c_beds = find_col(&["beds", "bedrooms"], &["bed"]);
    let c_baths = find_col(&["baths", "bathrooms"], &["bath"]);
    let c_sqft = find_col(&["square feet", "sqft"], &["square feet"]);
    let c_lot = find_col(&["lot size", "lot sqft"], &["lot size"]);
    let c_yb = find_col(&["year built"], &["year built"]);
    let c_dom = find_col(&["days on market", "dom"], &["days on market"]);
    let c_ppsf = find_col(&["$/square feet", "price/sqft"], &["$/square feet"]);
    let c_hoa = find_col(&["hoa/month", "hoa"], &["hoa/month"]);
    let c_status = find_col(&["status"], &["status"]);
    let c_url = headers
        .iter()
        .position(|h| h == "url" || h.starts_with("url "));
    let c_source = find_col(&["source"], &[]);
    let c_mls = find_col(&["mls#", "mls number", "mls"], &["mls#"]);
    let c_lat = find_col(&["latitude", "lat"], &["latitude"]);
    let c_lon = find_col(&["longitude", "lon", "lng"], &["longitude"]);

    let Some(i_addr) = c_addr else {
        return Vec::new();
    };

    let subj_parts = split_street_parts(subject_address, geo);
    let mut subj_keys = HashSet::new();
    let k1 = norm_street_key(subject_address);
    if !k1.is_empty() {
        subj_keys.insert(k1);
    }
    if let Some(disp) = geo.get("displayAddress").and_then(|v| v.as_str()) {
        let k2 = norm_street_key(disp);
        if !k2.is_empty() {
            subj_keys.insert(k2);
        }
    }

    let mut seen_mls = HashSet::new();
    let mut seen_streets = HashSet::new();
    let mut candidates: Vec<(f64, i64, Value)> = Vec::new();

    for rec in records {
        let cell = |idx: Option<usize>| -> Option<&str> {
            let s = rec.get(idx?)?.trim();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };

        let first_cell = rec.get(0).unwrap_or("").trim();
        if first_cell
            .to_ascii_lowercase()
            .starts_with("in accordance with")
        {
            continue;
        }
        let Some(street) = cell(Some(i_addr)) else {
            continue;
        };
        if street
            .to_ascii_lowercase()
            .starts_with("in accordance with")
        {
            continue;
        }

        let sale_type_low = cell(c_sale_type).unwrap_or("").to_ascii_lowercase();
        if sale_type_low.contains("past sale") || sale_type_low == "sold" {
            continue;
        }
        let status_raw = cell(c_status).unwrap_or("Active");
        let status_low = status_raw.to_ascii_lowercase();
        if status_low.contains("sold")
            || status_low.contains("closed")
            || status_low.contains("off market")
            || status_low.contains("expired")
            || status_low.contains("withdrawn")
        {
            continue;
        }
        let status_lbl = if status_low.contains("coming") {
            "Coming Soon"
        } else if status_low.contains("pend")
            || status_low.contains("contingent")
            || status_low.contains("contract")
        {
            "Pending"
        } else {
            "Active"
        };

        let Some(price) = parse_clean_i64(cell(c_price)).filter(|&p| p > 0) else {
            continue;
        };
        let (Some(rlat), Some(rlon)) = (parse_clean_f64(cell(c_lat)), parse_clean_f64(cell(c_lon)))
        else {
            continue;
        };

        let row_key = norm_street_key(street);
        if !row_key.is_empty() && subj_keys.contains(&row_key) {
            continue;
        }

        let dist_mi = haversine_mi(subj_lat, subj_lon, rlat, rlon);
        if dist_mi > 2.5 {
            continue;
        }
        if dist_mi < 0.012
            && !subj_parts.street_number.is_empty()
            && street
                .split_whitespace()
                .next()
                .map(|n| n.eq_ignore_ascii_case(&subj_parts.street_number))
                .unwrap_or(false)
        {
            continue;
        }

        let mls_num = cell(c_mls).map(ToOwned::to_owned);
        if let Some(ref mn) = mls_num {
            if !seen_mls.insert(mn.to_ascii_uppercase()) {
                continue;
            }
        }
        if !row_key.is_empty() && !seen_streets.insert(row_key) {
            continue;
        }

        let city = cell(c_city).unwrap_or(subj_parts.city.as_str());
        let state = cell(c_state).unwrap_or(subj_parts.state.as_str());
        let zip: String = cell(c_zip)
            .unwrap_or(subj_parts.zip.as_str())
            .chars()
            .take(5)
            .collect();
        let state_zip = format!("{state} {zip}").trim().to_string();
        let full_addr = if street.contains(',') {
            street.to_string()
        } else {
            [
                Some(street),
                (!city.is_empty()).then_some(city),
                (!state_zip.is_empty()).then_some(state_zip.as_str()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
        };

        let sqft = parse_clean_i64(cell(c_sqft)).filter(|&v| v > 0);
        let ppsf = parse_clean_i64(cell(c_ppsf))
            .filter(|&v| v > 0)
            .or_else(|| sqft.map(|sq| ((price as f64) / (sq as f64)).round() as i64));
        let beds = parse_clean_f64(cell(c_beds)).filter(|&v| v >= 0.0);
        let baths = parse_clean_f64(cell(c_baths)).filter(|&v| v > 0.0);
        let lot_sqft = parse_clean_i64(cell(c_lot)).filter(|&v| v > 0);
        let year_built = parse_clean_i64(cell(c_yb)).filter(|&v| (1700..=2035).contains(&v));
        let dom = parse_clean_i64(cell(c_dom)).filter(|&v| v >= 0);
        let hoa = parse_clean_i64(cell(c_hoa)).filter(|&v| v >= 0);
        let prop_type = cell(c_prop_type).map(ToOwned::to_owned);
        let asset_class = classify_asset_class(prop_type.as_deref());
        let source = cell(c_source)
            .map(|s| format!("MLS IDX ({s})"))
            .unwrap_or_else(|| "Public MLS IDX".to_string());
        let url = cell(c_url)
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .map(ToOwned::to_owned);

        let beds_val = match beds {
            Some(b) if (b - b.round()).abs() < 1e-9 => json!(b.round() as i64),
            Some(b) => json!(b),
            None => Value::Null,
        };
        let baths_val = match baths {
            Some(b) if (b - b.round()).abs() < 1e-9 => json!(b.round() as i64),
            Some(b) => json!(b),
            None => Value::Null,
        };

        let dist_r = round_n(dist_mi, 2);
        let item = json!({
            "address": full_addr,
            "status": status_lbl,
            "price": price,
            "pricePerSqft": ppsf,
            "beds": beds_val,
            "baths": baths_val,
            "sqft": sqft,
            "lotSqft": lot_sqft,
            "yearBuilt": year_built,
            "daysOnMarket": dom,
            "hoaMonthly": hoa,
            "propertyType": prop_type,
            "assetClass": asset_class,
            "mlsNumber": mls_num,
            "source": source,
            "url": url,
            "lat": round_n(rlat, 6),
            "lon": round_n(rlon, 6),
            "distanceMi": dist_r,
        });
        candidates.push((dist_mi, price, item));
    }

    candidates.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    candidates.into_iter().take(16).map(|(_, _, v)| v).collect()
}

/// Normalize portal / CSV date strings (`"October-15-2026"`, `"Aug-24-2026"`, `"2026-10-15"`,
/// `"10/15/2026"`, `"10/15/26"`, `"October 15, 2026"`) into ISO `YYYY-MM-DD`.
pub fn parse_portal_date_iso(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.len() >= 10
        && t.as_bytes().get(4) == Some(&b'-')
        && t.as_bytes().get(7) == Some(&b'-')
        && t[0..4].chars().all(|c| c.is_ascii_digit())
        && t[5..7].chars().all(|c| c.is_ascii_digit())
        && t[8..10].chars().all(|c| c.is_ascii_digit())
    {
        let yr: i32 = t[0..4].parse().ok()?;
        let mo: u32 = t[5..7].parse().ok()?;
        let dy: u32 = t[8..10].parse().ok()?;
        if (1850..=2100).contains(&yr) && (1..=12).contains(&mo) && (1..=31).contains(&dy) {
            return Some(format!("{yr:04}-{mo:02}-{dy:02}"));
        }
    }

    let month_num = |m: &str| -> Option<u32> {
        match m.trim().trim_end_matches('.').to_ascii_lowercase().as_str() {
            "1" | "01" | "jan" | "january" => Some(1),
            "2" | "02" | "feb" | "february" => Some(2),
            "3" | "03" | "mar" | "march" => Some(3),
            "4" | "04" | "apr" | "april" => Some(4),
            "5" | "05" | "may" => Some(5),
            "6" | "06" | "jun" | "june" => Some(6),
            "7" | "07" | "jul" | "july" => Some(7),
            "8" | "08" | "aug" | "august" => Some(8),
            "9" | "09" | "sep" | "sept" | "september" => Some(9),
            "10" | "oct" | "october" => Some(10),
            "11" | "nov" | "november" => Some(11),
            "12" | "dec" | "december" => Some(12),
            _ => None,
        }
    };

    let norm_year = |y_str: &str| -> Option<i32> {
        let y: i32 = y_str.trim().parse().ok()?;
        if (1850..=2100).contains(&y) {
            Some(y)
        } else if (0..=99).contains(&y) {
            let cur_yy = (chrono::Utc::now()
                .date_naive()
                .format("%Y")
                .to_string()
                .parse::<i32>()
                .unwrap_or(2026))
                % 100;
            Some(if y <= cur_yy + 1 { 2000 + y } else { 1900 + y })
        } else {
            None
        }
    };

    // Split on `-`, `/`, `,`, or whitespace (`October-15-2026`, `10/15/2026`, `Oct 15, 2026`)
    let toks: Vec<&str> = t
        .split(['-', '/', ',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if toks.len() >= 3 {
        if let (Some(mo), Ok(dy), Some(yr)) = (
            month_num(toks[0]),
            toks[1].parse::<u32>(),
            norm_year(toks[2]),
        ) {
            if (1..=31).contains(&dy) {
                return Some(format!("{yr:04}-{mo:02}-{dy:02}"));
            }
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
pub struct PublicGisParsedAll {
    pub subject_facts: Map<String, Value>,
    pub sold_comps: Vec<Value>,
    pub active_listings: Vec<Value>,
}

fn approx_months_since_iso(iso_date: &str) -> Option<i32> {
    if iso_date.len() < 7 {
        return None;
    }
    let yr: i32 = iso_date[0..4].parse().ok()?;
    let mo: i32 = iso_date[5..7].parse().ok()?;
    let now = chrono::Utc::now();
    let cur_yr: i32 = now.format("%Y").to_string().parse().ok()?;
    let cur_mo: i32 = now.format("%m").to_string().parse().ok()?;
    Some((cur_yr * 12 + cur_mo) - (yr * 12 + mo))
}

fn has_unit_marker(s: &str) -> bool {
    let low = s.to_ascii_lowercase();
    low.contains('#') || low.contains(" unit ") || low.contains(" apt ") || low.contains(" ste ")
}

pub fn parse_public_gis_csv_all(
    csv_text: &str,
    subject_address: &str,
    geo: &Value,
    subj_lat: f64,
    subj_lon: f64,
) -> PublicGisParsedAll {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .has_headers(false)
        .trim(csv::Trim::All)
        .from_reader(csv_text.as_bytes());

    let mut headers: Vec<String> = Vec::new();
    let mut records: Vec<csv::StringRecord> = Vec::new();
    for rec in rdr.records().flatten() {
        if headers.is_empty() {
            if rec.iter().any(|c| c.trim().eq_ignore_ascii_case("address")) {
                headers = rec.iter().map(|c| c.trim().to_ascii_lowercase()).collect();
            }
            continue;
        }
        records.push(rec);
    }
    if headers.is_empty() {
        return PublicGisParsedAll::default();
    }

    let find_col = |exact: &[&str], contains: &[&str]| -> Option<usize> {
        for &ex in exact {
            if let Some(i) = headers.iter().position(|h| h == ex) {
                return Some(i);
            }
        }
        for &ct in contains {
            if let Some(i) = headers.iter().position(|h| h.contains(ct)) {
                return Some(i);
            }
        }
        None
    };

    let c_sale_type = find_col(&["sale type"], &["sale type"]);
    let c_sold_date = find_col(&["sold date", "close date"], &["sold date", "close date"]);
    let c_prop_type = find_col(&["property type"], &["property type"]);
    let c_addr = find_col(&["address", "street"], &["address"]);
    let c_city = find_col(&["city"], &[]);
    let c_state = find_col(&["state or province", "state"], &["state"]);
    let c_zip = find_col(&["zip or postal code", "zip"], &["zip", "postal"]);
    let c_price = find_col(&["price", "list price"], &["price"]);
    let c_beds = find_col(&["beds", "bedrooms"], &["bed"]);
    let c_baths = find_col(&["baths", "bathrooms"], &["bath"]);
    let c_sqft = find_col(&["square feet", "sqft"], &["square feet"]);
    let c_lot = find_col(&["lot size", "lot sqft"], &["lot size"]);
    let c_yb = find_col(&["year built"], &["year built"]);
    let c_dom = find_col(&["days on market", "dom"], &["days on market"]);
    let c_ppsf = find_col(&["$/square feet", "price/sqft"], &["$/square feet"]);
    let c_hoa = find_col(&["hoa/month", "hoa"], &["hoa/month"]);
    let c_status = find_col(&["status"], &["status"]);
    let c_url = headers
        .iter()
        .position(|h| h == "url" || h.starts_with("url "));
    let c_source = find_col(&["source"], &[]);
    let c_mls = find_col(&["mls#", "mls number", "mls"], &["mls#"]);
    let c_lat = find_col(&["latitude", "lat"], &["latitude"]);
    let c_lon = find_col(&["longitude", "lon", "lng"], &["longitude"]);

    let Some(i_addr) = c_addr else {
        return PublicGisParsedAll::default();
    };

    let subj_parts = split_street_parts(subject_address, geo);
    let subj_has_unit = has_unit_marker(&subj_parts.street);
    let subj_name_kw = subj_parts
        .street_name
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut subj_keys = HashSet::new();
    let k1 = norm_street_key(subject_address);
    if !k1.is_empty() {
        subj_keys.insert(k1);
    }
    if let Some(disp) = geo.get("displayAddress").and_then(|v| v.as_str()) {
        let k2 = norm_street_key(disp);
        if !k2.is_empty() {
            subj_keys.insert(k2);
        }
    }

    let mut subj_sales: Vec<Value> = Vec::new();
    let mut subj_active_mls: Option<Value> = None;
    let mut subj_spec_facts: Map<String, Value> = Map::new();
    let mut subj_asset_class: Option<String> = None;

    let mut seen_sold_mls = HashSet::new();
    let mut seen_sold_streets = HashSet::new();
    let mut sold_candidates: Vec<(i32, f64, Option<String>, Value)> = Vec::new();

    let mut seen_act_mls = HashSet::new();
    let mut seen_act_streets = HashSet::new();
    let mut active_candidates: Vec<(f64, i64, Value)> = Vec::new();

    for rec in records {
        let cell = |idx: Option<usize>| -> Option<&str> {
            let s = rec.get(idx?)?.trim();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };

        let first_cell = rec.get(0).unwrap_or("").trim();
        if first_cell
            .to_ascii_lowercase()
            .starts_with("in accordance with")
        {
            continue;
        }
        let Some(street) = cell(Some(i_addr)) else {
            continue;
        };
        if street
            .to_ascii_lowercase()
            .starts_with("in accordance with")
        {
            continue;
        }

        let price_opt = parse_clean_i64(cell(c_price)).filter(|&p| p > 0);
        let rlat = parse_clean_f64(cell(c_lat));
        let rlon = parse_clean_f64(cell(c_lon));
        let dist_mi_opt = match (rlat, rlon) {
            (Some(la), Some(lo)) => Some(haversine_mi(subj_lat, subj_lon, la, lo)),
            _ => None,
        };

        let row_key = norm_street_key(street);
        let is_subject = (!row_key.is_empty() && subj_keys.contains(&row_key))
            || (!subj_has_unit
                && !has_unit_marker(street)
                && dist_mi_opt.map(|d| d < 0.016).unwrap_or(false)
                && !subj_parts.street_number.is_empty()
                && street
                    .split_whitespace()
                    .next()
                    .map(|n| n.eq_ignore_ascii_case(&subj_parts.street_number))
                    .unwrap_or(false)
                && (subj_name_kw.is_empty()
                    || street.to_ascii_lowercase().contains(&subj_name_kw)));

        let sale_type_low = cell(c_sale_type).unwrap_or("").to_ascii_lowercase();
        let status_raw = cell(c_status).unwrap_or("");
        let status_low = status_raw.to_ascii_lowercase();
        let sold_dt = cell(c_sold_date).and_then(parse_portal_date_iso);
        let is_sold = sale_type_low.contains("past sale")
            || sale_type_low == "sold"
            || status_low.contains("sold")
            || status_low.contains("closed");

        let sqft = parse_clean_i64(cell(c_sqft)).filter(|&v| v > 0);
        let ppsf = parse_clean_i64(cell(c_ppsf))
            .filter(|&v| v > 0)
            .or_else(|| {
                let pr = price_opt?;
                sqft.map(|sq| ((pr as f64) / (sq as f64)).round() as i64)
            });
        let beds = parse_clean_f64(cell(c_beds)).filter(|&v| v >= 0.0);
        let baths = parse_clean_f64(cell(c_baths)).filter(|&v| v > 0.0);
        let lot_sqft = parse_clean_i64(cell(c_lot)).filter(|&v| v > 0);
        let year_built = parse_clean_i64(cell(c_yb)).filter(|&v| (1700..=2035).contains(&v));
        let dom = parse_clean_i64(cell(c_dom)).filter(|&v| v >= 0);
        let hoa = parse_clean_i64(cell(c_hoa)).filter(|&v| v >= 0);
        let prop_type = cell(c_prop_type).map(ToOwned::to_owned);
        let asset_class = classify_asset_class(prop_type.as_deref());
        let mls_num = cell(c_mls).map(ToOwned::to_owned);
        let raw_src = cell(c_source);
        let source_lbl = match raw_src {
            Some(s) if !crate::shared::has_competitor_brand(s) && !s.trim().is_empty() => {
                format!("Public MLS / {s}")
            }
            _ => "Public MLS IDX".to_string(),
        };
        let url = cell(c_url)
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .map(ToOwned::to_owned);

        let beds_val = match beds {
            Some(b) if (b - b.round()).abs() < 1e-9 => json!(b.round() as i64),
            Some(b) => json!(b),
            None => Value::Null,
        };
        let baths_val = match baths {
            Some(b) if (b - b.round()).abs() < 1e-9 => json!(b.round() as i64),
            Some(b) => json!(b),
            None => Value::Null,
        };

        if is_subject {
            if subj_asset_class.is_none() {
                subj_asset_class = asset_class.clone();
            }
            if let Some(ref pt) = prop_type {
                subj_spec_facts
                    .entry("propertyType".to_string())
                    .or_insert_with(|| json!(pt));
            }
            if !beds_val.is_null() {
                subj_spec_facts
                    .entry("beds".to_string())
                    .or_insert_with(|| beds_val.clone());
            }
            if !baths_val.is_null() {
                subj_spec_facts
                    .entry("baths".to_string())
                    .or_insert_with(|| baths_val.clone());
            }
            if let Some(sq) = sqft {
                subj_spec_facts
                    .entry("sqft".to_string())
                    .or_insert_with(|| json!(sq));
            }
            if let Some(ls) = lot_sqft {
                subj_spec_facts
                    .entry("lotSqft".to_string())
                    .or_insert_with(|| json!(ls));
                subj_spec_facts
                    .entry("mlsLotSqft".to_string())
                    .or_insert_with(|| json!(ls));
            }
            if let Some(ref u) = url {
                subj_spec_facts
                    .entry("redfinUrl".to_string())
                    .or_insert_with(|| json!(u));
                if let Some(caps) = RE_REDFIN_HOME_ID.captures(u) {
                    subj_spec_facts
                        .entry("redfinPropertyId".to_string())
                        .or_insert_with(|| json!(&caps[1]));
                }
            }
            if let Some(yb) = year_built {
                subj_spec_facts
                    .entry("yearBuilt".to_string())
                    .or_insert_with(|| json!(yb));
            }
            if let Some(h) = hoa {
                subj_spec_facts
                    .entry("hoaMonthly".to_string())
                    .or_insert_with(|| json!(h));
            }

            if let Some(price) = price_opt {
                if is_sold {
                    if let Some(ref dt) = sold_dt {
                        let mut ev = json!({
                            "date": dt,
                            "event": "Sold",
                            "price": price,
                            "source": source_lbl,
                        });
                        if let Some(ref mn) = mls_num {
                            ev["mlsNumber"] = json!(mn);
                            ev["note"] = json!(format!("MLS #{mn}"));
                        }
                        if let Some(ref u) = url {
                            ev["url"] = json!(u);
                        }
                        subj_sales.push(ev);
                    }
                } else if !status_low.contains("off market")
                    && !status_low.contains("expired")
                    && !status_low.contains("withdrawn")
                    && subj_active_mls.is_none()
                {
                    let status_lbl =
                        if status_low.contains("coming") || status_low.contains("pre on-market") {
                            "Coming Soon"
                        } else if status_low.contains("pend")
                            || status_low.contains("contingent")
                            || status_low.contains("contract")
                        {
                            "Pending"
                        } else {
                            "Active"
                        };
                    subj_active_mls = Some(json!({
                        "mlsNumber": mls_num,
                        "status": status_lbl,
                        "listPrice": price,
                        "daysOnMarket": dom,
                        "url": url,
                        "source": source_lbl,
                    }));
                }
            }
            continue;
        }

        let Some(price) = price_opt else {
            continue;
        };

        let (Some(rlat), Some(rlon), Some(dist_mi)) = (rlat, rlon, dist_mi_opt) else {
            continue;
        };

        let city = cell(c_city).unwrap_or(subj_parts.city.as_str());
        let state = cell(c_state).unwrap_or(subj_parts.state.as_str());
        let zip: String = cell(c_zip)
            .unwrap_or(subj_parts.zip.as_str())
            .chars()
            .take(5)
            .collect();
        let state_zip = format!("{state} {zip}").trim().to_string();
        let full_addr = if street.contains(',') {
            street.to_string()
        } else {
            [
                Some(street),
                (!city.is_empty()).then_some(city),
                (!state_zip.is_empty()).then_some(state_zip.as_str()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
        };
        let display_comp_addr = if street.contains(',')
            || (!subj_parts.city.is_empty() && city.eq_ignore_ascii_case(&subj_parts.city))
        {
            street.to_string()
        } else if !city.is_empty() {
            format!("{street}, {city}")
        } else {
            street.to_string()
        };

        if is_sold {
            let Some(dt) = sold_dt else { continue };
            let Some(sq) = sqft else { continue };
            let Some(ppsf_v) = ppsf.filter(|&p| (30..=15_000).contains(&p)) else {
                continue;
            };
            if dist_mi > 1.25 {
                continue;
            }
            let Some(months_ago) = approx_months_since_iso(&dt).filter(|&m| (-1..=48).contains(&m))
            else {
                continue;
            };
            if let Some(ref mn) = mls_num {
                if !seen_sold_mls.insert(mn.to_ascii_uppercase()) {
                    continue;
                }
            }
            if !row_key.is_empty() && !seen_sold_streets.insert(row_key) {
                continue;
            }
            let deal_type = if mls_num.is_some() {
                "MLS / Brokered"
            } else {
                "Recorded sale"
            };
            let comp = json!({
                "address": display_comp_addr,
                "fullAddress": full_addr,
                "soldDate": dt,
                "price": price,
                "soldPrice": price,
                "listPrice": Value::Null,
                "saleToListPct": Value::Null,
                "beds": beds_val,
                "baths": baths_val,
                "sqft": sq,
                "lotSqft": lot_sqft,
                "yearBuilt": year_built,
                "pricePerSqft": ppsf_v,
                "mlsNumber": mls_num,
                "dealType": deal_type,
                "assetClass": asset_class.clone().unwrap_or_else(|| "Single-Family Detached".to_string()),
                "isSynthetic": false,
                "synthetic": false,
                "status": "Closed",
                "source": source_lbl,
                "url": url,
                "lat": round_n(rlat, 6),
                "lon": round_n(rlon, 6),
                "distanceMi": round_n(dist_mi, 2),
            });
            sold_candidates.push((months_ago.max(0), dist_mi, asset_class, comp));
        } else if !status_low.contains("off market")
            && !status_low.contains("expired")
            && !status_low.contains("withdrawn")
            && dist_mi <= 2.5
        {
            if let Some(ref mn) = mls_num {
                if !seen_act_mls.insert(mn.to_ascii_uppercase()) {
                    continue;
                }
            }
            if !row_key.is_empty() && !seen_act_streets.insert(row_key) {
                continue;
            }
            let status_lbl =
                if status_low.contains("coming") || status_low.contains("pre on-market") {
                    "Coming Soon"
                } else if status_low.contains("pend")
                    || status_low.contains("contingent")
                    || status_low.contains("contract")
                {
                    "Pending"
                } else {
                    "Active"
                };
            let item = json!({
                "address": full_addr,
                "status": status_lbl,
                "price": price,
                "pricePerSqft": ppsf,
                "beds": beds_val,
                "baths": baths_val,
                "sqft": sqft,
                "lotSqft": lot_sqft,
                "yearBuilt": year_built,
                "daysOnMarket": dom,
                "hoaMonthly": hoa,
                "propertyType": prop_type,
                "assetClass": asset_class,
                "mlsNumber": mls_num,
                "source": source_lbl,
                "url": url,
                "lat": round_n(rlat, 6),
                "lon": round_n(rlon, 6),
                "distanceMi": round_n(dist_mi, 2),
            });
            active_candidates.push((dist_mi, price, item));
        }
    }

    // Finalize subject facts
    subj_sales.sort_by(|a, b| {
        let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
        db.cmp(da)
    });
    if let Some(latest) = subj_sales.first() {
        subj_spec_facts.insert(
            "lastSale".into(),
            json!({
                "date": latest.get("date"),
                "price": latest.get("price"),
            }),
        );
        let transfers: Vec<Value> = subj_sales
            .iter()
            .map(|s| {
                let mls_s = s
                    .get("mlsNumber")
                    .and_then(|v| v.as_str())
                    .map(|m| format!(" (MLS #{m})"))
                    .unwrap_or_default();
                json!({
                    "date": s.get("date"),
                    "price": s.get("price"),
                    "deedType": format!("MLS Closed Sale{mls_s}"),
                    "note": s.get("source"),
                })
            })
            .collect();
        subj_spec_facts.insert("saleHistory".into(), Value::Array(subj_sales));
        subj_spec_facts.insert("titleAndLiens".into(), json!({ "transfers": transfers }));
    }
    if let Some(mls_l) = subj_active_mls {
        subj_spec_facts.insert("mlsListing".into(), mls_l);
    }

    // Filter & rank sold comps by asset class compatibility, recency (<=18m preferred, <=30m fallback), and proximity
    let target_class = subj_asset_class;
    let class_filtered: Vec<(i32, f64, Value)> = if let Some(ref tc) = target_class {
        let matching: Vec<(i32, f64, Value)> = sold_candidates
            .iter()
            .filter(|(_, _, ac, _)| ac.as_deref() == Some(tc.as_str()))
            .map(|(m, d, _, v)| (*m, *d, v.clone()))
            .collect();
        if matching.len() >= 3 {
            matching
        } else {
            sold_candidates
                .into_iter()
                .map(|(m, d, _, v)| (m, d, v))
                .collect()
        }
    } else {
        sold_candidates
            .into_iter()
            .map(|(m, d, _, v)| (m, d, v))
            .collect()
    };

    let primary: Vec<(i32, f64, Value)> = class_filtered
        .iter()
        .filter(|(m, d, _)| *m <= 18 && *d <= 0.75)
        .cloned()
        .collect();
    let mut chosen = if primary.len() >= 4 {
        primary
    } else {
        let secondary: Vec<(i32, f64, Value)> = class_filtered
            .iter()
            .filter(|(m, d, _)| *m <= 30 && *d <= 1.25)
            .cloned()
            .collect();
        if secondary.len() >= 3 {
            secondary
        } else {
            class_filtered
        }
    };
    chosen.sort_by(|a, b| {
        let sa = a.1 * 2.2 + (a.0 as f64) * 0.055;
        let sb = b.1 * 2.2 + (b.0 as f64) * 0.055;
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    });
    let sold_comps: Vec<Value> = chosen.into_iter().take(12).map(|(_, _, v)| v).collect();

    active_candidates.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    let active_listings: Vec<Value> = active_candidates
        .into_iter()
        .take(16)
        .map(|(_, _, v)| v)
        .collect();

    PublicGisParsedAll {
        subject_facts: subj_spec_facts,
        sold_comps,
        active_listings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_public_gis_active_csv_filters_subject_and_sorts_by_distance() {
        let sample_csv = concat!(
            "SALE TYPE,SOLD DATE,PROPERTY TYPE,ADDRESS,CITY,STATE OR PROVINCE,ZIP OR POSTAL CODE,PRICE,BEDS,BATHS,LOCATION,SQUARE FEET,LOT SIZE,YEAR BUILT,DAYS ON MARKET,$/SQUARE FEET,HOA/MONTH,STATUS,NEXT OPEN HOUSE START TIME,NEXT OPEN HOUSE END TIME,URL (SEE https://mls.example/cma FOR INFO ON PRICING),SOURCE,MLS#,FAVORITE,INTERESTED,LATITUDE,LONGITUDE\n",
            "\"In accordance with local MLS rules, some MLS listings are not included in the download\",,,,,,,,,,,,,,,,,,,,,,,,,,\n",
            "MLS Listing,,Single Family Residential,100 Synthparcelalpha Ln,San Jose,CA,95118,2150000,4,2.5,Synthetic District,2100,6600,1976,6,1024,,Active,,,https://mls.example/listing/synth-100,SynthMLS,SYNTH-MLS-100,N,Y,37.252000,-121.892000\n",
            "MLS Listing,,Single Family Residential,122 Synthactivefar Dr,San Jose,CA,95118,2020000,4,2.0,Synthetic District,1740,6100,1974,11,1161,,Coming Soon,,,https://mls.example/listing/synth-122,SynthMLS,SYNTH-MLS-122,N,Y,37.254300,-121.890100\n",
            "MLS Listing,,Single Family Residential,108 Synthactivenear Ct,San Jose,CA,95118,1820000,3,2.0,Synthetic District,1480,6050,1975,4,1230,,Active,,,https://mls.example/listing/synth-108,SynthMLS,SYNTH-MLS-108,N,Y,37.251200,-121.891100\n",
            "PAST SALE,October-17-2025,Single Family Residential,140 Synthclosedsold Rd,San Jose,CA,95118,1860000,5,2.5,Synthetic District,2140,7400,1973,,869,,Sold,,,https://mls.example/listing/synth-140,SynthMLS,SYNTH-MLS-140,N,Y,37.253500,-121.889900\n",
            "MLS Listing,,Townhouse,910 Synthdistant Walk,Palo Alto,CA,94303,1650000,3,2.5,Palo Alto,1460,950,2009,9,1130,485,Active,,,https://mls.example/listing/synth-910,SynthMLS,SYNTH-MLS-910,N,Y,37.424500,-122.103500\n"
        );
        let geo = json!({
            "displayAddress": "100 SYNTHPARCELALPHA LN, SAN JOSE, CA, 95118",
            "city": "San Jose",
            "state": "CA",
            "zip": "95118",
            "lat": 37.252000,
            "lon": -121.892000,
        });
        let rows = parse_public_gis_active_csv(
            sample_csv,
            "100 Synthparcelalpha Ln, San Jose, CA 95118",
            &geo,
            37.252000,
            -121.892000,
        );
        // Excludes disclaimer row, subject, closed PAST SALE, and >2.5 mi distant listing.
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0]["address"],
            json!("108 Synthactivenear Ct, San Jose, CA 95118")
        );
        assert_eq!(rows[0]["status"], json!("Active"));
        assert_eq!(rows[0]["mlsNumber"], json!("SYNTH-MLS-108"));
        assert_eq!(
            rows[1]["address"],
            json!("122 Synthactivefar Dr, San Jose, CA 95118")
        );
        assert_eq!(rows[1]["status"], json!("Coming Soon"));
        assert!(rows[0]["distanceMi"].as_f64().unwrap() < rows[1]["distanceMi"].as_f64().unwrap());
    }

    #[test]
    fn test_parse_portal_date_iso_formats() {
        assert_eq!(
            parse_portal_date_iso("October-17-2025"),
            Some("2025-10-17".to_string())
        );
        assert_eq!(
            parse_portal_date_iso("June-14-2022"),
            Some("2022-06-14".to_string())
        );
        assert_eq!(
            parse_portal_date_iso("3/5/2025"),
            Some("2025-03-05".to_string())
        );
        assert_eq!(
            parse_portal_date_iso("2025-11-26T12:00:00Z"),
            Some("2025-11-26".to_string())
        );
        assert_eq!(parse_portal_date_iso(""), None);
    }

    #[test]
    fn test_parse_public_gis_csv_all_extracts_subject_history_and_real_sold_comps() {
        let sample_csv = concat!(
            "SALE TYPE,SOLD DATE,PROPERTY TYPE,ADDRESS,CITY,STATE OR PROVINCE,ZIP OR POSTAL CODE,PRICE,BEDS,BATHS,LOCATION,SQUARE FEET,LOT SIZE,YEAR BUILT,DAYS ON MARKET,$/SQUARE FEET,HOA/MONTH,STATUS,NEXT OPEN HOUSE START TIME,NEXT OPEN HOUSE END TIME,URL (SEE https://mls.example/cma FOR INFO ON PRICING),SOURCE,MLS#,FAVORITE,INTERESTED,LATITUDE,LONGITUDE\n",
            "PAST SALE,June-14-2022,Single Family Residential,200 Synthparcelbeta Ave,Campbell,CA,95008,2180000,4,2.5,Campbell,2200,9200,1938,,991,,Sold,,,https://mls.example/listing/synth-200,SynthMLS,SYNTH-MLS-200,N,Y,37.281000,-121.952000\n",
            "PAST SALE,April-25-2025,Single Family Residential,214 Synthcomp01 Ave,Campbell,CA,95008,2420000,4,3.0,Campbell,2400,7500,2000,,1008,,Sold,,,https://mls.example/listing/synth-214,SynthMLS,SYNTH-MLS-214,N,Y,37.281400,-121.952500\n",
            "PAST SALE,March-12-2025,Single Family Residential,228 Synthcomp02 Ave,Campbell,CA,95008,2880000,5,3.5,Campbell,2720,8600,2004,,1059,,Sold,,,https://mls.example/listing/synth-228,SynthMLS,SYNTH-MLS-228,N,Y,37.281900,-121.951900\n",
            "PAST SALE,June-20-2025,Single Family Residential,242 Synthcomp03 Ave,Campbell,CA,95008,2590000,5,3.0,Campbell,2130,9300,1952,,1216,,Sold,,,https://mls.example/listing/synth-242,SynthMLS,SYNTH-MLS-242,N,Y,37.283800,-121.952600\n",
            "PAST SALE,May-19-2025,Multi-Family (2-4 Unit),290 Synthduplex Ave,Campbell,CA,95008,2250000,,4.0,Campbell,2160,10200,1969,,1042,,Sold,,,https://mls.example/listing/synth-290,SynthMLS,SYNTH-MLS-290,N,Y,37.276600,-121.952600\n"
        );
        let geo = json!({
            "displayAddress": "200 SYNTHPARCELBETA AVE, CAMPBELL, CA, 95008",
            "city": "Campbell",
            "state": "CA",
            "zip": "95008",
            "lat": 37.281000,
            "lon": -121.952000,
        });
        let parsed = parse_public_gis_csv_all(
            sample_csv,
            "200 Synthparcelbeta Ave, Campbell, CA 95008",
            &geo,
            37.281000,
            -121.952000,
        );
        assert_eq!(
            parsed.subject_facts.get("lastSale"),
            Some(&json!({ "date": "2022-06-14", "price": 2_180_000 }))
        );
        assert_eq!(parsed.sold_comps.len(), 3);
        assert_eq!(
            parsed.sold_comps[0]["address"],
            json!("214 Synthcomp01 Ave")
        );
        assert_eq!(parsed.sold_comps[0]["soldDate"], json!("2025-04-25"));
        assert_eq!(parsed.sold_comps[0]["price"], json!(2_420_000));
        assert_eq!(parsed.sold_comps[0]["listPrice"], Value::Null);
        assert_eq!(parsed.sold_comps[0]["mlsNumber"], json!("SYNTH-MLS-214"));
        assert_eq!(parsed.sold_comps[0]["isSynthetic"], json!(false));
    }
}
