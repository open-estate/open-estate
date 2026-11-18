// Hazard-Indexed Dwelling Replacement Cost (Coverage A) & Insurance Shock Modeling Engine.
//
// Models US residential insurability, state last-resort pools (California FAIR Plan + DIC,
// Florida Citizens, Texas TWIA, Louisiana Citizens, coastal wind pools), convective hail/tornado
// loading, NFIP Risk Rating 2.0 + private excess flood, and CEA / private seismic policies.
// Explicitly separates Dwelling Reconstruction Cost (Coverage A) from Land + Market Value and
// quantifies `monthlyPaymentShockUSD` relative to naive 0.35% lender/portal calculators.

use crate::modules::fire_protection::{
    evaluate_lidar_canopy_overhang, fuse_parcel_fire_protection, FIRE_PROTECTION_EXT_ID,
    FIRE_PROTECTION_HOMEPAGE_URL, FIRE_PROTECTION_SOURCE_NAME,
};
use crate::registry::rates_and_costs::{
    state_admitted_ho3_rate as state_admitted_base_rate, state_base_rebuild_per_sqft,
    CONVECTIVE_HAIL_STATES, COUNTY_REBUILD_COST_SQFT, TIER1_COASTAL_WIND_FIPS,
};
use crate::shared::{round_n, unwrap_data, EnvConfig, HttpClient, ModuleOutput, SourceItem};
use chrono::Datelike;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;

static RE_YEAR_4D: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(19[5-9]\d|20[0-3]\d)\b").unwrap());
static RE_REROOF_TEXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(re-?roof|reroof|new\s+roof|roof\s+replac|roof\s+tear[- ]?off|comp\s+shingle\s+roof\s+installed)\b")
        .unwrap()
});
static RE_EXPLICIT_REROOF_ACTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(re-?roof|reroof|roof\s+replac|roof\s+tear[- ]?off|new\s+roof)\b")
        .unwrap()
});
static RE_NON_REROOF_PERMIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(solar|photovoltaic|pv|inverter|battery|storage\s+system|ev\s+charger|skylight|hvac|heat\s+pump|condenser|water\s+heater)\b")
        .unwrap()
});
static RE_SEISMIC_TEXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(seismic|cripple\s*wall|brace\s*(?:and|&|\+)\s*bolt|foundation\s+bolt|anchor\s+bolt|soft[- ]story\s+retrofit|earthquake\s+retrofit)\b")
        .unwrap()
});
static RE_HILLSIDE_TEXT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(hillside|canyon|steep\s+slope|upslope|downslope|ridge[- ]?top|cantilever|stilt|caisson)\b")
        .unwrap()
});

fn extract_nri_score(haz: &Value, code: &str) -> Option<f64> {
    if let Some(arr) = haz.pointer("/nri/topHazards").and_then(|v| v.as_array()) {
        for item in arr {
            if item.get("code").and_then(|v| v.as_str()) == Some(code) {
                if let Some(sc) = item.get("score").and_then(|v| v.as_f64()) {
                    return Some(sc);
                }
            }
        }
    }
    haz.pointer(&format!("/nri/hazards/{code}/score"))
        .and_then(|v| v.as_f64())
}

fn extract_year_from_str(s: &str, min_yr: i32, max_yr: i32) -> Option<i32> {
    RE_YEAR_4D
        .captures_iter(s)
        .filter_map(|cap| cap.get(1)?.as_str().parse::<i32>().ok())
        .filter(|&y| y >= min_yr && y <= max_yr)
        .max()
}

#[derive(Debug, Clone)]
pub(crate) struct RoofLifecycleSnapshot {
    pub effective_age_years: Option<i32>,
    pub has_reroof_permit: bool,
    pub latest_reroof_year: Option<i32>,
    pub verification_status: &'static str,
    pub effective_year_source: &'static str,
    pub estimated_age_from_year_built: bool,
}

#[derive(Debug, Clone)]
struct RoofPermitContext {
    effective_age_years: Option<i32>,
    has_reroof_permit: bool,
    latest_reroof_year: Option<i32>,
    material_class: &'static str,
    has_seismic_retrofit: bool,
    verification_status: &'static str,
    effective_year_source: &'static str,
    estimated_age_from_year_built: bool,
}

