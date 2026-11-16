// Walkability, Schools & Crime module (`civic`):
// - EPA National Walkability Index (Smart Location Database v3 block group)
// - NCES EDGE public & private schools within 2 mi + NCES SABS 2015–16 attendance boundaries. School scores
//   come only from official public academic data: the California School Dashboard (CDE) All-Students tiers,
//   CAASPP within-state percentiles in CA, and federal EDFacts within-state percentiles in other states
//   (`school_metrics`). Student–teacher ratios stay facts.
//   Strict Fair-Housing guardrails: zero race/demographic fields are requested or read.
// - FBI Crime Data Explorer (`cde.ucr.cjis.gov/LATEST` keyless primary + `api.usa.gov` fallback) agency crime trends
//   plus optional municipal incident open-data supplement (`CRIME_MUNICIPAL_ENDPOINTS_JSON` / Socrata adapters).

use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration as ChronoDuration, Utc};
use geo::Point;
use geojson::{Feature, Geometry};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use url::Url;

use crate::db::SchoolMetricRow;
use crate::modules::school_metrics::{
    metric_year, state_metrics_for_report, CAASPP_LIST_URL, EDFACTS_INFO_URL,
    NO_PUBLIC_SCHOOL_DATA_NOTICE,
};
use crate::shared::{
    bearing_deg, compass16, haversine_m, round_n, EnvConfig, HttpClient, ModuleOutput, SourceItem,
    MILE_M,
};

const URL_SLD: &str = "https://services.arcgis.com/cJ9YHowT8TU7DUyn/ArcGIS/rest/services/Smart_Location_Database_/FeatureServer/0";
const URL_WALK_MAP: &str = "https://geodata.epa.gov/arcgis/rest/services/OA/WalkabilityIndex/MapServer/0";
const URL_NCES: &str = "https://nces.ed.gov/opengis/rest/services/K12_School_Locations";
const URL_SABS: &str = "https://nces.ed.gov/opengis/rest/services/K12_School_Locations/SABS_1516/MapServer/0";
/// California School Dashboard school table, published in the California Department of Education's own
/// ArcGIS account. Only `CDSCode`, `FedID`, level/grade fields and All-Students academic tiers are ever
/// requested from it; its demographic columns are never read.
pub const URL_CA_DASHBOARD: &str = "https://services3.arcgis.com/fdvHcZVgB2QSRNkL/arcgis/rest/services/SchoolSitesNavigatorTable2025/FeatureServer/0";
/// `scoreSource` of a school score computed from California School Dashboard tiers.
pub const SCORE_SOURCE_CA_DASHBOARD: &str = "California School Dashboard (CDE)";
/// School year of the NCES School Attendance Boundary Survey layer (`SABS_1516`).
pub const SABS_YEAR: &str = "2015-16";
/// Env var holding configured municipal crime open-data endpoints (Socrata SoQL / JSON).
pub const ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON: &str = "CRIME_MUNICIPAL_ENDPOINTS_JSON";
/// Optional Socrata application token for higher rate limits on city open-data portals.
pub const ENV_SOCRATA_APP_TOKEN: &str = "SOCRATA_APP_TOKEN";
/// Disable built-in municipal open-data adapters when `CRIME_MUNICIPAL_ENDPOINTS_JSON` is unset.
pub const ENV_CRIME_MUNICIPAL_DISABLE_BUILTIN: &str = "CRIME_MUNICIPAL_DISABLE_BUILTIN";

const URL_CDE_DIRECT: &str = "https://cde.ucr.cjis.gov/LATEST";
const URL_CDE_API: &str = "https://api.usa.gov/crime/fbi/cde";
/// California DOJ OpenJustice open-data file store ("Crimes & Clearances", the state UCR program).
pub const URL_CA_DOJ_DATASETS: &str = "https://data-openjustice.doj.ca.gov/sites/default/files/dataset";
pub const CA_DOJ_SOURCE_NAME: &str =
    "California DOJ OpenJustice — Crimes & Clearances (state UCR program)";
/// Time allowed for locating and downloading the CA DOJ file (≈6 MB, cached for 30 days).
const CA_DOJ_BACKFILL_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

const PUBLIC_YEARS: &[&str] = &["2425", "2324", "2223"];
const PRIVATE_YEARS: &[&str] = &["2324", "2122"];
const SCHOOL_RADIUS_MI: f64 = 2.0;
const MAX_PUBLIC: usize = 40;
const MAX_PRIVATE: usize = 25;
const CRIME_YEARS: i32 = 10;

static DIST_STRIP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(school|schools|district|public|the|of|sd|isd|usd|esd|uhsd|cusd|juhsd|jesd|county office of education)\b").unwrap()
});
static DIST_CORE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(elementary|secondary|high|middle|junior|union|unified|joint|city|common)\b").unwrap()
});
static CHOICE_MAGNET_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)magnet|innovation|alternative|community day|continuation|virtual|online|special|montessori|immersion").unwrap()
});
static COUNTY_SUFFIX_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\s+(county|parish|borough|census area|city and borough|municipality|municipio)$").unwrap()
});
static PLACE_SUFFIX_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\s+(city|town|village|borough|municipality|CDP|city and borough|urban county|consolidated government.*|metropolitan government.*|unified government.*)$").unwrap()
});

fn clean_str(v: Option<&Value>) -> Option<String> {
    let raw = match v? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() { None } else { Some(collapsed) }
}

fn num_f64(v: Option<&Value>) -> Option<f64> {
    let x = v?;
    x.as_f64()
        .or_else(|| x.as_str()?.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite())
}

fn title_case(s: &str) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() || t != t.to_uppercase() {
        return t;
    }
    let mut out = String::with_capacity(t.len());
    let mut cap_next = true;
    for ch in t.to_lowercase().chars() {
        if cap_next && ch.is_ascii_alphabetic() {
            out.push(ch.to_ascii_uppercase());
            cap_next = false;
        } else {
            out.push(ch);
            cap_next = matches!(ch, ' ' | '-' | '/' | '(');
        }
    }
    for (from, to) in [(" Of ", " of "), (" And ", " and "), (" The ", " the "), (" At ", " at "), (" In ", " in ")] {
        out = out.replace(from, to);
    }
    out
}

fn fmt_mi(mi: f64) -> String {
    if mi < 0.1 {
        format!("{} ft", (mi * 5280.0).round() as i64)
    } else {
        format!("{mi:.2} mi")
    }
}

fn fmt_comma_int(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    let mut rev: String = out.chars().rev().collect();
    if neg {
        rev.insert(0, '-');
    }
    rev
}

fn fmt_rate(v: Option<f64>) -> String {
    match v {
        None => "n/a".to_string(),
        Some(x) => {
            let whole = x.trunc() as i64;
            let frac = ((x - whole as f64).abs() * 10.0).round() as i64;
            if frac == 0 {
                fmt_comma_int(whole)
            } else {
                format!("{}.{frac}", fmt_comma_int(whole))
            }
        }
    }
}

fn walk_category(v: f64) -> &'static str {
    if v <= 5.75 {
        "Least walkable"
    } else if v <= 10.5 {
        "Below average walkable"
    } else if v <= 15.25 {
        "Above average walkable"
    } else {
        "Most walkable"
    }
}

async fn fetch_walkability(http: &HttpClient, lat: f64, lon: f64, block_group: Option<&str>) -> Result<(Value, &'static str)> {
    let fields = "GEOID10,GEOID20,NatWalkInd,D2A_Ranked,D2B_Ranked,D3B_Ranked,D4A_Ranked,D3B,D4A";
    let mut attr: Option<Value> = None;
    let mut used = URL_SLD;
    let mut first_err: Option<anyhow::Error> = None;

    match http.arcgis_point_query(URL_SLD, lat, lon, fields, &[], 8500).await {
        Ok(mut rows) if !rows.is_empty() => attr = Some(rows.swap_remove(0)),
        Ok(_) => {}
        Err(e) => first_err = Some(e),
    }
    if attr.is_none() {
        match http.arcgis_point_query(URL_WALK_MAP, lat, lon, fields, &[], 8500).await {
            Ok(mut rows) if !rows.is_empty() => {
                attr = Some(rows.swap_remove(0));
                used = URL_WALK_MAP;
            }
            Ok(_) => {}
            Err(e) if first_err.is_none() => first_err = Some(e),
            Err(_) => {}
        }
    }
    if attr.is_none() {
        if let Some(bg) = block_group {
            let digits: String = bg.chars().filter(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                if let Ok(j) = http
                    .arcgis_query(
                        URL_SLD,
                        &[
                            ("where", format!("GEOID20='{digits}'")),
                            ("outFields", fields.to_string()),
                            ("returnGeometry", "false".to_string()),
                        ],
                        8500,
                    )
                    .await
                {
                    attr = j.pointer("/features/0/attributes").cloned();
                    used = URL_SLD;
                }
            }
        }
    }
    let Some(a) = attr else {
        return Err(first_err.unwrap_or_else(|| anyhow!("no block group found")));
    };
    let idx = num_f64(a.get("NatWalkInd")).map(|v| round_n(v, 2));
    let transit_m = num_f64(a.get("D4A")).filter(|&v| v >= 0.0).map(|v| v.round() as i64);
    let d3b = num_f64(a.get("D3B")).filter(|&v| v >= 0.0).map(|v| round_n(v, 1));
    let bg_out = clean_str(a.get("GEOID20")).or_else(|| clean_str(a.get("GEOID10")));

    Ok((
        json!({
            "index": idx,
            "scale": "1 (least walkable) – 20 (most walkable)",
            "category": idx.map(walk_category),
            "blockGroup": bg_out,
            "components": {
                "intersectionDensity": { "rank": a.get("D3B_Ranked"), "perSqMi": d3b },
                "transitProximity": { "rank": a.get("D4A_Ranked"), "metersToNearestStop": transit_m },
                "employmentMix": { "rank": a.get("D2B_Ranked") },
                "employmentAndHousingMix": { "rank": a.get("D2A_Ranked") },
            },
            "note": "EPA Smart Location Database v3 (2021; 2018-era inputs). Ranks are national 1–20 quantiles; index = D2A/6 + D2B/6 + D3B/3 + D4A/3. Block-group average — not a parcel-level walkability score.",
        }),
        used,
    ))
}

fn fmt_grade(g: Option<&Value>) -> Option<String> {
    let t = clean_str(g)?;
    match t.as_str() {
        "PK" => Some("PK".to_string()),
        "KG" => Some("K".to_string()),
        "UG" => Some("Ungraded".to_string()),
        "AE" => Some("Adult".to_string()),
        "M" | "N" => None,
        other => other.parse::<i32>().ok().map(|n| n.to_string()).or(Some(t)),
    }
}

fn school_level(level_txt: Option<&str>, lo: Option<&str>, hi: Option<&str>) -> &'static str {
    match level_txt.unwrap_or("").to_ascii_lowercase().as_str() {
        "elementary" => return "elementary",
        "middle" => return "middle",
        "high" | "secondary" => return "high",
        "prekindergarten" => return "preschool",
        "adult education" => return "adult",
        _ => {}
    }
    let g_num = |g: Option<&str>| -> Option<i32> {
        match g? {
            "PK" => Some(-1),
            "K" => Some(0),
            s => s.parse::<i32>().ok(),
        }
    };
    if let (Some(a), Some(b)) = (g_num(lo), g_num(hi)) {
        if b <= 5 {
            return "elementary";
        }
        if a >= 9 {
            return "high";
        }
        if a >= 5 && b <= 9 {
            return "middle";
        }
        if a <= 5 && (6..=8).contains(&b) {
            return "elementary/middle";
        }
        if a <= 8 && b >= 12 {
            return if a <= 5 { "K-12" } else { "middle/high" };
        }
    }
    "other"
}

fn norm_alnum_spaces(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn norm_district(s: &str) -> String {
    let replaced = s.to_ascii_lowercase().replace('&', " and ");
    let stripped = DIST_STRIP_RE.replace_all(&replaced, " ");
    norm_alnum_spaces(&stripped)
}

fn core_district(s: &str) -> String {
    let nd = norm_district(s);
    let stripped = DIST_CORE_RE.replace_all(&nd, " ");
    norm_alnum_spaces(&stripped)
}

fn matches_assigned_district(district: Option<&str>, level: &str, assigned: Option<&Value>) -> Option<bool> {
    let asg = assigned?;
    let uni = clean_str(asg.get("unified"));
    let elem = clean_str(asg.get("elementary"));
    let sec = clean_str(asg.get("secondary"));
    if uni.is_none() && elem.is_none() && sec.is_none() {
        return None;
    }
    let Some(dist) = district else { return Some(false) };
    let nd = norm_district(dist);
    let cd = core_district(dist);
    if let Some(ref u) = uni {
        if nd == norm_district(u) || (!cd.is_empty() && cd == core_district(u)) {
            return Some(true);
        }
    }
    if let Some(ref e) = elem {
        if matches!(level, "elementary" | "middle" | "elementary/middle" | "K-12")
            && (nd == norm_district(e) || (!cd.is_empty() && cd == core_district(e) && !dist.to_ascii_lowercase().contains("high")))
        {
            return Some(true);
        }
    }
    if let Some(ref s) = sec {
        if matches!(level, "high" | "middle/high" | "K-12")
            && (nd == norm_district(s) || (!cd.is_empty() && cd == core_district(s) && !dist.to_ascii_lowercase().contains("elementary")))
        {
            return Some(true);
        }
    }
    Some(false)
}

async fn fetch_public_schools(http: &HttpClient, lat: f64, lon: f64, assigned: Option<&Value>) -> Result<(Vec<Value>, String, String)> {
    let fields = "NCESSCH,SCH_NAME,LEA_NAME,LEAID,GSLO,GSHI,SCHOOL_LEVEL,CHARTER_TEXT,VIRTUAL,SCHOOL_TYPE_TEXT,SY_STATUS_TEXT,MEMBER,TOTAL,STUTERATIO,LSTREET1,LCITY,PHONE";
    let mut feats: Option<Vec<Value>> = None;
    let mut used_y = "";
    let mut used_url = String::new();
    let mut last_err = anyhow!("NCES public school layers unavailable");

    for &y in PUBLIC_YEARS {
        let u = format!("{URL_NCES}/EDGE_ADMINDATA_PUBLICSCH_{y}/MapServer/1");
        match http.arcgis_radius_query(&u, lat, lon, SCHOOL_RADIUS_MI * MILE_M * 1.02, fields, &[], 9500).await {
            Ok(f) => {
                feats = Some(f);
                used_y = y;
                used_url = u;
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some(feats) = feats else { return Err(last_err) };

    let mut list = Vec::new();
    for f in feats {
        let Some(a) = f.get("attributes") else { continue };
        let status = clean_str(a.get("SY_STATUS_TEXT")).unwrap_or_default().to_ascii_lowercase();
        if status.contains("closed") || status.contains("inactive") || status.contains("future") {
            continue;
        }
        if clean_str(a.get("VIRTUAL")).map(|s| s.eq_ignore_ascii_case("full virtual")).unwrap_or(false) {
            continue;
        }
        let Some(slat) = f.pointer("/geometry/y").and_then(|v| v.as_f64()) else { continue };
        let Some(slon) = f.pointer("/geometry/x").and_then(|v| v.as_f64()) else { continue };
        let d_m = haversine_m(lat, lon, slat, slon);
        if d_m > SCHOOL_RADIUS_MI * MILE_M {
            continue;
        }
        let lo = fmt_grade(a.get("GSLO"));
        let hi = fmt_grade(a.get("GSHI"));
        let grades = match (&lo, &hi) {
            (Some(l), Some(h)) if l == h => Some(l.clone()),
            (Some(l), Some(h)) => Some(format!("{l}–{h}")),
            _ => None,
        };
        let enrollment = num_f64(a.get("MEMBER"))
            .filter(|&x| x >= 0.0)
            .or_else(|| num_f64(a.get("TOTAL")).filter(|&x| x >= 0.0))
            .map(|x| x.round() as i64);
        let district = clean_str(a.get("LEA_NAME"));
        let s_name = clean_str(a.get("SCH_NAME")).unwrap_or_else(|| "Public School".to_string());
        let s_city = clean_str(a.get("LCITY")).map(|c| title_case(&c)).unwrap_or_default();
        // A fact about staffing, never turned into a score or grade.
        let st_ratio = num_f64(a.get("STUTERATIO")).filter(|&x| x > 0.0).map(|x| round_n(x, 1));
        let search_q = urlencoding::encode(&[s_name.as_str(), s_city.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")).into_owned();
        let lvl = school_level(clean_str(a.get("SCHOOL_LEVEL")).as_deref(), lo.as_deref(), hi.as_deref());
        let in_dist = matches_assigned_district(district.as_deref(), lvl, assigned);
        let nces_id = clean_str(a.get("NCESSCH"));
        let d_mi = round_n(d_m / MILE_M, 2);
        let addr_parts: Vec<String> = [clean_str(a.get("LSTREET1")), if s_city.is_empty() { None } else { Some(s_city) }]
            .into_iter()
            .flatten()
            .collect();

        list.push(json!({
            "name": s_name,
            "level": lvl,
            "grades": grades,
            "district": district,
            "inAttendanceZone": false,
            "inAssignedDistrict": in_dist.unwrap_or(false),
            "type": clean_str(a.get("SCHOOL_TYPE_TEXT")),
            "charter": clean_str(a.get("CHARTER_TEXT")).as_deref() == Some("Yes"),
            "enrollment": enrollment,
            "studentTeacherRatio": st_ratio,
            "address": addr_parts.join(", "),
            "phone": clean_str(a.get("PHONE")),
            "distanceMi": d_mi,
            "distMi": d_mi,
            "direction": compass16(bearing_deg(lat, lon, slat, slon)),
            "lat": round_n(slat, 5),
            "lon": round_n(slon, 5),
            "ncesId": nces_id,
            "url": nces_id.as_deref().map(|id| format!("https://nces.ed.gov/ccd/schoolsearch/school_detail.asp?ID={id}")),
            "gsUrl": format!("https://www.greatschools.org/search/search.page?q={search_q}"),
            "nicheUrl": format!("https://www.niche.com/k12/search/best-schools/?q={search_q}"),
        }));
    }
    list.sort_by(|a, b| {
        let da = a["distanceMi"].as_f64().unwrap_or(99.0);
        let db = b["distanceMi"].as_f64().unwrap_or(99.0);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok((list, format!("20{}-{}", &used_y[..2], &used_y[2..]), used_url))
}

async fn fetch_private_schools(http: &HttpClient, lat: f64, lon: f64) -> Result<(Vec<Value>, String, String)> {
    let mut last_err = anyhow!("NCES private school layers unavailable");
    for &y in PRIVATE_YEARS {
        let u = format!("{URL_NCES}/EDGE_GEOCODE_PRIVATESCH_{y}/MapServer/0");
        match http.arcgis_radius_query(&u, lat, lon, SCHOOL_RADIUS_MI * MILE_M * 1.02, "PPIN,NAME,STREET,CITY,SCHOOLYEAR", &[], 9500).await {
            Ok(feats) => {
                let mut list = Vec::new();
                for f in feats {
                    let Some(a) = f.get("attributes") else { continue };
                    let Some(slat) = f.pointer("/geometry/y").and_then(|v| v.as_f64()) else { continue };
                    let Some(slon) = f.pointer("/geometry/x").and_then(|v| v.as_f64()) else { continue };
                    let d_m = haversine_m(lat, lon, slat, slon);
                    if d_m > SCHOOL_RADIUS_MI * MILE_M {
                        continue;
                    }
                    let s_name = clean_str(a.get("NAME")).map(|s| title_case(&s)).unwrap_or_else(|| "Private School".to_string());
                    let s_city = clean_str(a.get("CITY")).map(|s| title_case(&s)).unwrap_or_default();
                    let s_street = clean_str(a.get("STREET")).map(|s| title_case(&s));
                    let search_q = urlencoding::encode(&[s_name.as_str(), s_city.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")).into_owned();
                    let ppin = clean_str(a.get("PPIN"));
                    let d_mi = round_n(d_m / MILE_M, 2);
                    let addr = [s_street, if s_city.is_empty() { None } else { Some(s_city) }].into_iter().flatten().collect::<Vec<_>>().join(", ");
                    list.push(json!({
                        "name": s_name,
                        "address": addr,
                        "distanceMi": d_mi,
                        "distMi": d_mi,
                        "direction": compass16(bearing_deg(lat, lon, slat, slon)),
                        "lat": round_n(slat, 5),
                        "lon": round_n(slon, 5),
                        "ppin": ppin,
                        "url": ppin.as_deref().map(|id| format!("https://nces.ed.gov/surveys/pss/privateschoolsearch/school_detail.asp?ID={id}")),
                        "gsUrl": format!("https://www.greatschools.org/search/search.page?q={search_q}"),
                        "nicheUrl": format!("https://www.niche.com/k12/search/best-schools/?q={search_q}"),
                    }));
                }
                list.sort_by(|a, b| {
                    let da = a["distanceMi"].as_f64().unwrap_or(99.0);
                    let db = b["distanceMi"].as_f64().unwrap_or(99.0);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });
                return Ok((list, format!("20{}-{}", &y[..2], &y[2..]), u));
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn score_to_letter(n: Option<f64>) -> Option<&'static str> {
    let x = n.filter(|&v| v.is_finite() && v > 0.0)?;
    Some(if x >= 9.0 { "A+" } else if x >= 8.0 { "A" } else if x >= 7.5 { "A-" } else if x >= 7.0 { "B+" } else if x >= 6.0 { "B" } else if x >= 5.5 { "B-" } else if x >= 5.0 { "C+" } else if x >= 4.0 { "C" } else { "D" })
}

/// A 0–10 school score computed only from official public academic data.
#[derive(Debug, Clone, PartialEq)]
pub struct OfficialSchoolScore {
    /// 0–10, one decimal.
    pub score: f64,
    /// Publisher label shown next to the number, e.g. "California School Dashboard (CDE)".
    pub source: String,
    /// School year of the underlying data, e.g. "2024-25".
    pub year: String,
    /// Short label for compact displays.
    pub badge: &'static str,
}

const DASHBOARD_TIER_KEYS: [&str; 5] = ["elaTier", "mathTier", "sciTier", "cciTier", "gradTier"];

/// Official score of one school object (a `schools.public.list[]` entry or an `assignedOrNearest` card).
///
/// Only official academic fields are read: the within-state `percentile` (`CAASPP` / `school_metrics`,
/// divided by 10) when present, otherwise the California School Dashboard All-Students tiers under
/// `caDashboard` (mean of the available 1–5 tiers × 2). It never reads `score`, `rating`, `gsRating`,
/// `nicheGrade`, `staffingScore`, `studentTeacherRatio`, `readingProficiencyPct` or `estGrade`, so values
/// written by older report versions or gathered by web research can never reach a score.
/// `None` means unknown.
pub fn official_school_score(school: &Value) -> Option<OfficialSchoolScore> {
    if let Some(p) = num_f64(school.get("percentile")).filter(|p| (0.0..=100.0).contains(p)) {
        if let Some(src) = clean_str(school.get("metricSource")) {
            let badge = if src.to_ascii_uppercase().contains("EDFACTS") {
                "EDFacts"
            } else {
                "CAASPP"
            };
            return Some(OfficialSchoolScore {
                score: round_n(p / 10.0, 1),
                source: src,
                year: clean_str(school.get("metricYear")).unwrap_or_default(),
                badge,
            });
        }
    }
    let d = school.get("caDashboard").filter(|v| v.is_object())?;
    let tiers: Vec<f64> = DASHBOARD_TIER_KEYS
        .iter()
        .filter_map(|k| num_f64(d.get(*k)))
        .filter(|t| (1.0..=5.0).contains(t))
        .collect();
    if tiers.is_empty() {
        return None;
    }
    let mean = tiers.iter().sum::<f64>() / tiers.len() as f64;
    Some(OfficialSchoolScore {
        score: round_n(mean * 2.0, 1),
        source: SCORE_SOURCE_CA_DASHBOARD.to_string(),
        year: clean_str(d.get("year")).unwrap_or_else(|| "2024-25".to_string()),
        badge: "CA Dashboard",
    })
}

fn percentile_detail(school: &Value, default_state: &str) -> Option<String> {
    let mut parts = Vec::new();
    let prof = school.get("caaspp").or_else(|| school.get("edfacts"));
    if let Some(c) = prof {
        if let Some(e) = num_f64(c.get("elaPct")) {
            parts.push(format!("ELA {}%", e.round() as i64));
        }
        if let Some(m) = num_f64(c.get("mathPct")) {
            parts.push(format!("Math {}%", m.round() as i64));
        }
    }
    if let Some(p) = num_f64(school.get("percentile")) {
        let lvl = clean_str(school.get("percentileLevel")).unwrap_or_default();
        let st = clean_str(school.get("metricState")).unwrap_or_else(|| default_state.to_string());
        if lvl.is_empty() {
            parts.push(format!("{p:.1} {st} pct."));
        } else {
            parts.push(format!("{p:.1} {st} {lvl} pct."));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn dashboard_detail(d: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(v) = d.get("elaTier").and_then(|x| x.as_i64()) {
        parts.push(format!("ELA {v}/5"));
    }
    if let Some(v) = d.get("mathTier").and_then(|x| x.as_i64()) {
        parts.push(format!("Math {v}/5"));
    }
    if let Some(v) = d.get("cciTier").and_then(|x| x.as_i64()) {
        parts.push(format!("CCI {v}/5"));
    } else if let Some(v) = d.get("sciTier").and_then(|x| x.as_i64()) {
        parts.push(format!("Sci {v}/5"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Attaches ingested public metrics (within-state `percentile`, ELA/Math proficiency, source, year)
/// onto school objects matching by `ncesId`. Does not remove `caDashboard` tiers or URLs.
pub fn apply_school_metrics(
    schools: &mut [Value],
    metrics: &HashMap<String, SchoolMetricRow>,
) -> usize {
    if metrics.is_empty() {
        return 0;
    }
    let mut matched = 0usize;
    for s in schools.iter_mut() {
        let Some(id) = clean_str(s.get("ncesId")) else { continue };
        let Some(m) = metrics.get(&id) else { continue };
        let Some(obj) = s.as_object_mut() else { continue };
        obj.insert("percentile".into(), json!(m.percentile));
        obj.insert("percentileLevel".into(), json!(m.level));
        obj.insert("metricSource".into(), json!(m.source));
        obj.insert("metricYear".into(), json!(metric_year(m.year)));
        obj.insert("metricState".into(), json!(m.state));
        let prof_payload = json!({
            "elaPct": m.ela_pct,
            "mathPct": m.math_pct,
            "combinedPct": m.combined_pct,
        });
        if m.state.eq_ignore_ascii_case("CA") {
            obj.insert("caaspp".into(), prof_payload);
        } else {
            obj.insert("edfacts".into(), prof_payload);
        }
        matched += 1;
    }
    matched
}

/// Writes the official score fields onto a school object: `score`, `scoreSource`, `scoreYear`,
/// `scoreBadge`, `scoreDetail` and `estGrade` (a letter derived from the official score only).
/// Without official data every one of them is null, meaning unknown.
pub fn apply_official_score(school: &mut Value) {
    let official = official_school_score(school);
    let detail = match &official {
        Some(o) if o.badge == "CAASPP" => percentile_detail(school, "CA"),
        Some(o) if o.badge == "EDFacts" => percentile_detail(school, "state"),
        Some(o) if o.source == SCORE_SOURCE_CA_DASHBOARD => school.get("caDashboard").and_then(dashboard_detail),
        _ => None,
    };
    let Some(obj) = school.as_object_mut() else { return };
    match official {
        Some(o) => {
            obj.insert("score".into(), json!(o.score));
            obj.insert("scoreSource".into(), json!(o.source));
            obj.insert("scoreYear".into(), json!(o.year));
            obj.insert("scoreBadge".into(), json!(o.badge));
            obj.insert("scoreDetail".into(), json!(detail));
            obj.insert("estGrade".into(), json!(score_to_letter(Some(o.score))));
        }
        None => {
            for k in ["score", "scoreSource", "scoreYear", "scoreBadge", "scoreDetail", "estGrade"] {
                obj.insert(k.into(), Value::Null);
            }
        }
    }
}

/// The assigned (or nearest) public school for one level, with its official score.
#[derive(Debug, Clone, PartialEq)]
pub struct AssignedSchool {
    /// "elementary", "middle" or "high".
    pub level: &'static str,
    pub name: String,
    pub nces_id: Option<String>,
    /// `None` when the school has no official academic data (unknown).
    pub official: Option<OfficialSchoolScore>,
}

/// One assigned-or-nearest public school per level, as chosen by the civic module from public data
/// (NCES CCD directory, NCES SABS 2015–16 attendance zones, Census school districts).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssignedSchools {
    pub elementary: Option<AssignedSchool>,
    pub middle: Option<AssignedSchool>,
    pub high: Option<AssignedSchool>,
}

impl AssignedSchools {
    pub fn iter(&self) -> impl Iterator<Item = &AssignedSchool> {
        [&self.elementary, &self.middle, &self.high].into_iter().flatten()
    }

    /// Mean of the available official scores (one decimal); `None` when no assigned school has any.
    pub fn summary_score(&self) -> Option<f64> {
        let scores: Vec<f64> = self.iter().filter_map(|a| a.official.as_ref().map(|o| o.score)).collect();
        if scores.is_empty() {
            None
        } else {
            Some(round_n(scores.iter().sum::<f64>() / scores.len() as f64, 1))
        }
    }

    /// Distinct sources of the scores behind [`Self::summary_score`], in level order.
    pub fn sources(&self) -> Vec<String> {
        distinct_in_order(self.iter().filter_map(|a| a.official.as_ref().map(|o| o.source.clone())))
    }

    /// Distinct school years of the scores behind [`Self::summary_score`], in level order.
    pub fn years(&self) -> Vec<String> {
        distinct_in_order(self.iter().filter_map(|a| a.official.as_ref().map(|o| o.year.clone())))
    }
}

fn distinct_in_order(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for it in items {
        if !it.is_empty() && !out.contains(&it) {
            out.push(it);
        }
    }
    out
}

/// The assigned-or-nearest public school per level from `modules.civic.data`, scored with
/// [`official_school_score`]: `schools.assignedOrNearest` (by role), then
/// `schools.public.nearestInAssignedDistrictByLevel`, then `schools.public.nearestByLevel`.
/// Never reads `facts`, so GreatSchools / Niche values from web research cannot reach a score.
pub fn assigned_official_schools(civic_data: &Value) -> AssignedSchools {
    let sc = civic_data.get("schools").unwrap_or(&Value::Null);
    let cards = sc.get("assignedOrNearest").and_then(|v| v.as_array());
    let pick = |lv: &'static str| -> Option<AssignedSchool> {
        let from_cards = cards.and_then(|arr| {
            arr.iter().find(|c| {
                c.get("role")
                    .and_then(|r| r.as_str())
                    .map(|r| r.eq_ignore_ascii_case(lv))
                    .unwrap_or(false)
            })
        });
        let found = from_cards
            .or_else(|| sc.pointer(&format!("/public/nearestInAssignedDistrictByLevel/{lv}")).filter(|v| v.is_object()))
            .or_else(|| sc.pointer(&format!("/public/nearestByLevel/{lv}")).filter(|v| v.is_object()))?;
        Some(AssignedSchool {
            level: lv,
            name: clean_str(found.get("name"))?,
            nces_id: clean_str(found.get("ncesId")),
            official: official_school_score(found),
        })
    };
    AssignedSchools {
        elementary: pick("elementary"),
        middle: pick("middle"),
        high: pick("high"),
    }
}

/// Attendance zones (NCES SABS 2015–16) and official academic data (CAASPP percentiles from
/// `school_metrics` + California School Dashboard All-Students tiers) for the public schools near
/// the home, then the official score fields on each.
async fn enrich_public_schools(
    http: &HttpClient,
    env: &EnvConfig,
    public_list: &mut [Value],
    lat: f64,
    lon: f64,
    geo: &Value,
) -> (Vec<SourceItem>, Vec<String>) {
    let mut sources = Vec::new();
    let warnings = Vec::new();
    let nces_ids: Vec<String> = public_list
        .iter()
        .filter_map(|s| s.get("ncesId").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .filter(|id| (11..=12).contains(&id.len()) && id.chars().all(|c| c.is_ascii_digit()))
        .take(40)
        .collect();
    let in_clause = nces_ids.iter().map(|id| format!("'{id}'")).collect::<Vec<_>>().join(",");
    let state_abbr = geo
        .get("state")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()));
    let is_ca = state_abbr.map(|s| s.eq_ignore_ascii_case("CA")).unwrap_or(false);

    let (r_sabs, r_ca, state_metrics) = tokio::join!(
        http.arcgis_point_query(URL_SABS, lat, lon, "ncessch,schnam,leaid,gslo,gshi,level,openEnroll", &[], 8000),
        async {
            if !is_ca || in_clause.is_empty() {
                return Err(anyhow!("skip ca dash"));
            }
            // Strictly All-Students academic tiers only — zero demographic/race columns
            http.arcgis_query(
                URL_CA_DASHBOARD,
                &[
                    ("where", format!("FedID IN ({in_clause})")),
                    ("outFields", "FedID,YearData,ELAcolorALL,MathcolorALL,ScicolorALL,CCIcolorALL,GradcolorALL,DashboardURL".to_string()),
                    ("returnGeometry", "false".to_string()),
                ],
                8000,
            )
            .await
        },
        async {
            if nces_ids.is_empty() {
                HashMap::new()
            } else {
                state_metrics_for_report(None, env, state_abbr, &nces_ids).await
            }
        },
    );

    if let Ok(rows) = r_sabs {
        let zone_ids: HashSet<String> = rows
            .iter()
            .filter_map(|r| clean_str(r.get("ncessch")))
            .collect();
        if !zone_ids.is_empty() {
            for s in public_list.iter_mut() {
                if let Some(id) = s.get("ncesId").and_then(|v| v.as_str()) {
                    if zone_ids.contains(id) {
                        s["inAttendanceZone"] = json!(true);
                        s["inAssignedDistrict"] = json!(true);
                    }
                }
            }
            sources.push(SourceItem::new("NCES School Attendance Boundary Survey (SABS), 2015–16 boundaries", URL_SABS));
        }
    }

    if let Ok(j) = r_ca {
        if let Some(feats) = j.get("features").and_then(|v| v.as_array()) {
            let mut by_fed = HashMap::new();
            for f in feats {
                if let Some(a) = f.get("attributes") {
                    if let Some(id) = clean_str(a.get("FedID")) {
                        by_fed.insert(id, a.clone());
                    }
                }
            }
            let tier = |v: Option<&Value>| num_f64(v).map(|x| x.round() as i64).filter(|&n| (1..=5).contains(&n));
            let mut matched = 0usize;
            for s in public_list.iter_mut() {
                let Some(id) = s.get("ncesId").and_then(|v| v.as_str()) else { continue };
                let Some(a) = by_fed.get(id) else { continue };
                let d_url = clean_str(a.get("DashboardURL"));
                if let Some(ref u) = d_url {
                    s["dashboardUrl"] = json!(u);
                }
                let ela = tier(a.get("ELAcolorALL"));
                let math = tier(a.get("MathcolorALL"));
                let sci = tier(a.get("ScicolorALL"));
                let cci = tier(a.get("CCIcolorALL"));
                let grad = tier(a.get("GradcolorALL"));
                if ela.is_some() || math.is_some() || sci.is_some() || cci.is_some() || grad.is_some() {
                    let yr = clean_str(a.get("YearData")).unwrap_or_else(|| "2024-25".to_string());
                    s["caDashboard"] = json!({
                        "year": yr,
                        "elaTier": ela,
                        "mathTier": math,
                        "sciTier": sci,
                        "cciTier": cci,
                        "gradTier": grad,
                        "url": d_url,
                    });
                    matched += 1;
                }
            }
            if matched > 0 {
                sources.push(SourceItem::new("California Dept. of Education School Dashboard (2024-25)", "https://www.caschooldashboard.org/"));
            }
        }
    }

    let metrics_matched = apply_school_metrics(public_list, &state_metrics);
    if metrics_matched > 0 {
        for src in distinct_in_order(
            public_list
                .iter()
                .filter_map(|s| clean_str(s.get("metricSource"))),
        ) {
            if src.to_ascii_uppercase().contains("EDFACTS") {
                sources.push(SourceItem::new(
                    format!("U.S. Dept. of Education — {src}"),
                    EDFACTS_INFO_URL,
                ));
            } else {
                sources.push(SourceItem::new(
                    format!("California Dept. of Education — {src}"),
                    CAASPP_LIST_URL,
                ));
            }
        }
    }

    for s in public_list.iter_mut() {
        apply_official_score(s);
    }

    (sources, warnings)
}

pub fn match_agency(agencies_json: &Value, geo: &Value, lat: f64, lon: f64) -> Option<(Value, String)> {
    let county_raw = geo.get("county").and_then(|v| v.as_str()).unwrap_or("");
    let county = COUNTY_SUFFIX_RE.replace(county_raw, "").trim().to_ascii_uppercase();
    let place_raw = geo
        .get("place")
        .or_else(|| geo.get("city"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_cdp = place_raw.trim_end().ends_with("CDP") || place_raw.trim_end().ends_with("cdp");
    let place = norm_alnum_spaces(&PLACE_SUFFIX_RE.replace(place_raw, ""));

    let mut all: Vec<Value> = Vec::new();
    let mut by_county: Vec<Value> = Vec::new();
    if let Some(obj) = agencies_json.as_object() {
        for (k, arr_v) in obj {
            if let Some(arr) = arr_v.as_array() {
                if !county.is_empty() && k.eq_ignore_ascii_case(&county) {
                    by_county.extend(arr.iter().cloned());
                }
                all.extend(arr.iter().cloned());
            }
        }
    } else if let Some(arr) = agencies_json.as_array() {
        all.extend(arr.iter().cloned());
    }
    if by_county.is_empty() && !county.is_empty() {
        by_county = all
            .iter()
            .filter(|a| {
                a.get("counties")
                    .and_then(|v| v.as_str())
                    .map(|c| c.to_ascii_uppercase().contains(&county))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
    }
    let mut pool = if !by_county.is_empty() { by_county } else { all };
    if lat.is_finite() && lon.is_finite() {
        pool.sort_by(|a, b| {
            let dist = |x: &Value| -> f64 {
                match (x.get("latitude").and_then(|v| v.as_f64()), x.get("longitude").and_then(|v| v.as_f64())) {
                    (Some(la), Some(lo)) => haversine_m(lat, lon, la, lo),
                    _ => 1e9,
                }
            };
            dist(a).partial_cmp(&dist(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    if !place.is_empty() && !is_cdp {
        let city_pds: Vec<&Value> = pool
            .iter()
            .filter(|a| {
                a.get("agency_type_name").and_then(|v| v.as_str()) == Some("City")
                    && a.get("agency_name")
                        .and_then(|v| v.as_str())
                        .map(|n| n.to_ascii_lowercase().contains("police"))
                        .unwrap_or(false)
            })
            .collect();
        if let Some(&exact) = city_pds.iter().find(|a| {
            let nm = norm_alnum_spaces(a.get("agency_name").and_then(|v| v.as_str()).unwrap_or(""));
            nm.strip_suffix(" police department").unwrap_or(&nm) == place
        }) {
            return Some((exact.clone(), format!("city police department for \"{place_raw}\"")));
        }
        let prefix = format!("{place} ");
        if let Some(&starts) = city_pds.iter().find(|a| {
            norm_alnum_spaces(a.get("agency_name").and_then(|v| v.as_str()).unwrap_or("")).starts_with(&prefix)
        }) {
            return Some((starts.clone(), format!("city police department for \"{place_raw}\" (name prefix match)")));
        }
        if let Some(any_city) = pool.iter().find(|a| {
            a.get("agency_type_name").and_then(|v| v.as_str()) == Some("City")
                && norm_alnum_spaces(a.get("agency_name").and_then(|v| v.as_str()).unwrap_or("")).starts_with(&prefix)
        }) {
            return Some((any_city.clone(), format!("city agency for \"{place_raw}\"")));
        }
    }

    let sheriff = pool
        .iter()
        .find(|a| {
            a.get("agency_type_name").and_then(|v| v.as_str()) == Some("County")
                && a.get("agency_name")
                    .and_then(|v| v.as_str())
                    .map(|n| n.to_ascii_lowercase().contains("sheriff"))
                    .unwrap_or(false)
        })
        .or_else(|| pool.iter().find(|a| a.get("agency_type_name").and_then(|v| v.as_str()) == Some("County")));
    sheriff.map(|s| {
        let reason = if is_cdp || place.is_empty() {
            "county sheriff (unincorporated area / CDP)".to_string()
        } else {
            format!("county sheriff (no city agency matched \"{place_raw}\")")
        };
        (s.clone(), reason)
    })
}

#[derive(Debug, Clone)]
struct AnnualPoint {
    year: i32,
    count: Option<i64>,
    rate: Option<f64>,
    state_rate: Option<f64>,
    nat_rate: Option<f64>,
    population: Option<i64>,
}

fn annualize_cde(j: &Value, agency_name: &str, from: i32, to: i32) -> (Vec<AnnualPoint>, Option<String>, Option<String>) {
    let empty = Map::new();
    let rates = j.pointer("/offenses/rates").and_then(|v| v.as_object()).unwrap_or(&empty);
    let actuals = j.pointer("/offenses/actuals").and_then(|v| v.as_object()).unwrap_or(&empty);
    let pops = j.pointer("/populations/population").and_then(|v| v.as_object()).unwrap_or(&empty);
    let cov = j.pointer("/tooltips/Percent of Population Coverage").and_then(|v| v.as_object()).unwrap_or(&empty);

    let a_key = actuals
        .keys()
        .find(|k| k.ends_with(" Offenses") && !k.starts_with("United States"))
        .cloned()
        .unwrap_or_else(|| format!("{agency_name} Offenses"));
    let agency_label = a_key.strip_suffix(" Offenses").unwrap_or(&a_key).to_string();
    let state_key = rates
        .keys()
        .find(|k| k.ends_with(" Offenses") && k.as_str() != "United States Offenses" && k.as_str() != a_key)
        .cloned();
    let state_name = state_key.as_deref().and_then(|k| k.strip_suffix(" Offenses")).map(|s| s.to_string());

    let mut out = Vec::new();
    for y in from..=to {
        let months: Vec<String> = (1..=12).map(|m| format!("{m:02}-{y}")).collect();
        let cnt: Vec<Option<f64>> = months.iter().map(|m| actuals.get(&a_key).and_then(|o| num_f64(o.get(m)))).collect();
        let s_rate: Vec<Option<f64>> = months
            .iter()
            .map(|m| state_key.as_ref().and_then(|sk| rates.get(sk)).and_then(|o| num_f64(o.get(m))))
            .collect();
        let n_rate: Vec<Option<f64>> = months
            .iter()
            .map(|m| rates.get("United States Offenses").and_then(|o| num_f64(o.get(m))))
            .collect();

        let pop = pops
            .get(&agency_label)
            .and_then(|po| {
                num_f64(po.get(&format!("01-{y}")))
                    .or_else(|| months.iter().find_map(|m| num_f64(po.get(m))))
            })
            .map(|p| p.round() as i64);

        let count = if cnt.iter().all(|v| v.is_some()) {
            Some(cnt.iter().flatten().sum::<f64>().round() as i64)
        } else {
            None
        };
        let min_cov = |nm: &str| -> Option<f64> {
            let vals: Vec<f64> = months.iter().filter_map(|m| cov.get(nm).and_then(|o| num_f64(o.get(m)))).collect();
            if vals.is_empty() { None } else { Some(vals.into_iter().fold(f64::INFINITY, f64::min)) }
        };
        let st_cov = state_name.as_deref().and_then(min_cov);
        let us_cov = min_cov("United States");

        out.push(AnnualPoint {
            year: y,
            count,
            rate: match (count, pop) {
                (Some(c), Some(p)) if p > 0 => Some(round_n((c as f64 / p as f64) * 100_000.0, 1)),
                _ => None,
            },
            state_rate: if s_rate.iter().all(|v| v.is_some()) && st_cov.map(|c| c >= 60.0).unwrap_or(true) {
                Some(round_n(s_rate.iter().flatten().sum::<f64>(), 1))
            } else {
                None
            },
            nat_rate: if n_rate.iter().all(|v| v.is_some()) && us_cov.map(|c| c >= 60.0).unwrap_or(true) {
                Some(round_n(n_rate.iter().flatten().sum::<f64>(), 1))
            } else {
                None
            },
            population: pop,
        });
    }
    let data_through = clean_str(j.pointer("/cde_properties/max_data_date/UCR"));
    (out, state_name, data_through)
}

/// One agency-year from the California DOJ "Crimes & Clearances" file, on the FBI's summary
/// definitions: `violent` = homicide + rape + robbery + aggravated assault; `property` =
/// burglary + larceny-theft + motor-vehicle theft + arson.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateCrimeYear {
    pub violent: i64,
    pub property: i64,
}

/// Download candidates for the CA DOJ file, newest first. DOJ publishes each July with data
/// through the prior calendar year, under `…/{publication year}-07/…-1985-{data year}.csv`.
pub fn ca_doj_crime_urls(today_year: i32) -> Vec<String> {
    (0..3)
        .map(|k| {
            let pub_year = today_year - k;
            format!(
                "{URL_CA_DOJ_DATASETS}/{pub_year}-07/Crimes_and_Clearances_with_Arson-1985-{}.csv",
                pub_year - 1
            )
        })
        .collect()
}

/// Normalised county names an FBI agency record covers ("SANTA CLARA" -> `santa clara`).
fn norm_county_names(counties: &str) -> Vec<String> {
    counties
        .split([',', ';', '/'])
        .map(|c| {
            let n = norm_alnum_spaces(c);
            n.strip_suffix(" county").unwrap_or(&n).to_string()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// The name under which the CA DOJ file lists an FBI agency, normalised with
/// `norm_alnum_spaces`: city departments are listed by city ("Campbell"), sheriffs as
/// "<County> Co. Sheriff's Department". Other agency types are not matched.
pub fn ca_doj_agency_key(agency_name: &str, agency_type: &str, counties: &str) -> Option<String> {
    let name = norm_alnum_spaces(agency_name);
    match agency_type {
        "City" => {
            const SUFFIXES: &[&str] = &[
                " police department",
                " police dept",
                " department of public safety",
                " dept of public safety",
                " public safety department",
                " public safety",
                " police",
            ];
            let city = SUFFIXES
                .iter()
                .find_map(|suf| name.strip_suffix(suf))
                .unwrap_or(&name)
                .trim()
                .to_string();
            (!city.is_empty()).then_some(city)
        }
        "County" if name.contains("sheriff") => norm_county_names(counties)
            .into_iter()
            .next()
            .map(|county| format!("{county} co sheriff s department")),
        _ => None,
    }
}

/// Extracts one agency's annual totals from the CA DOJ CSV text. Returns an empty map when the
/// text is not that file (e.g. a redirect page) or the agency is not listed.
pub fn parse_ca_doj_series(csv_text: &str, counties: &str, agency_key: &str) -> HashMap<i32, StateCrimeYear> {
    let mut out = HashMap::new();
    let text = csv_text.trim_start_matches('\u{feff}');
    if !text.starts_with("Year,County,NCICCode") {
        return out;
    }
    let county_keys = norm_county_names(counties);
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(text.as_bytes());
    let Ok(headers) = rdr.headers().cloned() else { return out };
    let col = |name: &str| headers.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let (Some(i_year), Some(i_county), Some(i_agency), Some(i_violent), Some(i_property)) = (
        col("Year"),
        col("County"),
        col("NCICCode"),
        col("Violent_sum"),
        col("Property_sum"),
    ) else {
        return out;
    };
    let arson_cols: Vec<usize> = ["TotalStructural_sum", "TotalMobile_sum", "TotalOther_sum"]
        .iter()
        .filter_map(|n| col(n))
        .collect();
    let int_at = |rec: &csv::StringRecord, idx: usize| -> Option<i64> {
        rec.get(idx).and_then(|v| v.trim().parse::<f64>().ok()).map(|f| f.round() as i64)
    };
    for rec in rdr.records().flatten() {
        if norm_alnum_spaces(rec.get(i_agency).unwrap_or("")) != agency_key {
            continue;
        }
        let county = norm_alnum_spaces(rec.get(i_county).unwrap_or(""));
        let county = county.strip_suffix(" county").unwrap_or(&county);
        if !county_keys.is_empty() && !county_keys.iter().any(|c| c == county) {
            continue;
        }
        let (Some(year), Some(violent), Some(property)) = (
            rec.get(i_year).and_then(|v| v.trim().parse::<i32>().ok()),
            int_at(&rec, i_violent),
            int_at(&rec, i_property),
        ) else {
            continue;
        };
        // The FBI property-crime total includes arson; DOJ reports arson in separate columns.
        let arson: i64 = arson_cols.iter().filter_map(|&i| int_at(&rec, i)).sum();
        out.insert(year, StateCrimeYear { violent, property: property + arson });
    }
    out
}

fn year_count(y: &Value, kind: &str) -> Option<i64> {
    y.pointer(&format!("/{kind}/count")).and_then(|v| v.as_i64())
}

/// Guards the backfill against a wrong agency match: at least two years reported by both the FBI
/// and the state must agree within 3 % (or 3 offenses) on both the violent and property totals.
pub fn state_series_matches_fbi(years: &[Value], state: &HashMap<i32, StateCrimeYear>) -> bool {
    let close = |a: i64, b: i64| (a - b).abs() as f64 <= (a.max(b) as f64 * 0.03).max(3.0);
    let agreeing = years
        .iter()
        .filter(|y| y["complete"].as_bool() == Some(true))
        .filter(|y| {
            let Some(st) = y["year"].as_i64().and_then(|yr| state.get(&(yr as i32))) else {
                return false;
            };
            match (year_count(y, "violent"), year_count(y, "property")) {
                (Some(v), Some(p)) => close(v, st.violent) && close(p, st.property),
                _ => false,
            }
        })
        .count();
    agreeing >= 2
}

/// Fills years the FBI series is missing from the state series and returns the years filled.
/// Rates use the FBI population for that year (nearest reported year within two years otherwise).
pub fn backfill_crime_gap_years(years: &mut [Value], state: &HashMap<i32, StateCrimeYear>) -> Vec<i64> {
    let populations: Vec<(i64, f64)> = years
        .iter()
        .filter_map(|y| Some((y["year"].as_i64()?, y["population"].as_f64().filter(|p| *p > 0.0)?)))
        .collect();
    let mut filled = Vec::new();
    for y in years.iter_mut() {
        if y["complete"].as_bool() == Some(true) {
            continue;
        }
        let Some(year) = y["year"].as_i64() else { continue };
        let Some(st) = state.get(&(year as i32)) else { continue };
        let own_pop = y["population"].as_f64().filter(|p| *p > 0.0);
        let pop = own_pop.or_else(|| {
            populations
                .iter()
                .filter(|(py, _)| (py - year).abs() <= 2)
                .min_by_key(|(py, _)| (py - year).abs())
                .map(|(_, p)| *p)
        });
        let Some(pop) = pop else { continue };
        for (kind, count) in [("violent", st.violent), ("property", st.property)] {
            if !y[kind].is_object() {
                y[kind] = json!({});
            }
            y[kind]["count"] = json!(count);
            y[kind]["ratePer100k"] = json!(round_n(count as f64 / pop * 100_000.0, 1));
        }
        if own_pop.is_none() {
            y["population"] = json!(pop.round() as i64);
            y["populationEstimated"] = json!(true);
        }
        y["complete"] = json!(true);
        y["backfilled"] = json!(true);
        y["source"] = json!("California DOJ OpenJustice");
        filled.push(year);
    }
    filled.sort_unstable();
    filled
}

/// Downloads the CA DOJ file (newest publication that exists) and returns the agency's series.
async fn fetch_ca_doj_series(
    http: &HttpClient,
    counties: &str,
    agency_key: &str,
) -> Option<(HashMap<i32, StateCrimeYear>, String)> {
    for url in ca_doj_crime_urls(Utc::now().year()) {
        match http.get_text_opts(&url, 12_000, 0, 86_400 * 30, None).await {
            Ok(text) => {
                let series = parse_ca_doj_series(&text, counties, agency_key);
                if !series.is_empty() {
                    return Some((series, url));
                }
                // Not published yet (redirect page) or agency not listed in this edition.
            }
            Err(e) => tracing::debug!(error = %e, "CA DOJ crime file edition unavailable"),
        }
    }
    None
}

async fn fetch_agency_series(
    http: &HttpClient,
    ori: &str,
    agency_name: &str,
    from: i32,
    to: i32,
    api_key: &str,
    root: &str,
) -> Result<(Vec<Value>, Option<String>, Option<String>, String, Vec<String>)> {
    let base = format!("{root}/summarized/agency/{}", urlencoding::encode(ori));
    let q = if root == URL_CDE_DIRECT {
        format!("from=01-{from}&to=12-{to}")
    } else {
        format!("from=01-{from}&to=12-{to}&API_KEY={}", urlencoding::encode(api_key))
    };
    let v_url = format!("{base}/violent-crime?{q}");
    let p_url = format!("{base}/property-crime?{q}");
    let (r_v, r_p) = tokio::join!(
        http.get_json_opts(&v_url, 9500, 0, 86400 * 7, None),
        http.get_json_opts(&p_url, 9500, 0, 86400 * 7, None),
    );
    if r_v.is_err() && r_p.is_err() {
        return Err(anyhow!("CDE summarized data unavailable: {:?}", r_v.err()));
    }
    let mut warnings = Vec::new();
    let v_parsed = match r_v {
        Ok(ref j) => Some(annualize_cde(j, agency_name, from, to)),
        Err(e) => { warnings.push(format!("CDE violent-crime series failed: {e}")); None }
    };
    let p_parsed = match r_p {
        Ok(ref j) => Some(annualize_cde(j, agency_name, from, to)),
        Err(e) => { warnings.push(format!("CDE property-crime series failed: {e}")); None }
    };

    let mut years = Vec::new();
    for y in (from..=to).rev() {
        let vy = v_parsed.as_ref().and_then(|vp| vp.0.iter().find(|x| x.year == y));
        let py = p_parsed.as_ref().and_then(|pp| pp.0.iter().find(|x| x.year == y));
        let pick = |o: Option<&AnnualPoint>| match o {
            Some(pt) => json!({
                "count": pt.count,
                "ratePer100k": pt.rate,
                "stateRatePer100k": pt.state_rate,
                "nationalRatePer100k": pt.nat_rate,
            }),
            None => Value::Null,
        };
        let complete = vy.and_then(|x| x.count).is_some() && py.and_then(|x| x.count).is_some();
        years.push(json!({
            "year": y,
            "population": vy.and_then(|x| x.population).or_else(|| py.and_then(|x| x.population)),
            "violent": pick(vy),
            "property": pick(py),
            "complete": complete,
        }));
    }
    let st_name = v_parsed.as_ref().and_then(|x| x.1.clone()).or_else(|| p_parsed.as_ref().and_then(|x| x.1.clone()));
    let through = v_parsed.as_ref().and_then(|x| x.2.clone()).or_else(|| p_parsed.as_ref().and_then(|x| x.2.clone()));
    Ok((years, st_name, through, base, warnings))
}

async fn fetch_crime_stats(
    http: &HttpClient,
    geo: &Value,
    lat: f64,
    lon: f64,
    api_key: &str,
) -> Result<(Value, Vec<String>, Vec<String>, Vec<SourceItem>)> {
    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("").trim().to_ascii_uppercase();
    if st.len() != 2 || !st.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(anyhow!("crime: geo.state (2-letter) required"));
    }
    let roots = if !api_key.is_empty() && api_key != "DEMO_KEY" {
        [URL_CDE_DIRECT, URL_CDE_API]
    } else {
        [URL_CDE_DIRECT, URL_CDE_API]
    };
    let mut agencies_val: Option<(Value, &'static str)> = None;
    let mut last_err = anyhow!("FBI CDE agency directory unavailable");
    for root in roots {
        let u = if root == URL_CDE_DIRECT {
            format!("{root}/agency/byStateAbbr/{st}")
        } else {
            format!("{root}/agency/byStateAbbr/{st}?API_KEY={}", urlencoding::encode(api_key))
        };
        match http.get_json_opts(&u, 9000, 0, 86400 * 14, None).await {
            Ok(j) if j.is_object() || j.is_array() => {
                agencies_val = Some((j, root));
                break;
            }
            Ok(_) => {}
            Err(e) => last_err = e,
        }
    }
    let Some((agencies_json, primary_root)) = agencies_val else { return Err(last_err) };
    let backup_root = if primary_root == URL_CDE_DIRECT { URL_CDE_API } else { URL_CDE_DIRECT };
    let mut used_root = primary_root;

    let Some((mut agency, mut matched_by)) = match_agency(&agencies_json, geo, lat, lon) else {
        return Err(anyhow!("no reporting agency matched geo"));
    };
    let to = Utc::now().year() - 1;
    let from = to - CRIME_YEARS + 1;

    let ori = agency.get("ori").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let ag_name = agency.get("agency_name").and_then(|v| v.as_str()).unwrap_or("Police Department").to_string();
    let mut series = match fetch_agency_series(http, &ori, &ag_name, from, to, api_key, primary_root).await {
        Ok(s) => s,
        Err(_) => {
            used_root = backup_root;
            fetch_agency_series(http, &ori, &ag_name, from, to, api_key, backup_root).await?
        }
    };
    let mut warnings = series.4.clone();

    let has_complete = series.0.iter().any(|y| y["complete"].as_bool() == Some(true));
    if !has_complete && agency.get("agency_type_name").and_then(|v| v.as_str()) == Some("City") {
        let county_only = json!({ "county": geo.get("county"), "place": "" });
        if let Some((fb_ag, _)) = match_agency(&agencies_json, &county_only, lat, lon) {
            let fb_ori = fb_ag.get("ori").and_then(|v| v.as_str()).unwrap_or("");
            if !fb_ori.is_empty() && fb_ori != ori {
                let fb_nm = fb_ag.get("agency_name").and_then(|v| v.as_str()).unwrap_or("County Sheriff");
                if let Ok(fb_s) = fetch_agency_series(http, fb_ori, fb_nm, from, to, api_key, used_root).await {
                    warnings.push(format!("{ag_name} has no complete reporting year {from}–{to}; using {fb_nm}."));
                    warnings.extend(fb_s.4.clone());
                    matched_by = format!("county sheriff fallback (city agency \"{ag_name}\" reported no complete year)");
                    agency = fb_ag;
                    series = fb_s;
                }
            }
        }
    }

    let (mut years, state_name, data_through, base_url, _) = series;

    // The FBI accepted only NIBRS submissions for 2021, which left a hole in the series of most
    // California agencies. The state UCR program kept collecting the same summary counts, so the
    // hole can be filled from California DOJ — but only once the two series are shown to agree.
    let mut extra_sources: Vec<SourceItem> = Vec::new();
    let mut backfill = Value::Null;
    let has_gap = years.iter().any(|y| y["complete"].as_bool() != Some(true));
    if st == "CA" && has_gap {
        let ag_type = agency.get("agency_type_name").and_then(|v| v.as_str()).unwrap_or("");
        let ag_counties = clean_str(agency.get("counties"))
            .or_else(|| clean_str(geo.get("county")))
            .unwrap_or_default();
        let ag_label = agency.get("agency_name").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(key) = ca_doj_agency_key(ag_label, ag_type, &ag_counties) {
            // Bounded: the backfill is an improvement, never a reason to hold the report up.
            let fetched = tokio::time::timeout(
                CA_DOJ_BACKFILL_BUDGET,
                fetch_ca_doj_series(http, &ag_counties, &key),
            )
            .await
            .ok()
            .flatten();
            if let Some((state_series, url)) = fetched {
                if state_series_matches_fbi(&years, &state_series) {
                    let filled = backfill_crime_gap_years(&mut years, &state_series);
                    if !filled.is_empty() {
                        extra_sources.push(SourceItem::new(CA_DOJ_SOURCE_NAME, url.clone()));
                        backfill = json!({ "source": CA_DOJ_SOURCE_NAME, "url": url, "years": filled });
                    }
                } else {
                    tracing::warn!(agency = ag_label, "CA DOJ crime series does not reconcile with the FBI series; gap years left empty");
                }
            }
        }
    }

    let latest = years.iter().find(|y| y["complete"].as_bool() == Some(true)).cloned();
    let five_back = latest.as_ref().and_then(|l| {
        let target_yr = l["year"].as_i64()? - 5;
        years.iter().find(|y| y["year"].as_i64() == Some(target_yr) && y["complete"].as_bool() == Some(true)).cloned()
    });
    let pct_delta = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) if y > 0.0 => Some(round_n(((x - y) / y) * 100.0, 1)),
        _ => None,
    };
    let change_5y = match (&latest, &five_back) {
        (Some(l), Some(fb)) => json!({
            "fromYear": fb["year"],
            "toYear": l["year"],
            "violentRatePct": pct_delta(l.pointer("/violent/ratePer100k").and_then(|v| v.as_f64()), fb.pointer("/violent/ratePer100k").and_then(|v| v.as_f64())),
            "propertyRatePct": pct_delta(l.pointer("/property/ratePer100k").and_then(|v| v.as_f64()), fb.pointer("/property/ratePer100k").and_then(|v| v.as_f64())),
        }),
        _ => Value::Null,
    };

    let gaps: Vec<String> = years
        .iter()
        .filter(|y| y["complete"].as_bool() != Some(true))
        .filter_map(|y| y["year"].as_i64().map(|n| n.to_string()))
        .collect();
    if !gaps.is_empty() {
        let agency_label = agency.get("agency_name").and_then(|v| v.as_str()).unwrap_or("The reporting agency");
        let nibrs_note = if gaps.iter().any(|g| g == "2021") {
            " (2021 is the year the FBI accepted only NIBRS submissions)"
        } else {
            ""
        };
        warnings.push(format!(
            "{agency_label} has no complete FBI crime figures for {}{nibrs_note}; those years are omitted from the crime trend.",
            gaps.join(", ")
        ));
    }
    let mut caveats = vec![
        "Agency-wide totals for the whole jurisdiction; they do not describe any particular block or neighborhood.".to_string(),
        "Counts are crimes reported to and by police; reporting practices differ between agencies and changed with the 2021 NIBRS transition.".to_string(),
        "Rates use the FBI population estimate for the agency jurisdiction. Annual rates are sums of the monthly CDE rates and can differ slightly from the FBI annual publications.".to_string(),
        "State/US comparison rates are omitted for years in which the FBI estimate covered < 60% of the population (e.g. California in 2021).".to_string(),
    ];
    if let Some(filled) = backfill.get("years").and_then(|y| y.as_array()) {
        let list = filled.iter().filter_map(|y| y.as_i64()).map(|y| y.to_string()).collect::<Vec<_>>().join(", ");
        caveats.push(format!(
            "{list} totals come from the California DOJ OpenJustice state UCR file because the FBI series has no complete figures for that period; they use the same summary offense definitions and were checked against the years both sources report."
        ));
    }

    let data = json!({
        "source": "FBI Uniform Crime Reporting — Crime Data Explorer (summarized agency data)",
        "agency": {
            "ori": agency.get("ori"),
            "name": agency.get("agency_name"),
            "type": agency.get("agency_type_name"),
            "county": clean_str(agency.get("counties")).map(|c| title_case(&c)),
            "nibrsSince": agency.get("nibrs_start_date"),
        },
        "matchedBy": matched_by,
        "state": state_name.unwrap_or_else(|| st.clone()),
        "dataThrough": data_through,
        "latest": latest,
        "change5y": change_5y,
        "years": years,
        "definitions": {
            "violent": "murder & non-negligent manslaughter, rape, robbery, aggravated assault",
            "property": "burglary, larceny-theft, motor-vehicle theft, arson",
        },
        "caveats": caveats,
        "backfill": backfill,
    });
    let urls = vec![
        format!("{used_root}/agency/byStateAbbr/{st}"),
        format!("{base_url}/violent-crime"),
        format!("{base_url}/property-crime"),
    ];
    Ok((data, warnings, urls, extra_sources))
}

// ---------------------------------------------------------------- Municipal crime open-data supplement

/// Configuration for one municipal police incident open-data endpoint (Socrata SoQL JSON or JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MunicipalCrimeEndpoint {
    pub name: String,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub bbox: Option<[f64; 4]>,
    #[serde(alias = "url", alias = "baseUrl", alias = "base_url")]
    pub endpoint: String,
    #[serde(default = "default_socrata_kind")]
    pub kind: String,
    #[serde(default, alias = "date_field")]
    pub date_field: Option<String>,
    #[serde(default, alias = "category_field")]
    pub category_field: Option<String>,
    #[serde(default, alias = "lat_field")]
    pub lat_field: Option<String>,
    #[serde(default, alias = "lon_field")]
    pub lon_field: Option<String>,
    #[serde(default, alias = "location_field")]
    pub location_field: Option<String>,
    #[serde(default, alias = "tract_field")]
    pub tract_field: Option<String>,
    #[serde(default = "default_municipal_radius_m", alias = "radius_m")]
    pub radius_meters: f64,
    #[serde(default = "default_lookback_days", alias = "lookback_days")]
    pub lookback_days: i64,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default, alias = "source_url")]
    pub source_url: Option<String>,
    #[serde(default, alias = "app_token")]
    pub app_token: Option<String>,
}

fn default_socrata_kind() -> String {
    "socrata".to_string()
}

fn default_municipal_radius_m() -> f64 {
    MILE_M
}

fn default_lookback_days() -> i64 {
    365
}

/// Well-known municipal Socrata open-data incident endpoints used when
/// `CRIME_MUNICIPAL_ENDPOINTS_JSON` is not explicitly configured.
pub fn builtin_municipal_crime_endpoints() -> Vec<MunicipalCrimeEndpoint> {
    vec![
        MunicipalCrimeEndpoint {
            name: "San Francisco PD Incident Reports (DataSF)".into(),
            city: Some("San Francisco".into()),
            state: Some("CA".into()),
            bbox: None,
            endpoint: "https://data.sfgov.org/resource/wg3w-h783.json".into(),
            kind: "socrata".into(),
            date_field: Some("incident_datetime".into()),
            category_field: Some("incident_category".into()),
            lat_field: Some("latitude".into()),
            lon_field: Some("longitude".into()),
            location_field: None,
            tract_field: None,
            radius_meters: MILE_M,
            lookback_days: 365,
            year: None,
            source_url: Some("https://data.sfgov.org/d/wg3w-h783".into()),
            app_token: None,
        },
        MunicipalCrimeEndpoint {
            name: "Austin Police Department Crime Reports (Austin Open Data)".into(),
            city: Some("Austin".into()),
            state: Some("TX".into()),
            bbox: None,
            endpoint: "https://data.austintexas.gov/resource/fdj4-gpfu.json".into(),
            kind: "socrata".into(),
            date_field: Some("occ_date".into()),
            category_field: Some("highest_offense_description".into()),
            lat_field: Some("latitude".into()),
            lon_field: Some("longitude".into()),
            location_field: None,
            tract_field: Some("census_tract".into()),
            radius_meters: MILE_M,
            lookback_days: 365,
            year: None,
            source_url: Some("https://data.austintexas.gov/d/fdj4-gpfu".into()),
            app_token: None,
        },
        MunicipalCrimeEndpoint {
            name: "Seattle Police Department Crime Data (Seattle Open Data)".into(),
            city: Some("Seattle".into()),
            state: Some("WA".into()),
            bbox: None,
            endpoint: "https://data.seattle.gov/resource/tazs-3rd5.json".into(),
            kind: "socrata".into(),
            date_field: Some("offense_start_datetime".into()),
            category_field: Some("offense".into()),
            lat_field: Some("latitude".into()),
            lon_field: Some("longitude".into()),
            location_field: None,
            tract_field: None,
            radius_meters: MILE_M,
            lookback_days: 365,
            year: None,
            source_url: Some("https://data.seattle.gov/d/tazs-3rd5".into()),
            app_token: None,
        },
        MunicipalCrimeEndpoint {
            name: "Chicago Police Department Crimes (Chicago Data Portal)".into(),
            city: Some("Chicago".into()),
            state: Some("IL".into()),
            bbox: None,
            endpoint: "https://data.cityofchicago.org/resource/ijzp-q8t2.json".into(),
            kind: "socrata".into(),
            date_field: Some("date".into()),
            category_field: Some("primary_type".into()),
            lat_field: Some("latitude".into()),
            lon_field: Some("longitude".into()),
            location_field: None,
            tract_field: None,
            radius_meters: MILE_M,
            lookback_days: 365,
            year: None,
            source_url: Some("https://data.cityofchicago.org/d/ijzp-q8t2".into()),
            app_token: None,
        },
        MunicipalCrimeEndpoint {
            name: "LAPD Crime Data from 2020 to Present (LA Open Data)".into(),
            city: Some("Los Angeles".into()),
            state: Some("CA".into()),
            bbox: None,
            endpoint: "https://data.lacity.org/resource/2nrs-mtv8.json".into(),
            kind: "socrata".into(),
            date_field: Some("date_occ".into()),
            category_field: Some("crm_cd_desc".into()),
            lat_field: Some("lat".into()),
            lon_field: Some("lon".into()),
            location_field: None,
            tract_field: None,
            radius_meters: MILE_M,
            lookback_days: 365,
            year: None,
            source_url: Some("https://data.lacity.org/d/2nrs-mtv8".into()),
            app_token: None,
        },
        MunicipalCrimeEndpoint {
            name: "NYPD Complaint Data (NYC Open Data)".into(),
            city: Some("New York".into()),
            state: Some("NY".into()),
            bbox: None,
            endpoint: "https://data.cityofnewyork.us/resource/5uac-w243.json".into(),
            kind: "socrata".into(),
            date_field: Some("cmplnt_fr_dt".into()),
            category_field: Some("ofns_desc".into()),
            lat_field: Some("latitude".into()),
            lon_field: Some("longitude".into()),
            location_field: None,
            tract_field: None,
            radius_meters: MILE_M,
            lookback_days: 365,
            year: None,
            source_url: Some("https://data.cityofnewyork.us/d/5uac-w243".into()),
            app_token: None,
        },
    ]
}

fn validate_http_endpoint(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed).map_err(|e| format!("invalid URL ({e})"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("endpoint must be an http(s) URL with a host".to_string());
    }
    Ok(trimmed.to_string())
}

fn parse_municipal_endpoint_val(key_name: Option<&str>, v: &Value) -> Result<MunicipalCrimeEndpoint, String> {
    let obj = v.as_object().ok_or_else(|| "endpoint entry must be an object".to_string())?;
    let raw_url = obj
        .get("endpoint")
        .or_else(|| obj.get("url"))
        .or_else(|| obj.get("baseUrl"))
        .or_else(|| obj.get("base_url"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let endpoint = validate_http_endpoint(raw_url)?;
    let city = obj.get("city").and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let state = obj.get("state").and_then(Value::as_str).map(|s| s.trim().to_ascii_uppercase()).filter(|s| !s.is_empty());
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| key_name.map(str::to_string))
        .unwrap_or_else(|| match (&city, &state) {
            (Some(c), Some(st)) => format!("{c}, {st} Municipal Crime Open Data"),
            (Some(c), None) => format!("{c} Municipal Crime Open Data"),
            _ => "Municipal Crime Open Data".to_string(),
        });
    let bbox = obj.get("bbox").and_then(|b| {
        let a = b.as_array()?;
        if a.len() != 4 {
            return None;
        }
        let coords = [a[0].as_f64()?, a[1].as_f64()?, a[2].as_f64()?, a[3].as_f64()?];
        if coords.iter().all(|x| x.is_finite()) && coords[0] <= coords[2] && coords[1] <= coords[3] {
            Some(coords)
        } else {
            None
        }
    });
    let radius_meters = obj
        .get("radiusMeters")
        .or_else(|| obj.get("radius_m"))
        .and_then(Value::as_f64)
        .or_else(|| obj.get("radiusMi").and_then(Value::as_f64).map(|mi| mi * MILE_M))
        .unwrap_or(MILE_M)
        .clamp(200.0, 10_000.0);
    let lookback_days = obj
        .get("lookbackDays")
        .or_else(|| obj.get("lookback_days"))
        .and_then(Value::as_i64)
        .unwrap_or(365)
        .clamp(30, 1825);
    let str_opt = |k1: &str, k2: &str| {
        obj.get(k1)
            .or_else(|| obj.get(k2))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok(MunicipalCrimeEndpoint {
        name,
        city,
        state,
        bbox,
        endpoint,
        kind: str_opt("kind", "type").unwrap_or_else(default_socrata_kind).to_ascii_lowercase(),
        date_field: str_opt("dateField", "date_field"),
        category_field: str_opt("categoryField", "category_field"),
        lat_field: str_opt("latField", "lat_field"),
        lon_field: str_opt("lonField", "lon_field"),
        location_field: str_opt("locationField", "location_field"),
        tract_field: str_opt("tractField", "tract_field"),
        radius_meters,
        lookback_days,
        year: obj.get("year").and_then(Value::as_i64).map(|y| y as i32),
        source_url: str_opt("sourceUrl", "source_url"),
        app_token: str_opt("appToken", "app_token"),
    })
}

/// Parses `CRIME_MUNICIPAL_ENDPOINTS_JSON` into validated endpoint configurations plus warnings.
pub fn parse_municipal_endpoints(env: &EnvConfig) -> (Vec<MunicipalCrimeEndpoint>, Vec<String>) {
    let Some(raw) = env.get(ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON) else {
        return (Vec::new(), Vec::new());
    };
    let mut warnings = Vec::new();
    let val: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("{ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON} is invalid JSON: {e}"));
            return (Vec::new(), warnings);
        }
    };
    let mut out = Vec::new();
    if let Some(arr) = val.as_array() {
        for (i, item) in arr.iter().enumerate() {
            match parse_municipal_endpoint_val(None, item) {
                Ok(ep) => out.push(ep),
                Err(e) => warnings.push(format!("{ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON}[{i}]: {e}")),
            }
        }
    } else if let Some(map) = val.as_object() {
        for (k, item) in map {
            match parse_municipal_endpoint_val(Some(k), item) {
                Ok(ep) => out.push(ep),
                Err(e) => warnings.push(format!("{ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON}[{k}]: {e}")),
            }
        }
    } else {
        warnings.push(format!("{ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON} must be a JSON array or object"));
    }
    (out, warnings)
}

fn normalize_city_name(raw: &str) -> String {
    norm_alnum_spaces(&PLACE_SUFFIX_RE.replace(raw.trim(), ""))
}

/// Selects the configured or built-in municipal crime endpoint matching `geo` / `(lat, lon)`.
pub fn select_municipal_endpoint<'a>(
    endpoints: &'a [MunicipalCrimeEndpoint],
    geo: &Value,
    lat: f64,
    lon: f64,
) -> Option<&'a MunicipalCrimeEndpoint> {
    let geo_state = geo
        .get("state")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_uppercase())
        .unwrap_or_default();
    let geo_city = geo
        .get("place")
        .or_else(|| geo.get("city"))
        .and_then(Value::as_str)
        .map(normalize_city_name)
        .unwrap_or_default();

    for ep in endpoints {
        if let Some(ref st) = ep.state {
            if !geo_state.is_empty() && !st.eq_ignore_ascii_case(&geo_state) {
                continue;
            }
        }
        if let Some(bbox) = ep.bbox {
            if lon >= bbox[0] && lon <= bbox[2] && lat >= bbox[1] && lat <= bbox[3] {
                if let Some(ref c) = ep.city {
                    if geo_city.is_empty() || normalize_city_name(c) == geo_city {
                        return Some(ep);
                    }
                } else {
                    return Some(ep);
                }
            }
        }
        if let Some(ref c) = ep.city {
            if !geo_city.is_empty() && normalize_city_name(c) == geo_city {
                if ep.state.is_none() || ep.state.as_deref() == Some(geo_state.as_str()) {
                    return Some(ep);
                }
            }
        }
    }
    None
}

/// Builds a Socrata SoQL (or JSON) query URL around `(lat, lon)` for the given endpoint.
pub fn build_municipal_crime_url(
    ep: &MunicipalCrimeEndpoint,
    lat: f64,
    lon: f64,
    since_iso_date: &str,
) -> Result<String> {
    let mut url = Url::parse(&ep.endpoint)?;
    if ep.kind == "socrata" {
        let mut clauses: Vec<String> = Vec::new();
        let dlat = ep.radius_meters / 111_320.0;
        let dlon = ep.radius_meters / (111_320.0 * lat.to_radians().cos().abs().max(0.2));
        if let Some(ref loc_f) = ep.location_field {
            clauses.push(format!(
                "within_circle({loc_f}, {lat:.5}, {lon:.5}, {:.0})",
                ep.radius_meters
            ));
        } else if let (Some(ref lat_f), Some(ref lon_f)) = (&ep.lat_field, &ep.lon_field) {
            clauses.push(format!(
                "{lat_f} >= {:.5} AND {lat_f} <= {:.5} AND {lon_f} >= {:.5} AND {lon_f} <= {:.5}",
                lat - dlat,
                lat + dlat,
                lon - dlon,
                lon + dlon
            ));
        }
        if let Some(ref df) = ep.date_field {
            if !since_iso_date.is_empty() {
                clauses.push(format!("{df} >= '{since_iso_date}'"));
            }
        }
        let mut qp = url.query_pairs_mut();
        qp.append_pair("$limit", "500");
        if !clauses.is_empty() {
            qp.append_pair("$where", &clauses.join(" AND "));
        }
    }
    Ok(url.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncidentBucket {
    Violent,
    Property,
    Other,
}

fn classify_incident_category(raw: &str) -> IncidentBucket {
    let l = raw.to_ascii_lowercase();
    if l.contains("murder")
        || l.contains("homicide")
        || l.contains("manslaughter")
        || l.contains("rape")
        || l.contains("sexual assault")
        || l.contains("robbery")
        || l.contains("aggravated assault")
        || l.contains("felony assault")
        || l.contains("assault")
        || l.contains("shooting")
        || l.contains("kidnapping")
    {
        IncidentBucket::Violent
    } else if l.contains("burglary")
        || l.contains("larceny")
        || l.contains("theft")
        || l.contains("stolen")
        || l.contains("motor vehicle")
        || l.contains("auto")
        || l.contains("arson")
        || l.contains("vandalism")
        || l.contains("shoplift")
        || l.contains("break")
        || l.contains("property")
    {
        IncidentBucket::Property
    } else {
        IncidentBucket::Other
    }
}

fn extract_incident_coords(row: &Value, ep: &MunicipalCrimeEndpoint) -> Option<(f64, f64)> {
    let attr = row.get("attributes").unwrap_or(row);
    let lat = ep
        .lat_field
        .as_deref()
        .and_then(|k| num_f64(attr.get(k)))
        .or_else(|| num_f64(attr.get("latitude")))
        .or_else(|| num_f64(attr.get("lat")))
        .or_else(|| row.pointer("/geometry/y").and_then(Value::as_f64));
    let lon = ep
        .lon_field
        .as_deref()
        .and_then(|k| num_f64(attr.get(k)))
        .or_else(|| num_f64(attr.get("longitude")))
        .or_else(|| num_f64(attr.get("lon")))
        .or_else(|| num_f64(attr.get("lng")))
        .or_else(|| row.pointer("/geometry/x").and_then(Value::as_f64));
    if let (Some(la), Some(lo)) = (lat, lon) {
        if la.is_finite() && lo.is_finite() && la.abs() <= 90.0 && lo.abs() <= 180.0 && (la != 0.0 || lo != 0.0) {
            return Some((la, lo));
        }
    }
    for key in [
        ep.location_field.as_deref(),
        Some("location"),
        Some("point"),
        Some("geocoded_column"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(loc) = attr.get(key) {
            if let (Some(la), Some(lo)) = (num_f64(loc.get("latitude")), num_f64(loc.get("longitude"))) {
                return Some((la, lo));
            }
            if let Some(coords) = loc.get("coordinates").and_then(Value::as_array) {
                if coords.len() >= 2 {
                    if let (Some(lo), Some(la)) = (coords[0].as_f64(), coords[1].as_f64()) {
                        return Some((la, lo));
                    }
                }
            }
        }
    }
    None
}

fn extract_incident_category(row: &Value, ep: &MunicipalCrimeEndpoint) -> String {
    let attr = row.get("attributes").unwrap_or(row);
    for key in [
        ep.category_field.as_deref(),
        Some("incident_category"),
        Some("offense_type"),
        Some("offense"),
        Some("primary_type"),
        Some("crime_type"),
        Some("highest_offense_description"),
        Some("ofns_desc"),
        Some("crm_cd_desc"),
        Some("category"),
        Some("ucr_category"),
        Some("description"),
        Some("type"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(s) = clean_str(attr.get(key)) {
            return title_case(&s);
        }
    }
    "Unspecified incident".to_string()
}

fn extract_incident_year(row: &Value, ep: &MunicipalCrimeEndpoint) -> Option<i32> {
    let attr = row.get("attributes").unwrap_or(row);
    for key in [
        ep.date_field.as_deref(),
        Some("incident_datetime"),
        Some("incident_date"),
        Some("occ_date"),
        Some("offense_start_datetime"),
        Some("date_occ"),
        Some("cmplnt_fr_dt"),
        Some("date"),
        Some("reported_date"),
        Some("year"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(v) = attr.get(key) {
            if let Some(y) = v.as_i64().filter(|&n| (2000..=2100).contains(&n)) {
                return Some(y as i32);
            }
            if let Some(s) = v.as_str() {
                if let Some(prefix) = s.trim().get(..4) {
                    if let Ok(y) = prefix.parse::<i32>() {
                        if (2000..=2100).contains(&y) {
                            return Some(y);
                        }
                    }
                }
                if let Some(yr_tok) = s.split(['/', '-', ' ']).find(|tok| tok.len() == 4 && tok.bytes().all(|b| b.is_ascii_digit())) {
                    if let Ok(y) = yr_tok.parse::<i32>() {
                        if (2000..=2100).contains(&y) {
                            return Some(y);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Summarizes municipal crime incident JSON rows into a strictly factual tract/radius supplement.
/// Never emits `"safe"` or `"unsafe"` labels.
pub fn summarize_municipal_incidents(
    ep: &MunicipalCrimeEndpoint,
    rows: &[Value],
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Value {
    let tract = clean_str(geo.get("tractGeoid")).or_else(|| clean_str(geo.get("tract")));
    let mut total = 0usize;
    let mut violent = 0usize;
    let mut property = 0usize;
    let mut other = 0usize;
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut min_yr: Option<i32> = None;
    let mut max_yr: Option<i32> = None;

    for row in rows {
        let attr = row.get("attributes").unwrap_or(row);
        let in_tract = ep
            .tract_field
            .as_deref()
            .and_then(|tf| clean_str(attr.get(tf)))
            .and_then(|row_tr| {
                tract.as_deref().map(|gt| {
                    let a = row_tr.trim_start_matches('0');
                    let b = gt.trim_start_matches('0');
                    !a.is_empty() && (row_tr == gt || b.ends_with(a) || a.ends_with(b))
                })
            })
            .unwrap_or(false);

        if let Some((r_lat, r_lon)) = extract_incident_coords(row, ep) {
            if !in_tract && haversine_m(lat, lon, r_lat, r_lon) > ep.radius_meters {
                continue;
            }
        }

        let cat = extract_incident_category(row, ep);
        match classify_incident_category(&cat) {
            IncidentBucket::Violent => violent += 1,
            IncidentBucket::Property => property += 1,
            IncidentBucket::Other => other += 1,
        }
        *counts.entry(cat).or_insert(0) += 1;
        total += 1;
        if let Some(y) = extract_incident_year(row, ep) {
            min_yr = Some(min_yr.map(|m| m.min(y)).unwrap_or(y));
            max_yr = Some(max_yr.map(|m| m.max(y)).unwrap_or(y));
        }
    }

    let mut top: Vec<(String, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top_categories: Vec<Value> = top
        .into_iter()
        .take(8)
        .map(|(category, count)| json!({ "category": category, "count": count }))
        .collect();

    let year = ep.year.or(max_yr).unwrap_or_else(|| Utc::now().year());
    let period = match (min_yr, max_yr) {
        (Some(a), Some(b)) if a < b => format!("{a}–{b}"),
        (Some(a), _) => a.to_string(),
        _ => format!("last {} days", ep.lookback_days),
    };
    let radius_mi = round_n(ep.radius_meters / MILE_M, 2);

    json!({
        "source": ep.name,
        "sourceUrl": ep.source_url.as_deref().unwrap_or(ep.endpoint.as_str()),
        "kind": ep.kind,
        "scope": if tract.is_some() { "tract_radius" } else { "radius" },
        "city": ep.city,
        "state": ep.state,
        "tractGeoid": tract,
        "year": year,
        "period": period,
        "radiusMi": radius_mi,
        "totalIncidents": total,
        "violentIncidents": violent,
        "propertyIncidents": property,
        "otherIncidents": other,
        "topCategories": top_categories,
        "note": format!(
            "Reported municipal police incident records within {radius_mi:.2} mi (period: {period}); raw counts depend on local reporting practices and call volume and are not population-normalized rates."
        ),
    })
}

/// Fetches a configured (or well-known) municipal crime incident supplement for `(lat, lon)` / `geo`.
/// Returns `Ok(None)` when no endpoint matches, and `Err(reason)` if a matched endpoint fails.
pub async fn fetch_municipal_crime_supplement(
    http: &HttpClient,
    env: &EnvConfig,
    geo: &Value,
    lat: f64,
    lon: f64,
) -> Result<Option<(Value, String, Vec<String>)>, String> {
    let (configured, mut warnings) = parse_municipal_endpoints(env);
    let has_explicit_cfg = env.get(ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON).is_some();
    let disable_builtin = env.get(ENV_CRIME_MUNICIPAL_DISABLE_BUILTIN) == Some("1");

    let mut builtin_holder = Vec::new();
    let chosen = select_municipal_endpoint(&configured, geo, lat, lon).or_else(|| {
        if has_explicit_cfg || disable_builtin {
            None
        } else {
            builtin_holder = builtin_municipal_crime_endpoints();
            select_municipal_endpoint(&builtin_holder, geo, lat, lon)
        }
    });
    let Some(ep) = chosen else {
        return Ok(None);
    };

    let since = (Utc::now() - ChronoDuration::days(ep.lookback_days))
        .format("%Y-%m-%d")
        .to_string();
    let query_url = build_municipal_crime_url(ep, lat, lon, &since)
        .map_err(|e| format!("Municipal crime endpoint ({}) URL error: {e}", ep.name))?;

    let token = ep
        .app_token
        .as_deref()
        .or_else(|| env.get(ENV_SOCRATA_APP_TOKEN));
    let hdr_pair: [(&str, &str); 1];
    let headers = match token {
        Some(t) if !t.trim().is_empty() => {
            hdr_pair = [("X-App-Token", t.trim())];
            Some(hdr_pair.as_slice())
        }
        _ => None,
    };

    let json_val = http
        .get_json_opts(&query_url, 4500, 0, 3600, headers)
        .await
        .map_err(|e| format!("Municipal crime supplement ({}) unavailable: {e}", ep.name))?;

    let rows: &[Value] = if let Some(arr) = json_val.as_array() {
        arr.as_slice()
    } else if let Some(arr) = json_val.get("data").and_then(Value::as_array) {
        arr.as_slice()
    } else if let Some(arr) = json_val.get("features").and_then(Value::as_array) {
        arr.as_slice()
    } else {
        return Err(format!(
            "Municipal crime supplement ({}) returned unexpected JSON shape",
            ep.name
        ));
    };

    let summary = summarize_municipal_incidents(ep, rows, lat, lon, geo);
    let src_url = ep
        .source_url
        .clone()
        .unwrap_or_else(|| ep.endpoint.clone());
    if !has_explicit_cfg {
        warnings.clear();
    }
    Ok(Some((summary, src_url, warnings)))
}

fn slim_school(s: &Value) -> Value {
    json!({
        "name": s.get("name"),
        "level": s.get("level"),
        "grades": s.get("grades"),
        "district": s.get("district"),
        "inAttendanceZone": s.get("inAttendanceZone").and_then(|v| v.as_bool()).unwrap_or(false),
        "inAssignedDistrict": s.get("inAssignedDistrict").and_then(|v| v.as_bool()).unwrap_or(false),
        "distanceMi": s.get("distanceMi"),
        "distMi": s.get("distanceMi"),
        "direction": s.get("direction"),
        "ncesId": s.get("ncesId"),
        "score": s.get("score"),
        "scoreBadge": s.get("scoreBadge"),
        "scoreSource": s.get("scoreSource"),
        "scoreYear": s.get("scoreYear"),
        "scoreDetail": s.get("scoreDetail"),
        "estGrade": s.get("estGrade"),
        // Always null: third-party ratings are never fetched, stored or scored. The keys stay so that
        // older consumers that look them up keep working.
        "gsRating": Value::Null,
        "nicheGrade": Value::Null,
        "percentile": s.get("percentile"),
        "percentileLevel": s.get("percentileLevel"),
        "metricSource": s.get("metricSource"),
        "metricYear": s.get("metricYear"),
        "metricState": s.get("metricState"),
        "caaspp": s.get("caaspp"),
        "edfacts": s.get("edfacts"),
        "caDashboard": s.get("caDashboard"),
        "studentTeacherRatio": s.get("studentTeacherRatio"),
        "enrollment": s.get("enrollment"),
        "gsUrl": s.get("gsUrl"),
        "nicheUrl": s.get("nicheUrl"),
        "dashboardUrl": s.get("dashboardUrl").or_else(|| s.pointer("/caDashboard/url")),
    })
}

pub async fn run_civic(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<ModuleOutput> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Err(anyhow!("civic: lat / lon must be valid WGS84 coordinates"));
    }
    let api_key = env.get_or("DATA_GOV_API_KEY", "DEMO_KEY");
    let assigned = geo.get("schoolDistricts").filter(|v| !v.is_null());
    let block_group = geo.get("blockGroup").and_then(|v| v.as_str());
    let is_ca = geo
        .get("state")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().eq_ignore_ascii_case("CA"))
        .unwrap_or(false);
    let has_explicit_municipal = env.get(ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON).is_some();
    let mut warnings = Vec::new();
    let mut sources = Vec::new();

    let (r_walk, r_pub, r_priv, r_crime, r_municipal) = tokio::join!(
        fetch_walkability(http, lat, lon, block_group),
        fetch_public_schools(http, lat, lon, assigned),
        fetch_private_schools(http, lat, lon),
        fetch_crime_stats(http, geo, lat, lon, &api_key),
        fetch_municipal_crime_supplement(http, env, geo, lat, lon),
    );

    let walk = match r_walk {
        Ok((w, u)) => {
            sources.push(SourceItem::new("US EPA — National Walkability Index (Smart Location Database v3)", u));
            Some(w)
        }
        Err(e) => {
            warnings.push(format!("Walkability index unavailable: {e}"));
            None
        }
    };

    let (mut public_list, pub_year) = match r_pub {
        Ok((list, yr, u)) => {
            sources.push(SourceItem::new(format!("NCES EDGE — Public school locations & administrative data {yr}"), u));
            (list, Some(yr))
        }
        Err(e) => {
            warnings.push(format!("Public schools unavailable: {e}"));
            (Vec::new(), None)
        }
    };

    let (private_list, priv_year) = match r_priv {
        Ok((list, yr, u)) => {
            sources.push(SourceItem::new(format!("NCES EDGE — Private school locations (PSS) {yr}"), u));
            (list, Some(yr))
        }
        Err(e) => {
            warnings.push(format!("Private schools unavailable: {e}"));
            (Vec::new(), None)
        }
    };

    if !public_list.is_empty() {
        let (enr_src, enr_warn) = enrich_public_schools(http, env, &mut public_list, lat, lon, geo).await;
        sources.extend(enr_src);
        warnings.extend(enr_warn);
    }

    let level_match = |s: &Value, lv: &str| -> bool {
        let sl = s["level"].as_str().unwrap_or("");
        sl == lv || (lv != "high" && sl == "elementary/middle") || sl == "K-12" || (lv != "elementary" && sl == "middle/high")
    };
    let is_choice = |s: &Value| -> bool {
        CHOICE_MAGNET_RE.is_match(&format!("{} {}", s["name"].as_str().unwrap_or(""), s["type"].as_str().unwrap_or("")))
    };
    let pick_assigned = |lv: &str| -> Option<&Value> {
        if let Some(hit) = public_list.iter().find(|s| level_match(s, lv) && s["inAttendanceZone"].as_bool() == Some(true) && !is_choice(s)) {
            return Some(hit);
        }
        if let Some(hit) = public_list.iter().find(|s| level_match(s, lv) && s["inAttendanceZone"].as_bool() == Some(true)) {
            return Some(hit);
        }
        let non_charter = public_list.iter().find(|s| level_match(s, lv) && s["inAssignedDistrict"].as_bool() == Some(true) && s["charter"].as_bool() != Some(true) && !is_choice(s));
        let any_dist = public_list.iter().find(|s| level_match(s, lv) && s["inAssignedDistrict"].as_bool() == Some(true) && !is_choice(s));
        if let (Some(nc), Some(ad)) = (non_charter, any_dist) {
            let d_nc = nc["distanceMi"].as_f64().unwrap_or(99.0);
            let d_ad = ad["distanceMi"].as_f64().unwrap_or(99.0);
            return Some(if d_nc - d_ad > 0.6 { ad } else { nc });
        }
        any_dist.or_else(|| public_list.iter().find(|s| level_match(s, lv) && s["inAssignedDistrict"].as_bool() == Some(true)))
    };

    let levels = ["elementary", "middle", "high"];
    let mut nearest_by_level = Map::new();
    let has_any_assigned = assigned.is_some() || public_list.iter().any(|s| s["inAssignedDistrict"].as_bool() == Some(true));
    let mut nearest_assigned_by_level = Map::new();
    let mut primary_cards = Vec::new();

    for lv in levels {
        let nb = public_list.iter().find(|s| level_match(s, lv) && s["charter"].as_bool() != Some(true)).map(slim_school);
        let na = if has_any_assigned { pick_assigned(lv).map(slim_school) } else { None };
        if let Some(picked) = na.clone().or_else(|| nb.clone()) {
            let mut card = picked.as_object().cloned().unwrap_or_default();
            let role = format!("{}{}", &lv[..1].to_ascii_uppercase(), &lv[1..]);
            card.insert("role".into(), json!(role));
            primary_cards.push(Value::Object(card));
        }
        nearest_by_level.insert(lv.into(), nb.unwrap_or(Value::Null));
        nearest_assigned_by_level.insert(lv.into(), na.unwrap_or(Value::Null));
    }

    // Official scores only (see `official_school_score`); the cards are copies of scored list entries.
    let card_scores: Vec<OfficialSchoolScore> = primary_cards.iter().filter_map(official_school_score).collect();
    let summary_score = if card_scores.is_empty() {
        None
    } else {
        Some(round_n(card_scores.iter().map(|o| o.score).sum::<f64>() / card_scores.len() as f64, 1))
    };
    let join_or_null = |items: Vec<String>| -> Value {
        if items.is_empty() {
            Value::Null
        } else {
            json!(items.join("; "))
        }
    };
    let summary_source = join_or_null(distinct_in_order(card_scores.iter().map(|o| o.source.clone())));
    let summary_year = join_or_null(distinct_in_order(card_scores.iter().map(|o| o.year.clone())));
    let mut counts_by_level = Map::new();
    for lv in ["elementary", "middle", "high", "elementary/middle", "middle/high", "K-12", "preschool", "adult", "other"] {
        let c = public_list.iter().filter(|s| s["level"].as_str() == Some(lv)).count();
        if c > 0 {
            counts_by_level.insert(lv.into(), json!(c));
        }
    }

    let non_ca_missing_academic = !is_ca && summary_score.is_none();
    let academic_data_notice = if non_ca_missing_academic {
        json!(NO_PUBLIC_SCHOOL_DATA_NOTICE)
    } else {
        Value::Null
    };
    if non_ca_missing_academic {
        for card in primary_cards.iter_mut() {
            if card.get("score").map(|v| v.is_null()).unwrap_or(true) {
                if let Some(obj) = card.as_object_mut() {
                    obj.insert("academicDataNotice".into(), json!(NO_PUBLIC_SCHOOL_DATA_NOTICE));
                }
            }
        }
    }

    let schools = json!({
        "radiusMi": SCHOOL_RADIUS_MI as i64,
        "assignedDistricts": assigned,
        "assignedDistrictNote": "Districts come from the Census geocoder; attendance zones from NCES SABS 2015–16 boundaries where available, otherwise the nearest school in the assigned district. Confirm current assignment directly with the district.",
        "attendanceZones": {
            "source": "NCES School Attendance Boundary Survey (SABS)",
            "year": SABS_YEAR,
            "note": "2015–16 boundaries; confirm with the district",
        },
        "scoreMethod": "Official public academic data only: within-state percentile ÷ 10 (CAASPP in CA; EDFacts in other states) where available, otherwise mean of California School Dashboard All-Students tiers (1–5) × 2. Student–teacher ratios are facts, not scores; third-party ratings are never scored.",
        "academicDataNotice": academic_data_notice,
        "summaryScore": summary_score,
        "summaryScore100": summary_score.map(|s| (s * 10.0).round() as i64),
        "summaryGrade": score_to_letter(summary_score),
        "summaryScoreSource": summary_source,
        "summaryScoreYear": summary_year,
        "assignedOrNearest": primary_cards,
        "public": {
            "schoolYear": pub_year,
            "count": public_list.len(),
            "countsByLevel": counts_by_level,
            "nearestByLevel": nearest_by_level,
            "nearestInAssignedDistrictByLevel": if has_any_assigned { Value::Object(nearest_assigned_by_level) } else { Value::Null },
            "list": public_list.iter().take(MAX_PUBLIC).cloned().collect::<Vec<_>>(),
        },
        "private": {
            "schoolYear": priv_year,
            "count": private_list.len(),
            "list": private_list.iter().take(MAX_PRIVATE).cloned().collect::<Vec<_>>(),
        },
    });

    let municipal_supp = match r_municipal {
        Ok(Some((supp, supp_url, supp_warn))) => {
            warnings.extend(supp_warn);
            let src_name = supp
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("Municipal Police Open Data");
            sources.push(SourceItem::new(src_name, supp_url));
            Some(supp)
        }
        Ok(None) => None,
        Err(e) => {
            if has_explicit_municipal {
                warnings.push(e);
            }
            None
        }
    };

    let crime = match r_crime {
        Ok((mut c_data, c_warn, c_urls, c_sources)) => {
            warnings.extend(c_warn);
            sources.push(SourceItem::new("FBI Crime Data Explorer API (cde.ucr.cjis.gov)", c_urls.first().cloned().unwrap_or_else(|| URL_CDE_DIRECT.to_string())));
            sources.extend(c_sources);
            if let Some(supp) = municipal_supp {
                if let Some(obj) = c_data.as_object_mut() {
                    obj.insert("municipalSupplement".into(), supp);
                }
            }
            Some(c_data)
        }
        Err(e) => {
            if let Some(supp) = municipal_supp {
                warnings.push(format!("FBI agency crime statistics unavailable ({e}); showing municipal open-data incident supplement."));
                let src_lbl = supp
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("Municipal Police Open Data")
                    .to_string();
                let st_lbl = supp
                    .get("state")
                    .and_then(Value::as_str)
                    .or_else(|| geo.get("state").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                Some(json!({
                    "source": src_lbl,
                    "fbiAgencyAvailable": false,
                    "fbiUnavailableReason": e.to_string(),
                    "agency": Value::Null,
                    "state": st_lbl,
                    "latest": Value::Null,
                    "change5y": Value::Null,
                    "years": [],
                    "municipalSupplement": supp,
                    "caveats": [
                        "FBI Crime Data Explorer agency data was unavailable for this request; showing local municipal incident records only.",
                        "Raw municipal incident counts reflect reported calls/incidents within the configured radius and are not population-normalized rates.",
                    ],
                }))
            } else {
                warnings.push(format!("Crime statistics unavailable: {e}"));
                None
            }
        }
    };

    let mut highlights = Vec::new();
    if let Some(ref w) = walk {
        if let Some(idx) = w.get("index").and_then(|v| v.as_f64()) {
            let cat = w["category"].as_str().unwrap_or("");
            let rk = w.pointer("/components/intersectionDensity/rank").and_then(|v| v.as_i64()).unwrap_or(0);
            let tr = match w.pointer("/components/transitProximity/metersToNearestStop").and_then(|v| v.as_i64()) {
                Some(m) => format!(", nearest transit stop ~{m} m from the block-group center"),
                None => ", no transit stop data".to_string(),
            };
            highlights.push(json!({
                "level": if idx > 10.5 { "good" } else { "watch" },
                "text": format!("EPA National Walkability Index for this block group: {idx} of 20 ({cat}); intersection-density rank {rk}/20{tr}."),
            }));
        }
    }
    if pub_year.is_some() {
        highlights.push(json!({
            "level": if !public_list.is_empty() { "good" } else { "watch" },
            "text": format!("{} public school{} within 2 mi and {} private school{}.", public_list.len(), if public_list.len() == 1 { "" } else { "s" }, private_list.len(), if private_list.len() == 1 { "" } else { "s" }),
        }));
    }
    if non_ca_missing_academic {
        highlights.push(json!({
            "level": "info",
            "text": format!("{NO_PUBLIC_SCHOOL_DATA_NOTICE}."),
        }));
    }
    if let Some(ref cr) = crime {
        if let Some(l) = cr.get("latest").filter(|v| !v.is_null()) {
            let ag = cr.pointer("/agency/name").and_then(|v| v.as_str()).unwrap_or("Agency");
            let st_lbl = cr.get("state").and_then(|v| v.as_str()).unwrap_or("State");
            let yr = l["year"].as_i64().unwrap_or(0);
            let vc = l.pointer("/violent/count").and_then(|v| v.as_i64()).map(fmt_comma_int).unwrap_or_else(|| "n/a".into());
            let vr = fmt_rate(l.pointer("/violent/ratePer100k").and_then(|v| v.as_f64()));
            let vs = fmt_rate(l.pointer("/violent/stateRatePer100k").and_then(|v| v.as_f64()));
            let vu = fmt_rate(l.pointer("/violent/nationalRatePer100k").and_then(|v| v.as_f64()));
            let pc = l.pointer("/property/count").and_then(|v| v.as_i64()).map(fmt_comma_int).unwrap_or_else(|| "n/a".into());
            let pr = fmt_rate(l.pointer("/property/ratePer100k").and_then(|v| v.as_f64()));
            let ps = fmt_rate(l.pointer("/property/stateRatePer100k").and_then(|v| v.as_f64()));
            let pu = fmt_rate(l.pointer("/property/nationalRatePer100k").and_then(|v| v.as_f64()));
            let below = |sec: &str| -> bool {
                match (
                    l.pointer(&format!("/{sec}/ratePer100k")).and_then(|v| v.as_f64()),
                    l.pointer(&format!("/{sec}/stateRatePer100k")).and_then(|v| v.as_f64()),
                    l.pointer(&format!("/{sec}/nationalRatePer100k")).and_then(|v| v.as_f64()),
                ) {
                    (Some(r), Some(s), Some(u)) => r < s && r < u,
                    _ => false,
                }
            };
            highlights.push(json!({
                "level": if below("violent") && below("property") { "good" } else { "watch" },
                "text": format!("{ag} (agency-wide, {yr}): {vc} violent crimes ({vr} per 100k residents; {st_lbl} {vs}, US {vu}) and {pc} property crimes ({pr} per 100k; {st_lbl} {ps}, US {pu}). Jurisdiction-wide figures, not neighborhood-specific."),
            }));
        }
        if let Some(supp) = cr.get("municipalSupplement").filter(|v| v.is_object()) {
            let src = supp.get("source").and_then(Value::as_str).unwrap_or("Municipal open data");
            let period = supp.get("period").and_then(Value::as_str).unwrap_or("recent");
            let total = supp.get("totalIncidents").and_then(Value::as_i64).unwrap_or(0);
            let violent = supp.get("violentIncidents").and_then(Value::as_i64).unwrap_or(0);
            let property = supp.get("propertyIncidents").and_then(Value::as_i64).unwrap_or(0);
            let other = supp.get("otherIncidents").and_then(Value::as_i64).unwrap_or(0);
            let rad = supp.get("radiusMi").and_then(Value::as_f64).unwrap_or(1.0);
            highlights.push(json!({
                "level": "info",
                "text": format!(
                    "{src} ({period}): {total} reported incidents within {rad:.2} mi ({violent} violent-category, {property} property-category, {other} other). Local reported counts, not population-normalized rates."
                ),
            }));
        }
    }

    let walk_score = walk
        .as_ref()
        .and_then(|w| w.get("index").and_then(|v| v.as_f64()))
        .map(|idx| (((idx - 1.0) / 19.0) * 100.0).round() as i64);

    let mut map_layers = Vec::new();
    if !public_list.is_empty() {
        let feats: Vec<Value> = public_list
            .iter()
            .take(MAX_PUBLIC)
            .filter_map(|s| {
                let la = s["lat"].as_f64()?;
                let lo = s["lon"].as_f64()?;
                let mut props = Map::new();
                props.insert("name".into(), s["name"].clone());
                props.insert("popup".into(), json!(format!("{} — {} · {}", s["name"].as_str().unwrap_or(""), s["level"].as_str().unwrap_or(""), fmt_mi(s["distanceMi"].as_f64().unwrap_or(0.0)))));
                props.insert("level".into(), s["level"].clone());
                props.insert("district".into(), s["district"].clone());
                props.insert("inAssignedDistrict".into(), s["inAssignedDistrict"].clone());
                props.insert("url".into(), s["url"].clone());
                serde_json::to_value(Feature {
                    bbox: None,
                    geometry: Some(Geometry::from(&Point::new(lo, la))),
                    id: None,
                    properties: Some(props),
                    foreign_members: None,
                })
                .ok()
            })
            .collect();
        map_layers.push(json!({ "id": "civic-public-schools", "title": "Public schools (≤ 2 mi)", "kind": "points", "color": "#1f77b4", "features": feats }));
    }
    if !private_list.is_empty() {
        let feats: Vec<Value> = private_list
            .iter()
            .take(MAX_PRIVATE)
            .filter_map(|s| {
                let la = s["lat"].as_f64()?;
                let lo = s["lon"].as_f64()?;
                let mut props = Map::new();
                props.insert("name".into(), s["name"].clone());
                props.insert("popup".into(), json!(format!("{} (private) · {}", s["name"].as_str().unwrap_or(""), fmt_mi(s["distanceMi"].as_f64().unwrap_or(0.0)))));
                props.insert("url".into(), s["url"].clone());
                serde_json::to_value(Feature {
                    bbox: None,
                    geometry: Some(Geometry::from(&Point::new(lo, la))),
                    id: None,
                    properties: Some(props),
                    foreign_members: None,
                })
                .ok()
            })
            .collect();
        map_layers.push(json!({ "id": "civic-private-schools", "title": "Private schools (≤ 2 mi)", "kind": "points", "color": "#17becf", "features": feats }));
    }

    let data = json!({
        "walkability": walk,
        "schools": schools,
        "crime": crime,
        "scores": { "walkability": walk_score },
        "highlights": highlights,
        "mapLayers": map_layers.clone(),
        "layers": map_layers,
    });

    Ok(ModuleOutput {
        data,
        sources,
        warnings,
    })
}