/// Returns `true` strictly for verified/finaled subject-parcel reroof permits, excluding Solar PV,
/// HVAC, skylight, parent-lot, and unfinaled/unverified permit records.
pub(crate) fn is_verified_reroof_permit(p: &Value) -> bool {
    if p.get("isParentLot").and_then(|v| v.as_bool()) == Some(true)
        || p.get("scope").and_then(|v| v.as_str()) == Some("parent_lot")
        || p.get("verified").and_then(|v| v.as_bool()) == Some(false)
        || p.get("finaled").and_then(|v| v.as_bool()) == Some(false)
    {
        return false;
    }
    let cat = p
        .get("category")
        .or_else(|| p.get("type"))
        .or_else(|| p.get("workType"))
        .or_else(|| p.get("rawType"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let cat_low = cat.trim().to_ascii_lowercase();
    if matches!(
        cat_low.as_str(),
        "solar"
            | "photovoltaic"
            | "pv"
            | "electrical"
            | "hvac"
            | "mechanical"
            | "plumbing"
            | "new_construction"
            | "adu"
            | "pool"
            | "demolition"
            | "planning"
            | "code_enforcement"
    ) {
        return false;
    }
    let desc = p
        .get("description")
        .or_else(|| p.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let combined = format!("{cat} {desc}");
    if RE_NON_REROOF_PERMIT.is_match(&combined) && !RE_EXPLICIT_REROOF_ACTION.is_match(&combined) {
        return false;
    }
    matches!(
        cat_low.as_str(),
        "reroof" | "re-roof" | "roof_replacement" | "roofing"
    ) || RE_REROOF_TEXT.is_match(&combined)
}

pub(crate) fn resolve_roof_lifecycle(
    facts: &Value,
    condition: Option<&Value>,
    yb_opt: Option<i32>,
    material_class: &str,
    now_year: i32,
) -> RoofLifecycleSnapshot {
    let min_reroof_yr = yb_opt.unwrap_or(1940);
    let mut latest_reroof_year: Option<i32> = facts
        .get("roofYear")
        .and_then(|v| v.as_i64())
        .map(|y| y as i32)
        .filter(|&y| y >= min_reroof_yr && y <= now_year);
    let mut has_reroof_permit = latest_reroof_year.is_some();
    let mut stage2_new_const_year: Option<i32> = None;

    if let Some(cond) = condition.map(unwrap_data) {
        let roof_obj = cond.pointer("/systems/roof");
        let rep_obj = cond.pointer("/roofReplacement");
        let stg2_verified = roof_obj
            .and_then(|r| r.get("reroofVerified"))
            .and_then(|v| v.as_bool())
            .or_else(|| {
                rep_obj
                    .and_then(|r| r.get("reroofVerified"))
                    .and_then(|v| v.as_bool())
            })
            .or_else(|| {
                roof_obj
                    .and_then(|r| r.get("replacementDateVerified"))
                    .and_then(|v| v.as_bool())
            });
        let stg2_status = roof_obj
            .and_then(|r| r.get("verificationStatus"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                rep_obj
                    .and_then(|r| r.get("verificationStatus"))
                    .and_then(|v| v.as_str())
            });
        let stg2_eff_src = roof_obj
            .and_then(|r| r.get("effectiveYearSource"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                rep_obj
                    .and_then(|r| r.get("effectiveYearSource"))
                    .and_then(|v| v.as_str())
            });

        if stg2_verified == Some(true)
            || stg2_status == Some("verified_reroof")
            || stg2_eff_src == Some("verified_reroof_permit")
        {
            has_reroof_permit = true;
        } else if stg2_verified.is_none()
            && stg2_status.is_none()
            && roof_obj
                .and_then(|r| r.get("permitVerified"))
                .and_then(|v| v.as_bool())
                == Some(true)
        {
            has_reroof_permit = true;
        }

        if let Some(ry) = roof_obj
            .and_then(|r| r.get("verifiedReroofYear"))
            .or_else(|| rep_obj.and_then(|r| r.get("verifiedReroofYear")))
            .and_then(|v| v.as_i64())
            .map(|y| y as i32)
            .filter(|&y| y >= min_reroof_yr && y <= now_year)
        {
            has_reroof_permit = true;
            latest_reroof_year = Some(latest_reroof_year.map_or(ry, |cur| cur.max(ry)));
        } else if has_reroof_permit {
            if let Some(ry) = roof_obj
                .and_then(|r| {
                    r.get("effectiveYear")
                        .or_else(|| r.get("installedOrEffectiveYear"))
                })
                .and_then(|v| v.as_i64())
                .map(|y| y as i32)
                .filter(|&y| y >= min_reroof_yr && y <= now_year)
            {
                latest_reroof_year = Some(latest_reroof_year.map_or(ry, |cur| cur.max(ry)));
            }
        }

        if stg2_status == Some("new_construction")
            || stg2_eff_src == Some("new_construction_permit")
        {
            stage2_new_const_year = roof_obj
                .and_then(|r| {
                    r.get("newConstructionPermitYear")
                        .or_else(|| r.get("effectiveYear"))
                })
                .or_else(|| rep_obj.and_then(|r| r.get("newConstructionPermitYear")))
                .and_then(|v| v.as_i64())
                .map(|y| y as i32)
                .filter(|&y| y >= 1940 && y <= now_year);
        }
    }

    let mut all_permits: Vec<&Value> = Vec::new();
    if let Some(arr) = facts.get("permits").and_then(|v| v.as_array()) {
        all_permits.extend(arr.iter());
    }
    if let Some(arr) = facts
        .pointer("/buildingRegister/permits")
        .and_then(|v| v.as_array())
    {
        all_permits.extend(arr.iter());
    }
    if let Some(cond) = condition.map(unwrap_data) {
        if let Some(arr) = cond.get("permits").and_then(|v| v.as_array()) {
            all_permits.extend(arr.iter());
        }
    }
    for p in all_permits {
        if !is_verified_reroof_permit(p) {
            continue;
        }
        let cat = p
            .get("category")
            .or_else(|| p.get("type"))
            .or_else(|| p.get("workType"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let desc = p
            .get("description")
            .or_else(|| p.get("summary"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let date_str = p
            .get("finalDate")
            .or_else(|| p.get("issueDate"))
            .or_else(|| p.get("date"))
            .or_else(|| p.get("year"))
            .map(|v| {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else if let Some(n) = v.as_i64() {
                    n.to_string()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        let combined = format!("{cat} {desc}");
        has_reroof_permit = true;
        if let Some(y) = extract_year_from_str(&date_str, 1950, now_year)
            .or_else(|| extract_year_from_str(&combined, 1950, now_year))
        {
            latest_reroof_year = Some(latest_reroof_year.map_or(y, |cur| cur.max(y)));
        }
    }

    let explicit_age = facts
        .get("roofAgeYears")
        .and_then(|v| v.as_i64())
        .map(|a| a as i32)
        .filter(|&a| (0..=90).contains(&a));

    let (
        effective_age_years,
        verification_status,
        effective_year_source,
        estimated_age_from_year_built,
    ) = if let Some(age) = explicit_age {
        (
            Some(age),
            if has_reroof_permit {
                "verified_reroof"
            } else {
                "unverified"
            },
            "explicit_roof_age",
            false,
        )
    } else if let Some(ry) = latest_reroof_year {
        (
            Some((now_year - ry).max(0)),
            "verified_reroof",
            "verified_reroof_permit",
            false,
        )
    } else if let Some(ncy) = stage2_new_const_year {
        (
            Some((now_year - ncy).max(0)),
            "new_construction",
            "new_construction_permit",
            false,
        )
    } else {
        let est = yb_opt.map(|yb| {
            let struct_age = (now_year - yb).max(0);
            let cycle = match material_class {
                "tile_or_slate" | "metal" => 42,
                "wood_shake" => 22,
                "flat_membrane" => 18,
                _ => 25,
            };
            if struct_age <= cycle + 4 {
                struct_age
            } else {
                match material_class {
                    "tile_or_slate" | "metal" => 26,
                    _ => 21,
                }
            }
        });
        (
            est,
            if has_reroof_permit {
                "verified_reroof"
            } else {
                "unverified"
            },
            if est.is_some() {
                "year_built_unverified"
            } else {
                "unknown"
            },
            est.is_some(),
        )
    };

    RoofLifecycleSnapshot {
        effective_age_years,
        has_reroof_permit,
        latest_reroof_year,
        verification_status,
        effective_year_source,
        estimated_age_from_year_built,
    }
}

fn inspect_roof_and_seismic_context(
    facts: &Value,
    condition: Option<&Value>,
    now_year: i32,
) -> RoofPermitContext {
    let yb_opt = facts
        .get("yearBuilt")
        .or_else(|| facts.pointer("/countyRecords/buildingRecord/yearBuilt"))
        .and_then(|v| v.as_i64())
        .map(|y| y as i32)
        .filter(|&y| y >= 1750 && y <= now_year);

    let roof_raw = facts
        .get("roof")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let material_class = if roof_raw.contains("wood") || roof_raw.contains("shake") {
        "wood_shake"
    } else if roof_raw.contains("tile") || roof_raw.contains("clay") || roof_raw.contains("slate") {
        "tile_or_slate"
    } else if roof_raw.contains("metal") || roof_raw.contains("standing seam") {
        "metal"
    } else if roof_raw.contains("class 4") || roof_raw.contains("impact") {
        "impact_class4"
    } else if roof_raw.contains("flat")
        || roof_raw.contains("tar")
        || roof_raw.contains("foam")
        || roof_raw.contains("tpo")
    {
        "flat_membrane"
    } else {
        "composition_shingle"
    };

    let lifecycle = resolve_roof_lifecycle(facts, condition, yb_opt, material_class, now_year);
    let mut has_seismic_retrofit = facts
        .get("seismicRetrofitted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(cond) = condition.map(unwrap_data) {
        if cond
            .pointer("/systems/foundationDrainage/permitVerified")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            has_seismic_retrofit = true;
        }
    }

    let mut all_permits: Vec<&Value> = Vec::new();
    if let Some(arr) = facts.get("permits").and_then(|v| v.as_array()) {
        all_permits.extend(arr.iter());
    }
    if let Some(arr) = facts
        .pointer("/buildingRegister/permits")
        .and_then(|v| v.as_array())
    {
        all_permits.extend(arr.iter());
    }
    if let Some(cond) = condition.map(unwrap_data) {
        if let Some(arr) = cond.get("permits").and_then(|v| v.as_array()) {
            all_permits.extend(arr.iter());
        }
    }
    for p in all_permits {
        let cat = p
            .get("category")
            .or_else(|| p.get("type"))
            .or_else(|| p.get("workType"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let desc = p
            .get("description")
            .or_else(|| p.get("summary"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let combined = format!("{cat} {desc}");
        if cat.eq_ignore_ascii_case("seismic") || RE_SEISMIC_TEXT.is_match(&combined) {
            has_seismic_retrofit = true;
        }
    }

    for note_ptr in ["/permitNotes", "/remarks", "/mlsListing/remarks"] {
        if let Some(txt) = facts.pointer(note_ptr).and_then(|v| v.as_str()) {
            if RE_SEISMIC_TEXT.is_match(txt) {
                has_seismic_retrofit = true;
            }
        }
    }

    RoofPermitContext {
        effective_age_years: lifecycle.effective_age_years,
        has_reroof_permit: lifecycle.has_reroof_permit,
        latest_reroof_year: lifecycle.latest_reroof_year,
        material_class,
        has_seismic_retrofit,
        verification_status: lifecycle.verification_status,
        effective_year_source: lifecycle.effective_year_source,
        estimated_age_from_year_built: lifecycle.estimated_age_from_year_built,
    }
}

fn resolve_reference_market_value(
    facts: &Value,
    explicit_market_usd: Option<i64>,
) -> Option<(i64, &'static str)> {
    if let Some(v) = explicit_market_usd.filter(|&x| x > 25_000) {
        return Some((v, "valuation.estimate"));
    }
    for (key, src) in [
        ("estimatedValue", "facts.estimatedValue"),
        ("listPrice", "facts.listPrice"),
        ("price", "facts.price"),
    ] {
        if let Some(v) = facts
            .get(key)
            .and_then(|v| v.as_f64())
            .filter(|&x| x > 25_000.0)
        {
            return Some((v.round() as i64, src));
        }
    }
    if let Some(avms) = facts.get("avms") {
        let mut vals: Vec<f64> = Vec::new();
        if let Some(arr) = avms.as_array() {
            vals.extend(
                arr.iter()
                    .filter_map(|a| a.get("value").and_then(|v| v.as_f64()))
                    .filter(|&x| x > 25_000.0),
            );
        } else if let Some(obj) = avms.as_object() {
            for v in obj.values() {
                if let Some(num) = v
                    .get("value")
                    .and_then(|x| x.as_f64())
                    .or_else(|| v.as_f64())
                    .filter(|&x| x > 25_000.0)
                {
                    vals.push(num);
                }
            }
        }
        if !vals.is_empty() {
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            return Some((vals[vals.len() / 2].round() as i64, "facts.avms"));
        }
    }
    if let Some(lp) = facts
        .pointer("/lastSale/price")
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 25_000.0)
    {
        return Some((lp.round() as i64, "facts.lastSale"));
    }
    if let Some(comps) = facts.get("comps").and_then(|v| v.as_array()) {
        let mut vals: Vec<f64> = comps
            .iter()
            .filter_map(|c| {
                c.get("price")
                    .or_else(|| c.get("soldPrice"))
                    .and_then(|v| v.as_f64())
            })
            .filter(|&x| x > 25_000.0)
            .collect();
        if !vals.is_empty() {
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            return Some((vals[vals.len() / 2].round() as i64, "facts.comps"));
        }
    }
    None
}

/// Deterministic Hazard-Indexed Replacement Cost & Insurance Shock Engine.
pub fn run_insurance(
    facts: &Value,
    hazards: &Value,
    environment: &Value,
    geo: &Value,
) -> ModuleOutput {
    let f = unwrap_data(facts);
    run_insurance_with_condition(f, hazards, environment, geo, f.get("condition"), None)
}

/// Queries `fire_protection_hydrants` through `ExtensionHub::global().execute_enabled_extension(...)`
/// (with `geohash6` shared cache, rate limiting, and admin enablement) and fuses parcel-exact
/// distances, 3DEP LiDAR `canopyNearRoof` overhang, and `permit_roof_extractor` reroof cliff checks.
pub async fn fetch_fire_protection_via_hub(
    http: &HttpClient,
    env: &EnvConfig,
    facts: &Value,
    hazards: &Value,
    environment: &Value,
    geo: &Value,
    condition: Option<&Value>,
    house3d: Option<&Value>,
) -> Option<Value> {
    fetch_fire_protection_via_hub_ref(
        crate::extensions::ExtensionHub::global(),
        http,
        env,
        facts,
        hazards,
        environment,
        geo,
        condition,
        house3d,
    )
    .await
}

/// Variant of [`fetch_fire_protection_via_hub`] accepting an explicit [`crate::extensions::ExtensionHub`] handle.
pub async fn fetch_fire_protection_via_hub_ref(
    hub: &crate::extensions::ExtensionHub,
    http: &HttpClient,
    env: &EnvConfig,
    facts: &Value,
    hazards: &Value,
    environment: &Value,
    geo: &Value,
    condition: Option<&Value>,
    house3d: Option<&Value>,
) -> Option<Value> {
    let f = unwrap_data(facts);
    let h = unwrap_data(hazards);
    let e = unwrap_data(environment);
    let g = unwrap_data(geo);

    let lat = g
        .pointer("/refined/lat")
        .or_else(|| g.get("lat"))
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite() && x.abs() <= 90.0)?;
    let lon = g
        .pointer("/refined/lon")
        .or_else(|| g.get("lon"))
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite() && x.abs() <= 180.0)?;

    let mut extra_params = HashMap::new();
    if let Some(country) = g
        .get("country")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        extra_params.insert("country".to_string(), country.to_string());
    }
    let lidar_eval = evaluate_lidar_canopy_overhang(house3d, f, h, condition);
    if let Some(canopy_b) = lidar_eval.get("canopyNearRoof").and_then(Value::as_bool) {
        extra_params.insert("canopy_near_roof".to_string(), canopy_b.to_string());
    }

    let sdwis_pwsid = e
        .pointer("/water/primary/pwsid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let sdwis_water_name = e
        .pointer("/water/primary/name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let is_private_well = f.get("privateWell").and_then(Value::as_bool);
    let yb_opt = f
        .get("yearBuilt")
        .or_else(|| f.pointer("/countyRecords/buildingRecord/yearBuilt"))
        .and_then(Value::as_i64);
    let sqft_opt = f
        .get("sqft")
        .or_else(|| f.get("livingAreaSqft"))
        .and_then(Value::as_i64);

    let st = g
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    let county_fips = g.get("countyFips").and_then(Value::as_str).unwrap_or("");
    let nri_hrcn = extract_nri_score(h, "HRCN").unwrap_or(0.0);
    let is_coastal_wind =
        TIER1_COASTAL_WIND_FIPS.contains(county_fips) || st == "FL" || nri_hrcn >= 82.0;

    let query = crate::extensions::proto::ExtensionQueryProto {
        lat: Some(lat),
        lon: Some(lon),
        state: st,
        city: g.get("city").and_then(Value::as_str).unwrap_or("").to_string(),
        zip: g.get("zip").and_then(Value::as_str).unwrap_or("").to_string(),
        county_fips: county_fips.to_string(),
        county_name: g.get("county").and_then(Value::as_str).map(String::from),
        year_built: yb_opt,
        sqft: sqft_opt,
        sdwis_pwsid,
        sdwis_water_name,
        is_likely_private_well: is_private_well,
        extra_params,
        ..Default::default()
    };

    let envs = match hub
        .execute_enabled_extension(FIRE_PROTECTION_EXT_ID, &query, http, env)
        .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "insurance: fire_protection_hydrants lookup unavailable");
            return None;
        }
    };

    for env_msg in envs {
        if env_msg.extension_id != FIRE_PROTECTION_EXT_ID {
            continue;
        }
        if let Some(crate::extensions::proto::extension_envelope_proto::Payload::RawJson(raw)) =
            env_msg.payload
        {
            if let Ok(cell_val) = serde_json::from_str::<Value>(&raw) {
                let now_year = chrono::Utc::now().year();
                return Some(fuse_parcel_fire_protection(
                    &cell_val,
                    Some(lat),
                    Some(lon),
                    f,
                    h,
                    e,
                    condition,
                    house3d,
                    is_coastal_wind,
                    now_year,
                ));
            }
        }
    }
    None
}

/// Async entry point executing `fire_protection_hydrants` via `ExtensionHub::global()` and enriching
/// insurance underwriting, brush-clearance/LiDAR canopy overhang, ISO PPC proxy, and map layers.
pub async fn run_insurance_with_fire_protection(
    http: &HttpClient,
    env: &EnvConfig,
    facts: &Value,
    hazards: &Value,
    environment: &Value,
    geo: &Value,
    condition: Option<&Value>,
    house3d: Option<&Value>,
    market_estimate_usd: Option<i64>,
) -> ModuleOutput {
    let fire_prot = fetch_fire_protection_via_hub(
        http,
        env,
        facts,
        hazards,
        environment,
        geo,
        condition,
        house3d,
    )
    .await;
    run_insurance_with_signals(
        facts,
        hazards,
        environment,
        geo,
        condition,
        house3d,
        fire_prot.as_ref(),
        market_estimate_usd,
    )
}

/// Extended entry point accepting optional pre-computed `condition` triage and `valuation` market estimate.
pub fn run_insurance_with_condition(
    facts: &Value,
    hazards: &Value,
    environment: &Value,
    geo: &Value,
    condition: Option<&Value>,
    market_estimate_usd: Option<i64>,
) -> ModuleOutput {
    run_insurance_with_signals(
        facts,
        hazards,
        environment,
        geo,
        condition,
        None,
        None,
        market_estimate_usd,
    )
}

/// Full deterministic + extension-enriched insurance modeling entry point consuming optional
/// `house3d` LiDAR canopy metrics and `fire_protection_hydrants` signals.
#[allow(clippy::too_many_arguments)]
pub fn run_insurance_with_signals(
    facts: &Value,
    hazards: &Value,
    environment: &Value,
    geo: &Value,
    condition: Option<&Value>,
    house3d: Option<&Value>,
    fire_protection: Option<&Value>,
    market_estimate_usd: Option<i64>,
) -> ModuleOutput {
    let f = unwrap_data(facts);
    let h = unwrap_data(hazards);
    let e = unwrap_data(environment);
    let g = unwrap_data(geo);

    let now_year = chrono::Utc::now().year();
    let st = g
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    let county_fips = g.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let county_name = g.get("county").and_then(|v| v.as_str()).unwrap_or("");

    // =========================================================================
    // 1. Dwelling Reconstruction Cost Estimate (Coverage A)
    // =========================================================================
    let sqft_opt = f
        .get("sqft")
        .or_else(|| f.get("livingAreaSqft"))
        .or_else(|| f.pointer("/buildingRegister/livingSqft"))
        .or_else(|| f.pointer("/countyRecords/buildingRecord/livingAreaSqft"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 150.0);
    let stories_opt = f
        .get("stories")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            f.pointer("/buildingRegister/stories")
                .and_then(|v| v.as_f64())
        })
        .filter(|&s| s >= 1.0);
    let stories = stories_opt.unwrap_or(1.0);
    let yb_opt = f
        .get("yearBuilt")
        .or_else(|| f.pointer("/countyRecords/buildingRecord/yearBuilt"))
        .and_then(|v| v.as_i64())
        .map(|y| y as i32)
        .filter(|&y| y >= 1700 && y <= now_year + 1);
    let pt_lc = f
        .get("propertyType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_condo = pt_lc.contains("condo") || pt_lc.contains("co-op") || pt_lc.contains("coop");

    let ref_market_opt = resolve_reference_market_value(f, market_estimate_usd);

    let Some(sqft) = sqft_opt else {
        return ModuleOutput {
            data: json!({
                "status": "missing_required_input",
                "missingInputs": ["sqft"],
                "insurabilityTier": Value::Null,
                "insurabilityScore": Value::Null,
                "coverageAUSD": Value::Null,
                "dwellingReconstruction": {
                    "status": "missing_required_input",
                    "coverageAUSD": Value::Null,
                    "livingAreaSqft": Value::Null,
                    "stories": stories_opt,
                    "yearBuilt": yb_opt,
                    "isCondoWallsIn": is_condo,
                    "referenceMarketValueUSD": ref_market_opt.map(|(v, _)| v),
                    "referenceMarketValueSource": ref_market_opt.map(|(_, s)| s),
                    "impliedLandResidualUSD": Value::Null,
                    "rebuildToMarketRatioPct": Value::Null,
                    "reconstructionVsMarketNote": "Living area square footage is absent from public property records; Dwelling Reconstruction Cost (Coverage A) is omitted rather than invented."
                },
                "annualPremiumRangeUSD": Value::Null,
                "perilBreakdown": Value::Null,
                "naiveLenderEstimateUSD": ref_market_opt.map(|(v, _)| ((v as f64) * 0.0035).round() as i64),
                "realisticExpectedUSD": Value::Null,
                "naiveMonthlyUSD": ref_market_opt.map(|(v, _)| (((v as f64) * 0.0035) / 12.0).round() as i64),
                "realisticMonthlyUSD": Value::Null,
                "monthlyPaymentShockUSD": Value::Null,
                "paymentShockNote": "Living area square footage is required to model Coverage A and hazard-indexed insurance.",
                "vulnerabilityFlags": [],
                "mitigationDiscountsAndChecklist": [],
                "disclaimer": "Living area square footage is missing; no Coverage A or insurance premium values were invented."
            }),
            sources: Vec::new(),
            warnings: vec![
                "Missing living area square footage (sqft); Coverage A reconstruction and insurance shock not invented."
                    .to_string(),
            ],
        };
    };

    let gross_sqft = f
        .get("grossSqft")
        .and_then(|v| v.as_f64())
        .filter(|&x| x > sqft);

    let base_cost_per_sqft = COUNTY_REBUILD_COST_SQFT
        .get(county_fips)
        .copied()
        .unwrap_or_else(|| state_base_rebuild_per_sqft(&st));

    let stories_factor = if stories >= 2.5 {
        1.13
    } else if stories >= 1.8 {
        1.06
    } else {
        1.00
    };

    let vintage_code_factor = match yb_opt {
        Some(yb) if yb < 1940 => 1.18, // Plaster/lath, bespoke millwork, full electrical/plumbing/energy code upgrades
        Some(yb) if yb < 1970 => 1.11,
        Some(yb) if yb < 1990 => 1.06,
        Some(yb) if yb < 2010 => 1.02,
        _ => 1.00,
    };

    let cgs_landslide = h
        .pointer("/earthquake/california/landslide/inZone")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || h.pointer("/earthquake/california/landslideSusceptibility/class")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 7;
    let remarks_blob = format!(
        "{} {} {} {}",
        f.get("lotTopography")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        f.get("remarks").and_then(|v| v.as_str()).unwrap_or(""),
        f.pointer("/mlsListing/remarks")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        f.pointer("/localNotes/terrain")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    let is_hillside = cgs_landslide
        || f.get("hillside").and_then(|v| v.as_bool()).unwrap_or(false)
        || RE_HILLSIDE_TEXT.is_match(&remarks_blob);

    let terrain_slope_factor = if cgs_landslide && RE_HILLSIDE_TEXT.is_match(&remarks_blob) {
        1.22
    } else if is_hillside {
        1.18
    } else {
        1.00
    };

    let custom_finish_factor = if sqft >= 4_000.0 {
        1.16
    } else if sqft >= 2_800.0 {
        1.07
    } else {
        1.00
    };

    let garage_str = f
        .get("garage")
        .or_else(|| f.get("garageSpaces"))
        .or_else(|| f.pointer("/buildingRegister/parking"))
        .map(|v| {
            if let Some(s) = v.as_str() {
                s.to_ascii_lowercase()
            } else if let Some(n) = v.as_i64() {
                format!("{n}-car")
            } else if v.as_bool() == Some(true) {
                "2-car".to_string()
            } else {
                String::new()
            }
        })
        .unwrap_or_default();
    let garage_spaces = if garage_str.contains('3') || garage_str.contains("three") {
        3
    } else if garage_str.contains('2')
        || garage_str.contains("two")
        || garage_str.contains("attached")
    {
        2
    } else if garage_str.contains('1')
        || garage_str.contains("one")
        || garage_str.contains("detached")
    {
        1
    } else {
        0
    };
    let regional_index_ratio = (base_cost_per_sqft / 255.0).clamp(0.85, 2.30);
    let garage_allowance_usd = if is_condo {
        0
    } else {
        ((garage_spaces as f64) * 19_000.0 * regional_index_ratio.min(1.75)).round() as i64
    };
    let adu_or_aux_usd = match gross_sqft {
        Some(gs) if !is_condo && gs > sqft + 120.0 => {
            (((gs - sqft) * base_cost_per_sqft * 0.72).round() as i64).max(0)
        }
        _ => 0,
    };

    let shell_share = if is_condo { 0.36 } else { 1.0 };
    let effective_rebuild_per_sqft = base_cost_per_sqft
        * shell_share
        * stories_factor
        * vintage_code_factor
        * terrain_slope_factor
        * custom_finish_factor;

    let raw_coverage_a =
        sqft * effective_rebuild_per_sqft + (garage_allowance_usd as f64) + (adu_or_aux_usd as f64);
    let coverage_a_usd = (((raw_coverage_a / 1_000.0).round() as i64) * 1_000).max(95_000);
    let ordinance_law_10pct_usd = ((coverage_a_usd as f64 * 0.10) / 500.0).round() as i64 * 500;
    let extended_125_usd = ((coverage_a_usd as f64 * 1.25) / 1_000.0).round() as i64 * 1_000;

    let (
        ref_market_usd_opt,
        ref_market_source_opt,
        implied_land_residual_opt,
        rebuild_to_market_pct_opt,
        reconstruction_vs_market_note,
    ) = if let Some((ref_market_usd, ref_market_source)) = ref_market_opt {
        let implied_land_residual_usd = (ref_market_usd - coverage_a_usd).max(0);
        let rebuild_to_market_pct = round_n(
            (coverage_a_usd as f64 / (ref_market_usd.max(1) as f64)) * 100.0,
            1,
        );
        let note = if coverage_a_usd < ref_market_usd {
            format!(
                "Reconstruction Cost (Coverage A: ${}) != Land + Market Value (${}). Homeowners insurance covers rebuilding the physical structure and code-required upgrades at regional labor/material rates (~${}/sq ft effective), NOT the underlying land value (~${} implied land/location residual, {}% rebuild-to-price ratio).",
                fmt_commas(coverage_a_usd),
                fmt_commas(ref_market_usd),
                effective_rebuild_per_sqft.round() as i64,
                fmt_commas(implied_land_residual_usd),
                rebuild_to_market_pct
            )
        } else {
            format!(
                "Reconstruction Cost (Coverage A: ${}) != Land + Market Value (${}) — Coverage A EXCEEDS current market value ({}% rebuild-to-price ratio). Full replacement-cost policies must insure 100% of post-loss contractor & code-upgrade rebuild costs (~${}/sq ft effective) even when purchase price is lower.",
                fmt_commas(coverage_a_usd),
                fmt_commas(ref_market_usd),
                rebuild_to_market_pct,
                effective_rebuild_per_sqft.round() as i64
            )
        };
        (
            Some(ref_market_usd),
            Some(ref_market_source),
            Some(implied_land_residual_usd),
            Some(rebuild_to_market_pct),
            note,
        )
    } else {
        (
            None,
            None,
            None,
            None,
            format!(
                "Reconstruction Cost (Coverage A: ${}) reflects physical structure replacement and code-upgrade rates (~${}/sq ft effective); market value is absent from property records so land residual and rebuild-to-price ratio are omitted rather than invented.",
                fmt_commas(coverage_a_usd),
                effective_rebuild_per_sqft.round() as i64
            ),
        )
    };

    // =========================================================================
    // 2. Peril-by-Peril Risk & Premium Modeling
    // =========================================================================
    let roof_ctx = inspect_roof_and_seismic_context(f, condition, now_year);
    let mut vulnerability_flags: Vec<Value> = Vec::new();
    let mut nonrenewal_pool_triggers: Vec<String> = Vec::new();
    let mut surcharged_triggers: Vec<String> = Vec::new();

    // --- 2A. Wildfire / WUI & State FAIR Plan + DIC ---
    let ca_fhsz_str = h
        .pointer("/wildfire/california/fhsz")
        .or_else(|| h.pointer("/wildfire/zone"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ca_fhsz_class = h
        .pointer("/wildfire/california/fhszClass")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| {
            if ca_fhsz_str.contains("very high") {
                3
            } else if ca_fhsz_str.contains("high") {
                2
            } else if ca_fhsz_str.contains("moderate") {
                1
            } else {
                0
            }
        });
    let ca_resp_area = h
        .pointer("/wildfire/california/responsibilityArea")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let near_fhsz_m = h
        .pointer("/wildfire/california/nearestHighOrVeryHigh/distM")
        .and_then(|v| v.as_f64());

    let whp_label = h
        .pointer("/wildfire/whp/label")
        .or_else(|| h.pointer("/wildfire/risk"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let whp_class = h
        .pointer("/wildfire/whp/class")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| {
            if whp_label.contains("very high") {
                5
            } else if whp_label.contains("high") {
                4
            } else if whp_label.contains("moderate") {
                3
            } else {
                1
            }
        });
    let nri_wfir = extract_nri_score(h, "WFIR")
        .or_else(|| h.pointer("/wildfire/nri/score").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);

    // USFS WHP classes: 1=Very Low..5=Very High; 6=Non-Burnable Urban, 7=Water (exclude >=6!)
    let wui_very_high = ca_fhsz_class >= 3
        || whp_class == 5
        || (ca_fhsz_class == 2 && is_hillside && ca_resp_area.eq_ignore_ascii_case("SRA"));
    let wui_high = !wui_very_high
        && (ca_fhsz_class == 2
            || whp_class == 4
            || nri_wfir >= 88.0
            || (near_fhsz_m.map(|d| d <= 220.0).unwrap_or(false) && is_hillside));
    let wui_moderate = !wui_very_high
        && !wui_high
        && (ca_fhsz_class == 1
            || whp_class == 3
            || nri_wfir >= 65.0
            || near_fhsz_m.map(|d| d <= 650.0).unwrap_or(false));

    if roof_ctx.material_class == "wood_shake" {
        vulnerability_flags.push(json!({
            "id": "wood_shake_roof",
            "severity": "high",
            "title": "Wood Shake / Shingle Roof Material",
            "detail": "Untreated wood shake roofs carry severe ember-ignition & hail vulnerability and trigger automatic non-renewal with most admitted carriers."
        }));
    }

    if wui_very_high || wui_high {
        let pool_lbl = if st == "CA" {
            "California FAIR Plan + Difference-in-Conditions (DIC) wrap"
        } else {
            "State FAIR Plan / Surplus-Lines WUI Brush Market"
        };
        nonrenewal_pool_triggers.push(format!(
            "Wildfire / WUI exposure ({}) routes coverage into {}",
            if wui_very_high {
                "Very High Fire Hazard Severity / WHP Class 5"
            } else {
                "High Fire Hazard Severity / Severe WUI Brush"
            },
            pool_lbl
        ));
        vulnerability_flags.push(json!({
            "id": "wildfire_fair_plan_risk",
            "severity": "high",
            "title": if st == "CA" { "CA FAIR Plan + DIC Wrap Likely Required" } else { "Severe WUI Brush Non-Renewal Risk" },
            "detail": format!(
                "Standard admitted HO-3 carriers routinely moratorium or non-renew parcels in {} brush zones{}. Budget for {} coverage.",
                if wui_very_high { "Very High" } else { "High" },
                if is_hillside { " on hillside/canyon terrain" } else { "" },
                pool_lbl
            )
        }));
    } else if wui_moderate {
        surcharged_triggers.push(
            "Moderate WUI wildfire/brush proximity requires defensible-space underwriting & brush surcharge".into(),
        );
    }

    // --- 2B. Coastal Wind / Hurricane Pool & Roof Age Underwriting ---
    let nri_hrcn = extract_nri_score(h, "HRCN").unwrap_or(0.0);
    let nri_hail = extract_nri_score(h, "HAIL").unwrap_or(0.0);
    let nri_trnd = extract_nri_score(h, "TRND").unwrap_or(0.0);
    let nri_swnd = extract_nri_score(h, "SWND").unwrap_or(0.0);

    let county_lc = county_name.to_ascii_lowercase();
    let coastal_county_name_match = county_fips.is_empty()
        && ((st == "TX"
            && (county_lc.contains("galveston")
                || county_lc.contains("brazoria")
                || county_lc.contains("nueces")
                || county_lc.contains("cameron")
                || county_lc.contains("jefferson")
                || county_lc.contains("aransas")
                || county_lc.contains("matagorda")))
            || (st == "LA"
                && (county_lc.contains("orleans")
                    || county_lc.contains("jefferson")
                    || county_lc.contains("plaquemines")
                    || county_lc.contains("st. bernard")
                    || county_lc.contains("st. tammany")
                    || county_lc.contains("terrebonne")
                    || county_lc.contains("lafourche")
                    || county_lc.contains("cameron"))));
    let is_coastal_wind_county = TIER1_COASTAL_WIND_FIPS.contains(county_fips)
        || coastal_county_name_match
        || st == "FL"
        || nri_hrcn >= 82.0;
    let is_moderate_hurricane = !is_coastal_wind_county && nri_hrcn >= 58.0;

    let roof_age_cutoff = match roof_ctx.material_class {
        "tile_or_slate" | "metal" => 25,
        "flat_membrane" | "wood_shake" => 15,
        _ => {
            if is_coastal_wind_county {
                15
            } else {
                20
            }
        }
    };
    let roof_over_cutoff = roof_ctx
        .effective_age_years
        .map(|age| age > roof_age_cutoff)
        .unwrap_or(false);
    let roof_severe_unpermitted = roof_ctx
        .effective_age_years
        .map(|age| age >= 20)
        .unwrap_or(false)
        && !roof_ctx.has_reroof_permit
        && yb_opt.map(|yb| yb <= now_year - 20).unwrap_or(false);
    let roof_age_label = roof_ctx
        .effective_age_years
        .map(|age| format!("~{age} yrs"))
        .unwrap_or_else(|| "age unverified".to_string());

    let wind_pool_name = match st.as_str() {
        "FL" => "Florida Citizens Property Insurance / Surplus Wind",
        "TX" => "Texas Windstorm Insurance Association (TWIA) + Companion HO",
        "LA" => "Louisiana Citizens Property Insurance (Coastal/FAIR)",
        "NC" => "NC Joint Underwriting Association (Beach/Coastal Wind Pool)",
        "SC" => "SC Wind & Hail Underwriting Association (SCWHUA)",
        "AL" => "Alabama Insurance Underwriting Association (AIUA)",
        "MS" => "Mississippi Windstorm Underwriting Association (MWUA)",
        "MA" => "Massachusetts MPIUA FAIR Plan (Coastal)",
        "NY" => "NYPIUA Coastal Wind / Excess Market",
        _ => "State Coastal Wind Pool / E&S Surplus Market",
    };

    if is_coastal_wind_county
        && (roof_over_cutoff
            || nri_hrcn >= 90.0
            || st == "FL" && county_fips == "12086"
            || st == "FL" && county_fips == "12087")
    {
        nonrenewal_pool_triggers.push(format!(
            "Hurricane / wind-borne debris exposure{} triggers {}",
            if roof_over_cutoff {
                format!(
                    " + {roof_age_label} roof ({})",
                    if roof_ctx.has_reroof_permit {
                        "aged past 15-yr carrier cutoff"
                    } else {
                        "no reroof permit on file"
                    }
                )
            } else {
                String::new()
            },
            wind_pool_name
        ));
        vulnerability_flags.push(json!({
            "id": "coastal_wind_pool_risk",
            "severity": "high",
            "title": format!("{wind_pool_name} Exposure"),
            "detail": format!(
                "Located in a hurricane / wind-borne debris zone{} with estimated roof {roof_age_label} ({}). Expect 2%–5% named-storm deductible and mandatory 4-Point + Wind Mitigation inspection.",
                if !county_name.is_empty() { format!(" ({county_name})") } else { String::new() },
                if roof_ctx.has_reroof_permit { "permitted" } else { "no reroof permit verified" }
            )
        }));
    } else if is_coastal_wind_county || is_moderate_hurricane {
        surcharged_triggers.push(format!(
            "Coastal hurricane / windstorm loading ({})",
            if st == "FL" {
                "FL wind mitigation inspection required"
            } else {
                "2%–3% hurricane/wind deductible typical"
            }
        ));
    }

    if roof_over_cutoff || roof_severe_unpermitted {
        if !is_coastal_wind_county {
            surcharged_triggers.push(format!(
                "Estimated roof {roof_age_label} ({}) triggers carrier roof inspection or Actual Cash Value (ACV) roof schedule",
                if roof_ctx.has_reroof_permit {
                    "aging roof"
                } else {
                    "no reroof permit found"
                }
            ));
        }
        vulnerability_flags.push(json!({
            "id": "roof_age_underwriting_hold",
            "severity": if is_coastal_wind_county { "high" } else { "moderate" },
            "title": format!("Roof Age Underwriting Risk ({roof_age_label})"),
            "detail": format!(
                "Admitted carriers increasingly decline or apply ACV depreciation to {} roofs older than {} years when no recent reroof permit is on file.",
                roof_ctx.material_class.replace('_', " "),
                roof_age_cutoff
            )
        }));
    }

    // --- 2C. Convective Storm / Hail / Tornado Loading ---
    let convective_max = nri_hail.max(nri_trnd).max(nri_swnd);
    let in_hail_belt = CONVECTIVE_HAIL_STATES.contains(st.as_str())
        && (convective_max >= 55.0
            || matches!(st.as_str(), "OK" | "KS" | "TX" | "CO" | "NE" | "MN" | "MO"));
    let severe_convective = convective_max >= 82.0
        || (in_hail_belt
            && matches!(st.as_str(), "OK" | "KS" | "CO" | "TX" | "NE")
            && convective_max >= 68.0);

    if severe_convective || in_hail_belt {
        surcharged_triggers.push(format!(
            "Convective hail/tornado/wind loading ({} belt; FEMA NRI max {:.0}th pct) — 1%–2% wind/hail deductible typical",
            st,
            if convective_max > 0.0 { convective_max } else { 72.0 }
        ));
        vulnerability_flags.push(json!({
            "id": "convective_hail_belt",
            "severity": if severe_convective { "high" } else { "moderate" },
            "title": "Convective Hail / Windstorm Surcharge Belt",
            "detail": format!(
                "{} carries heavy hail/convective storm loss ratios; Class 4 impact-resistant shingles (UL 2218) can cut wind/hail premiums by 10%–25%.",
                if st.is_empty() { "Region" } else { &st }
            )
        }));
    }

    // --- 2C2. Fire Protection Infrastructure, ISO PPC Proxy & 3DEP LiDAR Canopy (`fire_protection_hydrants`) ---
    let parcel_lat = g
        .pointer("/refined/lat")
        .or_else(|| g.get("lat"))
        .and_then(Value::as_f64);
    let parcel_lon = g
        .pointer("/refined/lon")
        .or_else(|| g.get("lon"))
        .and_then(Value::as_f64);

    let resolved_fire_prot: Option<Value> =
        if crate::extensions::ExtensionHub::global().is_enabled(FIRE_PROTECTION_EXT_ID) {
            fire_protection
                .or_else(|| h.get("fireProtection"))
                .or_else(|| f.get("fireProtection"))
                .or_else(|| {
                    condition
                        .map(unwrap_data)
                        .and_then(|c| c.get("fireProtection"))
                })
                .filter(|v| v.is_object())
                .map(|raw_fp| {
                    fuse_parcel_fire_protection(
                        raw_fp,
                        parcel_lat,
                        parcel_lon,
                        f,
                        h,
                        e,
                        condition,
                        house3d,
                        is_coastal_wind_county,
                        now_year,
                    )
                })
        } else {
            None
        };

    let lidar_canopy = resolved_fire_prot
        .as_ref()
        .and_then(|fp| fp.get("lidarCanopyOverhang"))
        .cloned()
        .unwrap_or_else(|| evaluate_lidar_canopy_overhang(house3d, f, h, condition));

    let ppc_class_opt = resolved_fire_prot
        .as_ref()
        .and_then(|fp| fp.pointer("/isoPpcProxy/ppcClass"))
        .and_then(Value::as_i64);
    let ppc_hydrant_1000_opt = resolved_fire_prot
        .as_ref()
        .and_then(|fp| fp.pointer("/isoPpcProxy/hydrantWithin1000Ft"))
        .and_then(Value::as_bool);
    let ppc_basis_str = resolved_fire_prot
        .as_ref()
        .and_then(|fp| fp.pointer("/isoPpcProxy/basis"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let iso_ppc_10_unprotected = !is_condo && ppc_class_opt == Some(10);
    let iso_ppc_9_semi_protected = !is_condo && ppc_class_opt == Some(9);
    let iso_ppc_urban_protected = ppc_class_opt.map(|c| c <= 3).unwrap_or(false)
        && ppc_hydrant_1000_opt == Some(true);
    let has_lidar_canopy_overhang =
        lidar_canopy.get("underwritingFlag").and_then(Value::as_bool) == Some(true);

    if iso_ppc_10_unprotected {
        nonrenewal_pool_triggers.push(
            "ISO Public Protection Class 10 proxy (>5 est. road miles from nearest mapped fire station) triggers unprotected fire declination or FAIR/surplus routing"
                .to_string(),
        );
        vulnerability_flags.push(json!({
            "id": "iso_ppc_10_unprotected",
            "severity": "high",
            "title": "Unprotected Fire Response Distance (ISO PPC 10 Proxy)",
            "detail": if ppc_basis_str.is_empty() {
                "Parcel sits more than 5 estimated road miles from the nearest mapped fire station; standard admitted HO-3 carriers routinely decline PPC 10 risks or require verified private fire-water storage / FAIR Plan coverage.".to_string()
            } else {
                format!("{ppc_basis_str} Standard admitted HO-3 carriers routinely decline PPC 10 risks or require verified fire-water storage / FAIR Plan coverage.")
            }
        }));
    } else if iso_ppc_9_semi_protected {
        surcharged_triggers.push(
            "ISO Public Protection Class 8B/9 proxy (fire station <=5 road mi, no mapped hydrant within 1,000 ft / rural water) carries semi-protected fire surcharge"
                .to_string(),
        );
        vulnerability_flags.push(json!({
            "id": "iso_ppc_9_hydrant_gap",
            "severity": "moderate",
            "title": "Hydrant >1,000 ft / Semi-Protected Water Supply (ISO PPC 8B/9 Proxy)",
            "detail": if ppc_basis_str.is_empty() {
                "Fire station is within 5 road miles, but no pressurized fire hydrant is confirmed within 1,000 ft; expect semi-protected fire rating unless a municipal hydrant or certified drafting cistern is verified.".to_string()
            } else {
                ppc_basis_str.to_string()
            }
        }));
    }

    if has_lidar_canopy_overhang {
        if wui_very_high || wui_high || wui_moderate {
            surcharged_triggers.push(
                "USGS 3DEP LiDAR detected >=3m tree canopy within 2m of roof perimeter (Zone 0/1 aerial brush-clearance inspection flag)"
                    .to_string(),
            );
        }
        vulnerability_flags.push(json!({
            "id": "lidar_roof_canopy_overhang",
            "severity": if wui_very_high || wui_high || wui_moderate { "high" } else { "moderate" },
            "title": "3DEP LiDAR Tree Canopy Within 2 m of Roof Perimeter",
            "detail": lidar_canopy
                .get("arboristTrimCreditNote")
                .and_then(Value::as_str)
                .unwrap_or("Airborne LiDAR detected >=3 m tree canopy overhanging or within 2 m of the roof edge; carriers frequently issue post-bind aerial-imagery tree-trimming requirements.")
        }));
    }

    // --- Compute Base Homeowners / FAIR Plan + DIC Wrap ---
    let uses_fair_or_wind_pool = !nonrenewal_pool_triggers.is_empty();
    let (base_expected, dic_expected, policy_type, carrier_market, dic_note): (
        f64,
        f64,
        String,
        &'static str,
        Option<String>,
    ) = if (wui_very_high || wui_high || iso_ppc_10_unprotected) && !is_condo {
        // State FAIR Plan (fire/smoke/lightning) + Difference-in-Conditions (DIC) wrap
        let fair_rate = if wui_very_high {
            if is_hillside {
                0.0082
            } else {
                0.0068
            }
        } else if wui_high {
            if is_hillside {
                0.0056
            } else {
                0.0046
            }
        } else {
            // Unprotected ISO PPC 10 rural fire pool / specialty surplus dwelling fire
            0.0042
        };
        let shake_mult = if roof_ctx.material_class == "wood_shake" {
            1.30
        } else {
            1.0
        };
        let canopy_mult = if has_lidar_canopy_overhang { 1.06 } else { 1.0 };
        let fair_exp =
            (coverage_a_usd as f64 * fair_rate * shake_mult * canopy_mult).max(if wui_very_high {
                5_800.0
            } else if wui_high {
                4_100.0
            } else {
                3_600.0
            });
        let dic_exp = (1_150.0 + coverage_a_usd as f64 * 0.00085).clamp(1_350.0, 3_200.0);
        let ptype = if st == "CA" {
            "CA FAIR Plan Dwelling Fire".to_string()
        } else {
            format!(
                "{} FAIR Plan / Surplus WUI Fire",
                if st.is_empty() { "State" } else { &st }
            )
        };
        let dnote = format!(
            "Mandatory companion Difference-in-Conditions (DIC) wrap policy covering water damage, personal liability, theft & falling objects excluded by {} bare fire policy.",
            if st == "CA" { "the CA FAIR Plan" } else { "state FAIR Plan" }
        );
        (
            fair_exp,
            dic_exp,
            ptype,
            "State Last-Resort FAIR Pool + Admitted/E&S DIC",
            Some(dnote),
        )
    } else if is_coastal_wind_county && uses_fair_or_wind_pool && !is_condo {
        // FL Citizens / TX TWIA / LA Citizens / Coastal Wind Pool
        let separate_wind_wrap = matches!(st.as_str(), "TX" | "NC" | "SC" | "AL" | "MS");
        let roof_mult = if roof_ctx
            .effective_age_years
            .map(|a| a > 20)
            .unwrap_or(false)
            && !roof_ctx.has_reroof_permit
        {
            1.48
        } else if roof_over_cutoff {
            1.28
        } else if roof_ctx.material_class == "impact_class4" || roof_ctx.material_class == "metal" {
            0.86
        } else {
            1.08
        };
        let wind_rate = match st.as_str() {
            "FL" => 0.0135,
            "LA" => 0.0118,
            "TX" => 0.0088,
            _ => 0.0082,
        };
        let main_exp = (coverage_a_usd as f64 * wind_rate * roof_mult).max(5_200.0);
        if separate_wind_wrap {
            let ex_wind_ho = (coverage_a_usd as f64 * 0.0028).clamp(1_250.0, 3_400.0);
            (
                main_exp,
                ex_wind_ho,
                wind_pool_name.to_string(),
                "State Windstorm Pool + Ex-Wind Companion HO-3",
                Some(
                    "Coastal windstorm pool covers wind/hail only; companion ex-wind HO-3 policy required for fire, water & liability."
                        .into(),
                ),
            )
        } else {
            (
                main_exp,
                0.0,
                wind_pool_name.to_string(),
                "State Last-Resort Wind / Citizens Pool",
                None,
            )
        }
    } else {
        // Admitted or Tight/Surcharged HO-3 / HO-5 / HO-6 Market
        let mut rate = state_admitted_base_rate(&st);
        if wui_moderate {
            rate *= 1.42;
        }
        if is_coastal_wind_county {
            rate *= 1.32;
        } else if is_moderate_hurricane {
            rate *= 1.16;
        }
        if severe_convective {
            rate *= 1.34;
        } else if in_hail_belt {
            rate *= 1.16;
        }
        if roof_ctx.material_class == "wood_shake" {
            rate *= 1.35;
        } else if roof_ctx.material_class == "impact_class4" && in_hail_belt {
            rate *= 0.86;
        }
        if roof_over_cutoff {
            rate *= 1.24;
        } else if roof_ctx.has_reroof_permit
            && roof_ctx
                .effective_age_years
                .map(|a| a <= 5)
                .unwrap_or(false)
        {
            rate *= 0.90;
        }
        if iso_ppc_9_semi_protected {
            rate *= 1.12;
        } else if iso_ppc_urban_protected {
            rate *= 0.96;
        }
        if has_lidar_canopy_overhang && wui_moderate {
            rate *= 1.06;
        }
        if yb_opt.map(|yb| yb < 1960).unwrap_or(false) {
            rate *= 1.12;
        }
        let min_ho = if is_condo { 780.0 } else { 1_350.0 };
        let mkt_liability_load = ref_market_usd_opt
            .map(|m| m as f64 * 0.00012)
            .unwrap_or(0.0);
        let ho_exp = (coverage_a_usd as f64 * rate + mkt_liability_load).max(min_ho);
        let ptype = if is_condo {
            "HO-6 Condo Walls-In + Loss Assessment".to_string()
        } else if !surcharged_triggers.is_empty() {
            "HO-3 Homeowners (Surcharged / Specialty Admitted)".to_string()
        } else {
            "Standard Admitted HO-3 / HO-5 Homeowners".to_string()
        };
        let cmarket = if !surcharged_triggers.is_empty() {
            "Tight Admitted / Specialty E&S Market"
        } else {
            "Standard Admitted Carrier Market"
        };
        (ho_exp, 0.0, ptype, cmarket, None)
    };

    // --- 2D. Flood Insurance (NFIP Risk Rating 2.0 Proxy + Private Excess Flood) ---
    let flood_zone_raw_opt = h
        .pointer("/flood/zone")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty());
    let flood_checked = h.pointer("/flood/checked").and_then(|v| v.as_bool());
    let in_sfha_explicit = h
        .pointer("/flood/sfha")
        .or_else(|| h.pointer("/flood/inSfha"))
        .and_then(|v| v.as_bool());
    let has_flood_data = flood_checked != Some(false)
        && (flood_zone_raw_opt.is_some() || in_sfha_explicit.is_some());
    let flood_zone_raw = flood_zone_raw_opt.clone().unwrap_or_default();
    let flood_subtype = h
        .pointer("/flood/zoneSubtype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let in_sfha_flag = in_sfha_explicit.unwrap_or(false);
    let fz_tok = flood_zone_raw
        .split([' ', ';', ',', '/'])
        .next()
        .unwrap_or("");
    let is_coastal_v_zone = fz_tok == "V"
        || fz_tok.starts_with("VE")
        || (fz_tok.starts_with('V')
            && fz_tok[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()));
    let is_a_zone = matches!(fz_tok, "A" | "AE" | "AO" | "AH" | "AR" | "A99")
        || fz_tok.starts_with("AE")
        || fz_tok.starts_with("AO")
        || fz_tok.starts_with("AH")
        || (fz_tok.starts_with('A')
            && fz_tok[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()));
    let flood_mandatory = in_sfha_flag || is_coastal_v_zone || is_a_zone;

    let near_sfha_m = h
        .pointer("/flood/nearestSfha/distM")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            h.pointer("/flood/nearestSfha/distMi")
                .and_then(|v| v.as_f64())
                .map(|mi| mi * 1_609.344)
        });
    let flood_depth_ft = h
        .pointer("/flood/depthFt")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let is_shaded_x = !flood_mandatory
        && (flood_subtype.contains("0.2 PCT")
            || flood_subtype.contains("500")
            || flood_subtype.contains("SHADED")
            || flood_subtype.contains("LEVEE")
            || h.pointer("/flood/risk")
                .and_then(|v| v.as_str())
                .map(|r| r.eq_ignore_ascii_case("moderate"))
                .unwrap_or(false));
    let dam_inundation = h
        .pointer("/other/damInundation/inArea")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let levee_protected = h
        .pointer("/other/leveeProtected")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let nri_flood = extract_nri_score(h, "IFLD")
        .unwrap_or(0.0)
        .max(extract_nri_score(h, "CFLD").unwrap_or(0.0));

    let fnd_lc = f
        .get("foundation")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let fnd_flood_mult = if fnd_lc.contains("basement") {
        1.22
    } else if fnd_lc.contains("crawl") || fnd_lc.contains("raised") || fnd_lc.contains("pier") {
        0.90 // Higher first-floor elevation above grade reduces Risk Rating 2.0 water-intrusion frequency
    } else {
        1.04 // Slab-on-grade
    };

    // NFIP caps residential building coverage at $250,000; private excess flood covers rebuild gap above $250k
    let nfip_cov = (coverage_a_usd.min(250_000)) as f64;
    let excess_flood_cov = ((coverage_a_usd - 250_000).max(0)) as f64;

    let (flood_expected, flood_recommended, flood_band_label): (f64, bool, String) =
        if is_coastal_v_zone {
            let exp = (nfip_cov * 0.024 + excess_flood_cov * 0.0042) * fnd_flood_mult;
            let clamped = exp.clamp(4_800.0, 15_500.0);
            surcharged_triggers.push(format!(
                "FEMA Zone {flood_zone_raw} Coastal Velocity SFHA triggers mandatory flood insurance"
            ));
            vulnerability_flags.push(json!({
                "id": "mandatory_fema_sfha_flood",
                "severity": "high",
                "title": format!("Mandatory Flood Insurance — FEMA Coastal Velocity Zone {flood_zone_raw}"),
                "detail": "Federally backed mortgages mandate flood insurance in Coastal High Hazard V/VE zones; NFIP caps dwelling coverage at $250k, so private excess flood is recommended above $250k Coverage A."
            }));
            (
                clamped,
                true,
                format!("Mandatory SFHA Coastal Wave Velocity ({flood_zone_raw}) — NFIP + Excess Private Flood"),
            )
        } else if flood_mandatory {
            let depth_mult = if flood_depth_ft >= 3.0 {
                1.35
            } else if flood_depth_ft >= 1.0 {
                1.15
            } else {
                1.0
            };
            let exp = (nfip_cov * 0.0102 + excess_flood_cov * 0.0018) * fnd_flood_mult * depth_mult;
            let clamped = exp.clamp(1_850.0, 7_200.0);
            surcharged_triggers.push(format!(
                "FEMA Zone {flood_zone_raw} Special Flood Hazard Area (SFHA) mandates lender flood escrow"
            ));
            vulnerability_flags.push(json!({
                "id": "mandatory_fema_sfha_flood",
                "severity": "high",
                "title": format!("Mandatory Lender Flood Insurance — FEMA Zone {flood_zone_raw} (SFHA)"),
                "detail": "1%-annual-chance (100-year) FEMA Special Flood Hazard Area requires continuous flood insurance for any federally regulated mortgage. NFIP Risk Rating 2.0 prices by distance-to-water, first-floor height & rebuild cost."
            }));
            (
                clamped,
                true,
                format!("Mandatory SFHA ({flood_zone_raw}) — NFIP Risk Rating 2.0 + Excess Private Flood"),
            )
        } else if is_shaded_x || near_sfha_m.map(|d| d <= 150.0).unwrap_or(false) || levee_protected
        {
            let exp = (720.0 + coverage_a_usd as f64 * 0.00055) * fnd_flood_mult;
            (
                exp.clamp(780.0, 1_950.0),
                true,
                if is_shaded_x {
                    "Zone X (Shaded / 0.2% 500-Yr Floodplain) — Recommended NFIP RR2.0 / Private Flood".into()
                } else {
                    format!(
                        "Near SFHA Boundary ({:.0} m) — Recommended Risk Rating 2.0 Proximity Band",
                        near_sfha_m.unwrap_or(100.0)
                    )
                },
            )
        } else if near_sfha_m.map(|d| d <= 500.0).unwrap_or(false)
            || dam_inundation
            || nri_flood >= 75.0
        {
            let exp = (540.0 + coverage_a_usd as f64 * 0.00032) * fnd_flood_mult;
            (
                exp.clamp(550.0, 1_250.0),
                true,
                "Moderate Flood / Drainage Proximity — Optional Preferred Private/NFIP Flood"
                    .into(),
            )
        } else if has_flood_data {
            let zone_lbl = if flood_zone_raw.is_empty() {
                "X".to_string()
            } else {
                flood_zone_raw.clone()
            };
            (
                (440.0 + coverage_a_usd as f64 * 0.00018).clamp(450.0, 820.0),
                false,
                format!("Minimal Flood Zone {zone_lbl} — Optional Preferred-Risk Flood Benchmark"),
            )
        } else {
            (
                (440.0 + coverage_a_usd as f64 * 0.00018).clamp(450.0, 820.0),
                false,
                "FEMA Flood Zone Unverified — Optional Preferred Flood Benchmark (Confirm NFHL)"
                    .into(),
            )
        };

    // --- 2E. Earthquake Insurance (CEA / Public Seismic Actuarial Benchmark) ---
    let pga_opt = h
        .pointer("/earthquake/designMaps/pgaM")
        .or_else(|| h.pointer("/earthquake/designMaps/pga"))
        .and_then(|v| v.as_f64());
    let ss_opt = h
        .pointer("/earthquake/designMaps/ss")
        .and_then(|v| v.as_f64());
    let sdc = h
        .pointer("/earthquake/designMaps/sdc")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let fault_km = h
        .pointer("/earthquake/faults/nearestRecentlyActive/distKm")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            h.pointer("/earthquake/nearestFaults/0/distKm")
                .and_then(|v| v.as_f64())
        });
    let in_ap = h
        .pointer("/earthquake/california/alquistPriolo/inZone")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let in_liq = h
        .pointer("/earthquake/california/liquefaction/inZone")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let is_raised_or_unknown_fnd = !fnd_lc.contains("slab")
        || fnd_lc.contains("crawl")
        || fnd_lc.contains("raised")
        || fnd_lc.contains("pier");
    let high_seismic_zone = pga_opt.map(|p| p >= 0.30).unwrap_or(false)
        || ss_opt.map(|s| s >= 0.75).unwrap_or(false)
        || matches!(sdc, "D" | "D0" | "D1" | "D2" | "E" | "F")
        || fault_km.map(|d| d <= 20.0).unwrap_or(false)
        || in_ap
        || in_liq;

    let pre1980_cripple_wall = yb_opt.map(|yb| yb < 1980).unwrap_or(false)
        && is_raised_or_unknown_fnd
        && high_seismic_zone
        && !roof_ctx.has_seismic_retrofit;
    let soft_story_potential = yb_opt.map(|yb| yb < 1985).unwrap_or(false)
        && stories >= 2.0
        && (garage_str.contains("attached") || garage_str.contains("tuck") || is_hillside)
        && high_seismic_zone
        && !roof_ctx.has_seismic_retrofit;

    if let Some(yb) = yb_opt.filter(|_| pre1980_cripple_wall) {
        vulnerability_flags.push(json!({
            "id": "pre1980_cripple_wall_seismic",
            "severity": "high",
            "title": format!("Pre-1980 Wood-Frame Cripple-Wall Vulnerability (Built {yb})"),
            "detail": "Pre-1980 raised/crawlspace wood-frame homes without verified sill-plate anchor bolting & plywood shear-wall bracing can slide off their foundations in strong shaking — and pay 10%–25% higher seismic insurance until retrofitted."
        }));
    }
    if soft_story_potential {
        vulnerability_flags.push(json!({
            "id": "soft_story_potential",
            "severity": "moderate",
            "title": "Multi-Story Over Garage / Hillside Soft-Story Check",
            "detail": "Living space over a wide garage door opening or hillside cripple framing concentrates lateral drift during earthquakes; have a structural engineer verify shear walls."
        }));
    }
    if in_ap || in_liq || cgs_landslide {
        vulnerability_flags.push(json!({
            "id": "cgs_liquefaction_or_landslide",
            "severity": if in_ap { "high" } else { "moderate" },
            "title": "Mapped Seismic Ground-Failure / Fault Rupture Zone",
            "detail": format!(
                "Mapped inside {} — ground deformation amplifies structural & utility pipe damage beyond standard shaking.",
                [
                    in_ap.then_some("Alquist-Priolo Fault Rupture Zone"),
                    in_liq.then_some("CGS Liquefaction Zone"),
                    cgs_landslide.then_some("CGS Earthquake-Induced Landslide Zone"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" + ")
            )
        }));
    }

    let eq_deductible_pct: i64 =
        if pga_opt.map(|p| p >= 0.45).unwrap_or(false) || in_ap || pre1980_cripple_wall {
            15
        } else {
            10
        };
    let eq_deductible_usd =
        ((coverage_a_usd as f64 * (eq_deductible_pct as f64 / 100.0)) / 500.0).round() as i64 * 500;

    let eq_expected_opt = if pga_opt.is_some() || high_seismic_zone {
        let mut eq_rate = match pga_opt {
            Some(pga) if pga >= 0.75 => 0.0044,
            Some(pga) if pga >= 0.55 => 0.0033,
            Some(pga) if pga >= 0.35 => 0.0023,
            Some(pga) if pga >= 0.18 => 0.0013,
            Some(_) => 0.0005,
            None => 0.0020,
        };
        if let Some(dk) = fault_km {
            if dk <= 5.0 {
                eq_rate *= 1.32;
            } else if dk <= 12.0 {
                eq_rate *= 1.15;
            }
        }
        if in_ap {
            eq_rate *= 1.35;
        }
        if in_liq || cgs_landslide {
            eq_rate *= 1.22;
        }
        if pre1980_cripple_wall {
            eq_rate *= 1.34;
        } else if yb_opt.map(|yb| yb < 1980).unwrap_or(false) && roof_ctx.has_seismic_retrofit {
            eq_rate *= 0.82; // CEA / private Brace+Bolt retrofit discount!
        }
        if stories >= 2.0 {
            eq_rate *= 1.10;
        }

        Some(
            (coverage_a_usd as f64 * eq_rate)
                .clamp(if high_seismic_zone { 850.0 } else { 280.0 }, 14_500.0),
        )
    } else {
        None
    };

    let eq_provider = if st == "CA" {
        "California Earthquake Authority (CEA) Public Tariff Benchmark"
    } else if matches!(
        st.as_str(),
        "WA" | "OR" | "AK" | "UT" | "NV" | "MO" | "TN" | "SC" | "HI"
    ) {
        "State DOI Stand-Alone Seismic Actuarial Benchmark"
    } else {
        "Optional Earthquake Endorsement Benchmark"
    };

    // Environmental air/water risk note (smoke/corrosive exposure)
    let env_aqi_max = e
        .pointer("/air/usAqi/meanDailyMax")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    // =========================================================================
    // 3. Insurability Tier, Score & Payment Shock Assembly
    // =========================================================================
    let insurability_tier = if !nonrenewal_pool_triggers.is_empty() {
        "High Non-Renewal / State FAIR or Wind Pool Risk"
    } else if !surcharged_triggers.is_empty() || flood_mandatory {
        "Tight / Surcharged Market"
    } else {
        "Standard Admitted Market"
    };

    let mut score: f64 = 90.0;
    if wui_very_high {
        score -= 58.0;
    } else if wui_high {
        score -= 46.0;
    } else if wui_moderate {
        score -= 18.0;
    }
    if is_coastal_wind_county {
        score -= if uses_fair_or_wind_pool { 44.0 } else { 22.0 };
    } else if is_moderate_hurricane {
        score -= 12.0;
    }
    if severe_convective {
        score -= 18.0;
    } else if in_hail_belt {
        score -= 9.0;
    }
    if roof_over_cutoff {
        score -= if is_coastal_wind_county { 18.0 } else { 11.0 };
    }
    if roof_ctx.material_class == "wood_shake" {
        score -= 18.0;
    }
    if is_coastal_v_zone {
        score -= 22.0;
    } else if flood_mandatory {
        score -= 16.0;
    } else if is_shaded_x {
        score -= 6.0;
    }
    if pre1980_cripple_wall {
        score -= 7.0;
    }
    if env_aqi_max >= 85.0 {
        score -= 3.0;
    }
    if iso_ppc_10_unprotected {
        score -= 38.0;
    } else if iso_ppc_9_semi_protected {
        score -= 10.0;
    } else if iso_ppc_urban_protected {
        score += 2.0;
    }
    if has_lidar_canopy_overhang {
        score -= if wui_very_high || wui_high || wui_moderate {
            6.0
        } else {
            3.0
        };
    }

    let insurability_score = match insurability_tier {
        "High Non-Renewal / State FAIR or Wind Pool Risk" => score.round().clamp(10.0, 42.0) as i64,
        "Tight / Surcharged Market" => score.round().clamp(45.0, 74.0) as i64,
        _ => score.round().clamp(76.0, 96.0) as i64,
    };

    let round50 = |x: f64| -> i64 { ((x / 50.0).round() as i64 * 50).max(0) };

    let base_low = round50(base_expected * 0.82);
    let base_exp_i = round50(base_expected);
    let base_high = round50(base_expected * 1.28);

    let dic_low = if dic_expected > 0.0 {
        round50(dic_expected * 0.85)
    } else {
        0
    };
    let dic_exp_i = if dic_expected > 0.0 {
        round50(dic_expected)
    } else {
        0
    };
    let dic_high = if dic_expected > 0.0 {
        round50(dic_expected * 1.25)
    } else {
        0
    };

    let flood_low = round50(flood_expected * 0.78);
    let flood_exp_i = round50(flood_expected);
    let flood_high = round50(flood_expected * 1.35);

    let eq_low_opt = eq_expected_opt.map(|eq| round50(eq * 0.78));
    let eq_exp_opt = eq_expected_opt.map(round50);
    let eq_high_opt = eq_expected_opt.map(|eq| round50(eq * 1.38));

    // Lender-required + essential carrying annual premium:
    // Base Homeowners/FAIR Plan + DIC/Ex-Wind Wrap + Mandatory SFHA Flood (when required by lender)
    let required_flood_low = if flood_mandatory { flood_low } else { 0 };
    let required_flood_exp = if flood_mandatory { flood_exp_i } else { 0 };
    let required_flood_high = if flood_mandatory { flood_high } else { 0 };

    let total_low = base_low + dic_low + required_flood_low;
    let total_expected = base_exp_i + dic_exp_i + required_flood_exp;
    let total_high = base_high + dic_high + required_flood_high;

    let all_perils_expected = base_exp_i
        + dic_exp_i
        + (if flood_mandatory || flood_recommended {
            flood_exp_i
        } else {
            0
        })
        + (if high_seismic_zone {
            eq_exp_opt.unwrap_or(0)
        } else {
            0
        });

    let realistic_expected_annual = total_expected;
    let realistic_monthly = ((realistic_expected_annual as f64) / 12.0).round() as i64;

    // Naive lender / portal calculator benchmark: 0.35% of property market value per year (only when market value exists!)
    let (naive_lender_annual_opt, naive_monthly_opt, monthly_payment_shock_opt, payment_shock_note) =
        if let Some(ref_market_usd) = ref_market_usd_opt {
            let naive_lender_annual = ((ref_market_usd as f64) * 0.0035).round() as i64;
            let naive_monthly = ((naive_lender_annual as f64) / 12.0).round() as i64;
            let monthly_payment_shock =
                (((realistic_expected_annual - naive_lender_annual) as f64) / 12.0).round() as i64;
            let note = if monthly_payment_shock > 0 {
                format!(
                    "Realistic hazard-indexed insurance (${}/yr · ${}/mo) costs +${}/mo (+${}/yr) MORE than a naive 0.35% lender/portal calculator (${}/yr · ${}/mo).",
                    fmt_commas(realistic_expected_annual),
                    fmt_commas(realistic_monthly),
                    fmt_commas(monthly_payment_shock),
                    fmt_commas(realistic_expected_annual - naive_lender_annual),
                    fmt_commas(naive_lender_annual),
                    fmt_commas(naive_monthly)
                )
            } else {
                let rtm = rebuild_to_market_pct_opt.unwrap_or(100.0);
                format!(
                    "Realistic hazard-indexed insurance (${}/yr · ${}/mo) is in line with or below a naive 0.35% of purchase price estimate (${}/yr · ${}/mo) because land accounts for {}% of property value.",
                    fmt_commas(realistic_expected_annual),
                    fmt_commas(realistic_monthly),
                    fmt_commas(naive_lender_annual),
                    fmt_commas(naive_monthly),
                    round_n(100.0 - rtm.min(100.0), 1)
                )
            };
            (
                Some(naive_lender_annual),
                Some(naive_monthly),
                Some(monthly_payment_shock),
                note,
            )
        } else {
            (
                None,
                None,
                None,
                format!(
                    "Realistic hazard-indexed insurance is modeled at ${}/yr (${}/mo); property market value is absent so the 0.35%-of-price lender calculator comparison is omitted rather than invented.",
                    fmt_commas(realistic_expected_annual),
                    fmt_commas(realistic_monthly)
                ),
            )
        };

    // =========================================================================
    // 4. Actionable Buyer Mitigation Discounts & Contingency Checklist
    // =========================================================================
    let mut checklist: Vec<Value> = vec![
        json!({
            "id": "clue_loss_history",
            "priority": "critical",
            "title": "Order LexisNexis C.L.U.E. 5-Year Property Loss History Report During Contingency",
            "estimatedSavingsOrImpact": "Prevents surprise post-close non-renewal or 30%–80% water/fire claim surcharges",
            "detail": "Have the seller request the LexisNexis C.L.U.E. (Comprehensive Loss Underwriting Exchange) 5-year claims history within the first 5 days of escrow, and obtain a written, bindable insurance quote BEFORE removing inspection/insurance contingencies."
        }),
        json!({
            "id": "roof_certification_check",
            "priority": if roof_over_cutoff || !roof_ctx.has_reroof_permit { "critical" } else { "recommended" },
            "title": "Verify Roof Age, Finaled Reroof Permit & Remaining Useful Life Certification",
            "estimatedSavingsOrImpact": if is_coastal_wind_county {
                "Unlocks admitted wind carriers & saves $1,500–$4,500/yr vs. Wind Pool"
            } else {
                "Preserves full Replacement Cost Value (RCV) roof settlement vs. depreciated ACV"
            },
            "detail": format!(
                "Estimated roof is {roof_age_label} ({}). Request reroof permit final card and a licensed roofer 3–5 yr leak-free / 4-Point certification.",
                if let Some(ry) = roof_ctx.latest_reroof_year {
                    format!("latest reroof permit {ry}")
                } else {
                    "no reroof permit verified in public records".into()
                }
            )
        }),
    ];

    if wui_very_high || wui_high || wui_moderate || st == "CA" {
        checklist.push(json!({
            "id": "wildfire_defensible_space",
            "priority": if wui_very_high || wui_high { "critical" } else { "recommended" },
            "title": if st == "CA" {
                "CA 'Safer from Wildfires' & IBHS Wildfire Prepared Home Mitigation Credits"
            } else {
                "IBHS Wildfire Prepared Home & WUI Defensible Space Hardening"
            },
            "estimatedSavingsOrImpact": "5%–20% CA FAIR Plan / admitted wildfire mitigation discount",
            "detail": "Establish Zone 0 (0–5 ft non-combustible hardscape/gravel perimeter around structure & decks), 100-ft AB-38 defensible space brush clearance (required at point-of-sale in CA High/Very High FHSZ), 1/8-inch ember-resistant attic/crawl vents, Class A roof assembly, and enclosed eaves."
        }));
    }

    if is_coastal_wind_county || severe_convective || in_hail_belt {
        checklist.push(json!({
            "id": "wind_hail_fortified",
            "priority": "high",
            "title": if st == "FL" {
                "OIR-B1-1802 Uniform Wind Mitigation + 4-Point Inspection"
            } else {
                "IBHS FORTIFIED Roof / UL 2218 Class 4 Impact-Resistant Shingle Discount"
            },
            "estimatedSavingsOrImpact": "15%–45% wind/hail premium credit",
            "detail": if st == "FL" {
                "Document roof-to-wall hurricane clips/straps, secondary water resistance (peel-and-stick SWR), hip-roof geometry, and Miami-Dade/impact-rated openings or shutters to maximize statutory FL wind credits."
            } else {
                "When reroofing, upgrade to UL 2218 Class 4 impact-resistant shingles and sealed roof deck (IBHS FORTIFIED Roof) for substantial hail/wind carrier discounts."
            }
        }));
    }

    if flood_mandatory || flood_recommended {
        checklist.push(json!({
            "id": "fema_elevation_cert",
            "priority": if flood_mandatory { "critical" } else { "recommended" },
            "title": "Request Seller NFIP Assumable Policy + FEMA Elevation Certificate / Private Flood Quotes",
            "estimatedSavingsOrImpact": "Can save $600–$3,000+/yr via NFIP policy assumption or LOMA appeal",
            "detail": format!(
                "In {}, ask whether the seller holds an active NFIP policy (buyers can legally assume the seller's Risk Rating 2.0 glide-path rate at closing!), compare private excess/primary flood Lloyd's carriers, and check FEMA Elevation Certificate first-floor height above Base Flood Elevation.",
                flood_zone_raw
            )
        }));
    }

    if high_seismic_zone {
        checklist.push(json!({
            "id": "seismic_ebb_retrofit",
            "priority": if pre1980_cripple_wall { "high" } else { "recommended" },
            "title": if st == "CA" {
                "CEA Earthquake Brace + Bolt (EBB) $3,000+ Grant & Seismic Retrofit Discount"
            } else {
                "Foundation Sill-Plate Anchor Bolting & Cripple-Wall Shear Bracing"
            },
            "estimatedSavingsOrImpact": if st == "CA" {
                "Up to $3,000–$7,000 EBB grant + 10%–25% CEA annual premium discount"
            } else {
                "10%–20% seismic endorsement discount & major collapse-risk reduction"
            },
            "detail": "Pre-1980 raised-foundation wood-frame homes qualify for California Residential Mitigation Program EBB grants (anchor bolts + cripple-wall plywood + strapped water heater) and substantial CEA rate reductions."
        }));
    }

    if has_lidar_canopy_overhang {
        checklist.push(json!({
            "id": "lidar_arborist_canopy_trim",
            "priority": if wui_very_high || wui_high { "critical" } else { "high" },
            "title": "ISA Certified Arborist 6–10 ft Roof & Chimney Canopy Clearance Trim",
            "estimatedSavingsOrImpact": "Clears carrier aerial-imagery overhang hold & reduces roof limb/ember loss risk",
            "detail": "USGS 3DEP airborne LiDAR detected >=3 m vegetation canopy within 2 m of the roof perimeter. Prune overhanging branches at least 6–10 ft back from roof planes and chimneys, and retain dated before/after photos for underwriting."
        }));
    }

    if iso_ppc_10_unprotected || iso_ppc_9_semi_protected {
        checklist.push(json!({
            "id": "rural_fire_suppression_water",
            "priority": if iso_ppc_10_unprotected { "critical" } else { "high" },
            "title": "Verify Local Fire District Substation & Dedicated Fire-Water Cistern / Hydrant Access",
            "estimatedSavingsOrImpact": "Can reclassify PPC 9/10 to protected PPC 5–8B & unlock admitted HO-3 markets",
            "detail": "Confirm whether a volunteer/paid fire substation or pressurized municipal hydrant sits closer than mapped in federal GIS, or document an NFPA 1142-compliant >=2,500-gallon fire-department drafting tank / dry hydrant."
        }));
    }

    checklist.push(json!({
        "id": "smart_water_shutoff",
        "priority": "recommended",
        "title": "Install Automatic Whole-Home Water Leak Shutoff Valve",
        "estimatedSavingsOrImpact": "5%–10% HO-3 / DIC non-fire water-damage credit",
        "detail": "Non-weather plumbing leaks are the #1 frequency claim on HO-3 and DIC policies; ultrasonic automatic shutoff valves (e.g., Flo / Phyn) are increasingly required or discounted on high-value homes."
    }));

    let base_breakdown = json!({
        "low": base_low,
        "expected": base_exp_i,
        "high": base_high,
        "policyType": policy_type,
        "carrierMarket": carrier_market,
    });
    let dic_breakdown = json!({
        "low": dic_low,
        "expected": dic_exp_i,
        "high": dic_high,
        "requiredWithFairOrWindPool": dic_exp_i > 0,
        "note": dic_note,
    });
    let flood_breakdown = json!({
        "low": flood_low,
        "expected": flood_exp_i,
        "high": flood_high,
        "mandatoryByLender": flood_mandatory,
        "recommended": flood_recommended,
        "femaZone": if has_flood_data {
            flood_zone_raw_opt
                .as_deref()
                .or(if flood_mandatory { Some("SFHA") } else { Some("UNMAPPED") })
        } else {
            None
        },
        "femaZoneVerified": has_flood_data,
        "nearestSfhaMeters": near_sfha_m.map(|d| d.round() as i64),
        "pricingBand": flood_band_label,
    });
    let eq_breakdown = json!({
        "low": eq_low_opt,
        "expected": eq_exp_opt,
        "high": eq_high_opt,
        "recommended": high_seismic_zone,
        "provider": eq_provider,
        "deductiblePct": eq_deductible_pct,
        "deductibleUSD": eq_deductible_usd,
        "pgaG": pga_opt.map(|p| round_n(p, 3)),
        "pre1980CrippleWallRisk": pre1980_cripple_wall,
    });

    let mut sources = vec![
        SourceItem::new(
            "FEMA National Flood Hazard Layer (NFHL) & NFIP Risk Rating 2.0 Actuarial Framework",
            "https://www.fema.gov/flood-insurance/risk-rating",
        ),
        SourceItem::new(
            "FEMA National Risk Index (NRI) County & Census Tract Peril Loss Models",
            "https://hazards.fema.gov/nri/",
        ),
        SourceItem::new(
            "USGS ASCE 7-22 Seismic Design Maps & California Earthquake Authority (CEA) Rating Factors",
            "https://www.earthquakeauthority.com/",
        ),
        SourceItem::new(
            "CAL FIRE Fire Hazard Severity Zones / USFS Wildfire Hazard Potential & State FAIR / Wind Pool Rules",
            "https://www.cfpnet.com/",
        ),
    ];

    let mut map_layers: Vec<Value> = Vec::new();
    if let Some(ref fp) = resolved_fire_prot {
        let fp_url = fp
            .get("sourceUrl")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(FIRE_PROTECTION_HOMEPAGE_URL);
        if !sources.iter().any(|s| s.url == fp_url) {
            sources.push(SourceItem::new(FIRE_PROTECTION_SOURCE_NAME, fp_url));
        }
        if let Some(ml) = fp.get("mapLayer") {
            let has_feats = ml
                .get("features")
                .and_then(Value::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if has_feats {
                map_layers.push(ml.clone());
            }
        }
    }

    let mut roof_uw = json!({
        "materialClass": roof_ctx.material_class,
        "estimatedAgeYears": roof_ctx.effective_age_years,
        "carrierAgeCutoffYears": roof_age_cutoff,
        "hasVerifiedReroofPermit": roof_ctx.has_reroof_permit,
        "latestReroofPermitYear": roof_ctx.latest_reroof_year,
        "verificationStatus": roof_ctx.verification_status,
        "effectiveYearSource": roof_ctx.effective_year_source,
        "estimatedAgeFromYearBuilt": roof_ctx.estimated_age_from_year_built,
        "exceedsCarrierAgeCutoff": roof_over_cutoff,
    });
    if lidar_canopy.get("evaluated").and_then(Value::as_bool) == Some(true) {
        if let Some(obj) = roof_uw.as_object_mut() {
            obj.insert("lidarCanopyOverhang".to_string(), lidar_canopy);
        }
    }

    let mut out_data = json!({
        "status": "public_benchmark",
        "insurabilityTier": insurability_tier,
        "insurabilityScore": insurability_score,
        "coverageAUSD": coverage_a_usd,
        "dwellingReconstruction": {
            "coverageAUSD": coverage_a_usd,
            "baseCostPerSqftUSD": base_cost_per_sqft.round() as i64,
            "effectiveRebuildPerSqftUSD": effective_rebuild_per_sqft.round() as i64,
            "livingAreaSqft": sqft.round() as i64,
            "stories": stories_opt,
            "yearBuilt": yb_opt,
            "isCondoWallsIn": is_condo,
            "isHillsideTerrain": is_hillside,
            "multipliers": {
                "stories": stories_factor,
                "vintageBuildingCode": vintage_code_factor,
                "terrainSlope": terrain_slope_factor,
                "customFinish": custom_finish_factor,
            },
            "garageAllowanceUSD": garage_allowance_usd,
            "auxiliaryOrAduAllowanceUSD": adu_or_aux_usd,
            "ordinanceAndLaw10PctUSD": ordinance_law_10pct_usd,
            "extendedReplacementCost125USD": extended_125_usd,
            "referenceMarketValueUSD": ref_market_usd_opt,
            "referenceMarketValueSource": ref_market_source_opt,
            "impliedLandResidualUSD": implied_land_residual_opt,
            "rebuildToMarketRatioPct": rebuild_to_market_pct_opt,
            "reconstructionVsMarketNote": reconstruction_vs_market_note,
        },
        "annualPremiumRangeUSD": {
            "low": total_low,
            "expected": total_expected,
            "high": total_high,
            "includesMandatoryFlood": flood_mandatory,
            "excludesOptionalEarthquake": true,
            "withRecommendedFloodAndQuakeExpectedUSD": all_perils_expected,
            "baseHomeownersOrFairPlan": base_breakdown,
            "dicWrap": dic_breakdown,
            "floodRequiredOrRecommended": flood_breakdown,
            "earthquakeOptional": eq_breakdown,
        },
        "perilBreakdown": {
            "baseHomeownersOrFairPlan": base_breakdown,
            "dicWrap": dic_breakdown,
            "floodRequiredOrRecommended": flood_breakdown,
            "earthquakeOptional": eq_breakdown,
        },
        "naiveLenderEstimateUSD": naive_lender_annual_opt,
        "realisticExpectedUSD": realistic_expected_annual,
        "naiveMonthlyUSD": naive_monthly_opt,
        "realisticMonthlyUSD": realistic_monthly,
        "monthlyPaymentShockUSD": monthly_payment_shock_opt,
        "paymentShockNote": payment_shock_note,
        "roofUnderwriting": roof_uw,
        "underwritingTriggers": {
            "poolOrNonRenewal": nonrenewal_pool_triggers,
            "surchargeDrivers": surcharged_triggers,
        },
        "vulnerabilityFlags": vulnerability_flags,
        "mitigationDiscountsAndChecklist": checklist,
        "disclaimer": "Modeled homeowners, wildfire, flood, and seismic insurance estimate for residential due-diligence budgeting only — NOT a bindable insurance quote, coverage binder, or appraisal. Confirm insurability, 5-year LexisNexis C.L.U.E. claims history, and binding rates directly with licensed insurers.",
    });

    if let Some(fp) = resolved_fire_prot {
        if let Some(obj) = out_data.as_object_mut() {
            obj.insert("fireProtection".to_string(), fp);
            if !map_layers.is_empty() {
                obj.insert("mapLayers".to_string(), Value::Array(map_layers));
            }
        }
    }

    ModuleOutput {
        data: out_data,
        sources,
        warnings: Vec::new(),
    }
}

fn fmt_commas(n: i64) -> String {
    let neg = n < 0;
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    let rev: String = out.chars().rev().collect();
    if neg {
        format!("-{rev}")
    } else {
        rev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn insurance_consumes_fire_protection_hydrants_extension_and_lidar_canopy() {
        let app = Router::new()
            .route(
                "/fema_far",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "NAME": "REMOTE VALLEY STATION 9",
                                "CITY": "SAN JOSE",
                                "STATE": "CA",
                                "ZIP": "95120"
                            },
                            "geometry": { "x": -121.961797, "y": 37.3800 }
                        }]
                    }))
                }),
            )
            .route(
                "/empty_fc",
                get(|| async { Json(json!({ "features": [] })) }),
            )
            .route(
                "/empty_ovp",
                get(|| async { Json(json!({ "elements": [] })) }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let env = EnvConfig::from_map(HashMap::from([
            (
                "FIRE_STATIONS_FEMA_URL".to_string(),
                format!("http://{addr}/fema_far"),
            ),
            (
                "FIRE_STATIONS_USGS_URL".to_string(),
                format!("http://{addr}/empty_fc"),
            ),
            (
                "FIRE_STATIONS_OSM_URL".to_string(),
                format!("http://{addr}/empty_fc"),
            ),
            (
                "FIRE_HYDRANTS_OSM_URL".to_string(),
                format!("http://{addr}/empty_fc"),
            ),
            (
                "OVERPASS_ENDPOINTS".to_string(),
                format!("http://{addr}/empty_ovp"),
            ),
        ]));
        let http = HttpClient::offline_loopback_only(None);

        let facts = json!({
            "sqft": 2150,
            "yearBuilt": 2018,
            "roof": "Composition shingle"
        });
        let geo = json!({
            "state": "CA",
            "countyFips": "06085",
            "city": "San Jose",
            "lat": 37.282147,
            "lon": -121.961797
        });
        let house3d = json!({
            "metrics": {
                "canopyNearRoof": true,
                "tallestTreeM": 12.4,
                "tallestTreeDistM": 1.2,
                "treesMapped": 4
            }
        });

        let out = run_insurance_with_fire_protection(
            &http,
            &env,
            &facts,
            &Value::Null,
            &Value::Null,
            &geo,
            None,
            Some(&house3d),
            Some(1_800_000),
        )
        .await;

        assert_eq!(
            out.data["fireProtection"]["isoPpcProxy"]["ppcClass"].as_i64(),
            Some(10)
        );
        assert_eq!(
            out.data["insurabilityTier"].as_str(),
            Some("High Non-Renewal / State FAIR or Wind Pool Risk")
        );
        let flags = out.data["vulnerabilityFlags"].as_array().unwrap();
        assert!(flags.iter().any(|f| f["id"] == "iso_ppc_10_unprotected"));
        assert!(flags.iter().any(|f| f["id"] == "lidar_roof_canopy_overhang"));
        let checklist = out.data["mitigationDiscountsAndChecklist"]
            .as_array()
            .unwrap();
        assert!(checklist
            .iter()
            .any(|c| c["id"] == "lidar_arborist_canopy_trim"));
        assert!(checklist
            .iter()
            .any(|c| c["id"] == "rural_fire_suppression_water"));
        assert_eq!(
            out.data["mapLayers"][0]["id"].as_str(),
            Some("fire-protection")
        );

        // Disabling `fire_protection_hydrants` on an isolated ExtensionHub cleanly skips lookup
        let custom_hub = crate::extensions::ExtensionHub::new_default();
        custom_hub
            .enable(FIRE_PROTECTION_EXT_ID, false)
            .expect("disable fire_protection_hydrants");
        let disabled_fp = fetch_fire_protection_via_hub_ref(
            &custom_hub,
            &http,
            &env,
            &facts,
            &Value::Null,
            &Value::Null,
            &geo,
            None,
            Some(&house3d),
        )
        .await;
        assert!(disabled_fp.is_none());

        srv.abort();
    }
}
