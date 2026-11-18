//! NOAA / NASA Rainfall Climatology, Atlas 14 Design Storms & Live QPE/QPF adapter.
//!
//! Combines keyless federal open-data hydrometeorology feeds for a US parcel:
//! - NASA POWER (`community=AG`) 25-year monthly & annual rainfall climatology (`PRECTOTCORR`)
//! - NOAA HDSC PFDS Atlas 14 design storm depths (`fe_text_mean.csv`), with graceful fallback
//!   for WA/OR/ID/MT/WY where Atlas 14 returns `ErrorMsg`
//! - NOAA RCC-ACIS (`GridData` 4km PRISM + `StnMeta`/`StnData` GHCN) 30-year normals, annual
//!   wet/heavy-rain day counts, and dated historical 1-day & 3-day storm maxima
//! - NOAA RFC multi-sensor observed QPE (`obs/rfc_qpe/MapServer/identify` 24h/72h/7d/30d/365d/WYTD),
//!   treating negative sentinel values (`-10000`) and `"NoData"` as `0.00` inches
//! - NOAA WPC 7-day quantitative precipitation forecast (`wpc_qpf`) and RFC Flash Flood Guidance
//!
//! Never fabricates rainfall figures when offline or when upstream services fail.

use anyhow::Result;
use chrono::Datelike;
use futures::future::BoxFuture;
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use url::Url;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind,
    ExecutionMode, ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto,
    ExtensionTier, GeoScopeProto, LicenseClass, ManifestSourceAttributionProto,
    MeteringSpecProto, ProvenanceTier, RateLimitSpecProto, ShareScope, SharingSpecProto,
    SourceAttributionProto,
};
use crate::shared::{haversine_m, now_iso, round_n, EnvConfig, HttpClient};

pub const RAINFALL_EXT_ID: &str = "rainfall_climatology";
pub const RAINFALL_SOURCE_NAME: &str =
    "NOAA Atlas 14 PFDS / RCC-ACIS / RFC QPE / NASA POWER";
pub const RAINFALL_HOMEPAGE_URL: &str = "https://hdsc.nws.noaa.gov/pfds/";
pub const RAINFALL_DOCS_URL: &str =
    "https://power.larc.nasa.gov/docs/services/api/temporal/climatology/";
pub const RAINFALL_CACHE_TTL_SECS: u64 = 3600;

pub const URL_NASA_POWER_CLIMATOLOGY: &str =
    "https://power.larc.nasa.gov/api/temporal/climatology/point";
pub const URL_NOAA_ATLAS14_CSV: &str =
    "https://hdsc.nws.noaa.gov/cgi-bin/hdsc/new/fe_text_mean.csv";
pub const URL_RCC_ACIS_BASE: &str = "https://data.rcc-acis.org";
pub const URL_NOAA_RFC_QPE_IDENTIFY: &str =
    "https://mapservices.weather.noaa.gov/raster/rest/services/obs/rfc_qpe/MapServer/identify";
pub const URL_NOAA_WPC_QPF_LAYER11: &str =
    "https://mapservices.weather.noaa.gov/vector/rest/services/precip/wpc_qpf/MapServer/11";
pub const URL_NOAA_RFC_FFG_IDENTIFY: &str =
    "https://mapservices.weather.noaa.gov/raster/rest/services/precip/rfc_gridded_ffg/MapServer/identify";

const REQ_TIMEOUT_MS: u64 = 8_500;
const TTL_30D_SECS: i64 = 30 * 86_400;
const TTL_1H_SECS: i64 = 3_600;

const MONTH_KEYS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DAYS_IN_MONTH: [f64; 12] = [
    31.0, 28.25, 31.0, 30.0, 31.0, 30.0, 31.0, 31.0, 30.0, 31.0, 30.0, 31.0,
];

/// Process-wide 30-day cache for static RCC-ACIS POST responses (PRISM 10-yr summary + station normals).
static ACIS_STATIC_CACHE: Lazy<Cache<String, Value>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(1_024)
        .time_to_live(Duration::from_secs(30 * 86_400))
        .build()
});

#[derive(Debug, Clone)]
pub struct RainfallEndpoints {
    pub nasa_power_url: String,
    pub atlas14_csv_url: String,
    pub acis_base_url: String,
    pub rfc_qpe_identify_url: String,
    pub wpc_qpf_layer_url: String,
    pub rfc_ffg_identify_url: String,
    pub use_static_cache: bool,
}

impl Default for RainfallEndpoints {
    fn default() -> Self {
        Self {
            nasa_power_url: URL_NASA_POWER_CLIMATOLOGY.to_string(),
            atlas14_csv_url: URL_NOAA_ATLAS14_CSV.to_string(),
            acis_base_url: URL_RCC_ACIS_BASE.to_string(),
            rfc_qpe_identify_url: URL_NOAA_RFC_QPE_IDENTIFY.to_string(),
            wpc_qpf_layer_url: URL_NOAA_WPC_QPF_LAYER11.to_string(),
            rfc_ffg_identify_url: URL_NOAA_RFC_FFG_IDENTIFY.to_string(),
            use_static_cache: true,
        }
    }
}

fn is_us_country_opt(country: &str) -> bool {
    let trimmed = country.trim();
    if trimmed.is_empty() {
        return true;
    }
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "US" | "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA"
    )
}

/// Builds the shared NASA POWER `community=AG` climatology URL used by both `rainfall_climatology`
/// and `usda_soil_moisture` so concurrent calls coalesce in `HttpClient`'s singleflight L1 cache.
pub fn build_nasa_power_ag_url(base_url: &str, lat: f64, lon: f64) -> Result<String> {
    let u = Url::parse_with_params(
        base_url,
        &[
            (
                "parameters",
                "PRECTOTCORR,GWETTOP,GWETROOT,GWETPROF,RH2M,T2MDEW",
            ),
            ("community", "AG"),
            ("longitude", &format!("{lon:.4}")),
            ("latitude", &format!("{lat:.4}")),
            ("format", "JSON"),
        ],
    )?;
    Ok(u.into())
}

#[derive(Debug, Clone)]
pub struct NasaRainfallNormals {
    pub monthly_in: [f64; 12],
    pub annual_in: f64,
}

pub fn parse_nasa_power_rainfall(json_val: &Value) -> Option<NasaRainfallNormals> {
    let p = json_val.pointer("/properties/parameter/PRECTOTCORR")?;
    let mut monthly_in = [0.0_f64; 12];
    let mut valid_months = 0usize;
    for (idx, mk) in MONTH_KEYS.iter().enumerate() {
        if let Some(mm_day) = p.get(*mk).and_then(Value::as_f64) {
            if mm_day.is_finite() && mm_day >= 0.0 {
                monthly_in[idx] = round_n(mm_day * DAYS_IN_MONTH[idx] / 25.4, 2);
                valid_months += 1;
            }
        }
    }
    if valid_months < 12 {
        return None;
    }
    let sum_monthly: f64 = monthly_in.iter().sum();
    let annual_in = p
        .get("ANN")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|mm_day| round_n(mm_day * 365.25 / 25.4, 2))
        .unwrap_or_else(|| round_n(sum_monthly, 2));
    Some(NasaRainfallNormals {
        monthly_in,
        annual_in,
    })
}

/// Parses NOAA HDSC PFDS `fe_text_mean.csv` output, handling non-covered Pacific Northwest states
/// (WA/OR/ID/MT/WY -> `ErrorMsg = 'Error 3.0: Selected location is not within a project area'`)
/// cleanly without erroring.
pub fn parse_atlas14_csv(raw_csv: &str) -> Value {
    let trimmed = raw_csv.trim();
    if trimmed.is_empty() {
        return json!({
            "available": false,
            "errorMsg": "Empty NOAA Atlas 14 response",
        });
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("errormsg") || lower.contains("not within a project area") {
        let err_msg = trimmed
            .lines()
            .find(|l| l.to_ascii_lowercase().contains("errormsg"))
            .and_then(|l| l.split_once('='))
            .map(|(_, rhs)| {
                rhs.trim()
                    .trim_end_matches(';')
                    .trim()
                    .trim_matches('\'')
                    .trim_matches('"')
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Selected location is not within a published NOAA Atlas 14 project area".to_string());
        return json!({
            "available": false,
            "volume": null,
            "errorMsg": err_msg,
            "hr1Yr2In": null,
            "hr1Yr10In": null,
            "hr1Yr100In": null,
            "hr6Yr10In": null,
            "hr6Yr100In": null,
            "hr24Yr2In": null,
            "hr24Yr10In": null,
            "hr24Yr25In": null,
            "hr24Yr50In": null,
            "hr24Yr100In": null,
            "hr24Yr500In": null,
            "day3Yr10In": null,
            "day3Yr100In": null,
        });
    }

    let volume = trimmed
        .lines()
        .find(|l| l.trim_start().starts_with("NOAA Atlas"))
        .map(|l| l.trim().to_string());

    let mut ari_years: Vec<u32> = Vec::new();
    let mut in_table = false;

    let mut hr1_yr2: Option<f64> = None;
    let mut hr1_yr10: Option<f64> = None;
    let mut hr1_yr100: Option<f64> = None;
    let mut hr6_yr10: Option<f64> = None;
    let mut hr6_yr100: Option<f64> = None;
    let mut hr24_yr2: Option<f64> = None;
    let mut hr24_yr10: Option<f64> = None;
    let mut hr24_yr25: Option<f64> = None;
    let mut hr24_yr50: Option<f64> = None;
    let mut hr24_yr100: Option<f64> = None;
    let mut hr24_yr500: Option<f64> = None;
    let mut day3_yr10: Option<f64> = None;
    let mut day3_yr100: Option<f64> = None;

    for line in trimmed.lines() {
        let l = line.trim();
        if l.is_empty() {
            if in_table && !ari_years.is_empty() {
                break;
            }
            continue;
        }
        let l_low = l.to_ascii_lowercase();
        if l_low.starts_with("by duration for ari") || l_low.starts_with("by duration for aep") {
            ari_years = l
                .split(',')
                .skip(1)
                .filter_map(|tok| tok.trim().parse::<u32>().ok())
                .collect();
            in_table = !ari_years.is_empty();
            continue;
        }
        if !in_table {
            continue;
        }
        let mut cols = l.split(',');
        let Some(dur_raw) = cols.next() else {
            continue;
        };
        let dur = dur_raw.trim().trim_end_matches(':').to_ascii_lowercase();
        let vals: Vec<Option<f64>> = cols
            .map(|c| {
                c.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .map(|v| round_n(v, 2))
            })
            .collect();

        let pick_ari = |target_yr: u32| -> Option<f64> {
            let idx = ari_years.iter().position(|&y| y == target_yr)?;
            vals.get(idx).copied().flatten()
        };

        match dur.as_str() {
            "60-min" | "1-hr" => {
                hr1_yr2 = pick_ari(2);
                hr1_yr10 = pick_ari(10);
                hr1_yr100 = pick_ari(100);
            }
            "6-hr" => {
                hr6_yr10 = pick_ari(10);
                hr6_yr100 = pick_ari(100);
            }
            "24-hr" | "1-day" => {
                hr24_yr2 = pick_ari(2).or(hr24_yr2);
                hr24_yr10 = pick_ari(10).or(hr24_yr10);
                hr24_yr25 = pick_ari(25).or(hr24_yr25);
                hr24_yr50 = pick_ari(50).or(hr24_yr50);
                hr24_yr100 = pick_ari(100).or(hr24_yr100);
                hr24_yr500 = pick_ari(500).or(hr24_yr500);
            }
            "3-day" => {
                day3_yr10 = pick_ari(10);
                day3_yr100 = pick_ari(100);
            }
            _ => {}
        }
    }

    let available = hr24_yr10.is_some() || hr24_yr100.is_some() || hr1_yr10.is_some();
    json!({
        "available": available,
        "volume": volume,
        "errorMsg": if available { Value::Null } else { json!("Atlas 14 table unavailable for location") },
        "hr1Yr2In": hr1_yr2,
        "hr1Yr10In": hr1_yr10,
        "hr1Yr100In": hr1_yr100,
        "hr6Yr10In": hr6_yr10,
        "hr6Yr100In": hr6_yr100,
        "hr24Yr2In": hr24_yr2,
        "hr24Yr10In": hr24_yr10,
        "hr24Yr25In": hr24_yr25,
        "hr24Yr50In": hr24_yr50,
        "hr24Yr100In": hr24_yr100,
        "hr24Yr500In": hr24_yr500,
        "day3Yr10In": day3_yr10,
        "day3Yr100In": day3_yr100,
    })
}

fn parse_pixel_value_raw(attrs: &Value) -> Option<String> {
    for key in [
        "Service Pixel Value",
        "Classify.Pixel Value",
        "Pixel Value",
        "Raster.Service Pixel Value",
    ] {
        if let Some(v) = attrs.get(key) {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            } else if let Some(n) = v.as_f64() {
                if n.is_finite() {
                    return Some(n.to_string());
                }
            }
        }
    }
    None
}

/// Parses NOAA RFC multi-sensor QPE `MapServer/identify` JSON (`layers=all:28,40,56,68,88,100`).
/// Per NOAA RFC convention, negative pixel sentinels (`-10000`) and `"NoData"` represent `0.00"` rain.
pub fn parse_rfc_qpe_identify(json_val: &Value) -> Option<Value> {
    let results = json_val.get("results")?.as_array()?;
    if results.is_empty() {
        return None;
    }

    let mut h24: Option<f64> = None;
    let mut h72: Option<f64> = None;
    let mut d7: Option<f64> = None;
    let mut d30: Option<f64> = None;
    let mut d365: Option<f64> = None;
    let mut wytd: Option<f64> = None;

    for item in results {
        let layer_id = item.get("layerId").and_then(Value::as_i64).unwrap_or(-1);
        let Some(attrs) = item.get("attributes") else {
            continue;
        };
        let name_lc = attrs
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let val_in = match parse_pixel_value_raw(attrs) {
            Some(raw) => {
                let r_lc = raw.to_ascii_lowercase();
                if r_lc == "nodata" || r_lc == "null" || r_lc == "nan" {
                    0.0
                } else if let Ok(v) = raw.parse::<f64>() {
                    if !v.is_finite() || v < 0.0 {
                        0.0
                    } else {
                        round_n(v, 2)
                    }
                } else {
                    0.0
                }
            }
            None => continue,
        };

        if layer_id == 28 || name_lc.contains("last24hours") {
            h24 = Some(val_in);
        } else if layer_id == 40 || name_lc.contains("last3days") || name_lc.contains("last72") {
            h72 = Some(val_in);
        } else if layer_id == 56 || name_lc.contains("last7days") {
            d7 = Some(val_in);
        } else if layer_id == 68 || name_lc.contains("last30days") {
            d30 = Some(val_in);
        } else if layer_id == 88 || name_lc.contains("last365days") {
            d365 = Some(val_in);
        } else if layer_id == 100 || name_lc.contains("wytd") {
            wytd = Some(val_in);
        }
    }

    if h24.is_none()
        && h72.is_none()
        && d7.is_none()
        && d30.is_none()
        && d365.is_none()
        && wytd.is_none()
    {
        return None;
    }

    Some(json!({
        "h24In": h24,
        "h72In": h72,
        "d7In": d7,
        "d30In": d30,
        "d365In": d365,
        "wytdIn": wytd,
    }))
}

/// Parses NOAA RFC Gridded Flash Flood Guidance (`rfc_gridded_ffg/MapServer/identify`, `layers=all:3,7`).
/// Note: `5kmffg_01` and `5kmffg_03` raster pixel values are in **millimeters** (`mm / 25.4 -> inches`).
pub fn parse_rfc_ffg_identify(json_val: &Value) -> Option<Value> {
    let results = json_val.get("results")?.as_array()?;
    let mut hr1_in: Option<f64> = None;
    let mut hr3_in: Option<f64> = None;

    for item in results {
        let layer_id = item.get("layerId").and_then(Value::as_i64).unwrap_or(-1);
        let Some(attrs) = item.get("attributes") else {
            continue;
        };
        let name_lc = attrs
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let Some(raw) = parse_pixel_value_raw(attrs) else {
            continue;
        };
        let Ok(val_mm) = raw.parse::<f64>() else {
            continue;
        };
        if !val_mm.is_finite() || val_mm <= 0.0 || val_mm > 1_500.0 {
            continue;
        }
        let inches = round_n(val_mm / 25.4, 2);
        if layer_id == 3 || name_lc.contains("ffg_01") {
            hr1_in = Some(inches);
        } else if layer_id == 7 || name_lc.contains("ffg_03") {
            hr3_in = Some(inches);
        }
    }

    if hr1_in.is_none() && hr3_in.is_none() {
        return None;
    }
    Some(json!({
        "hr1In": hr1_in,
        "hr3In": hr3_in,
    }))
}

pub fn parse_wpc_qpf_7d(json_val: &Value) -> Option<(f64, Option<String>)> {
    let feats = json_val.get("features")?.as_array()?;
    if feats.is_empty() {
        return Some((0.0, None));
    }
    let mut best_qpf: Option<f64> = None;
    let mut valid_time: Option<String> = None;
    for f in feats {
        let Some(attrs) = f.get("attributes") else {
            continue;
        };
        if let Some(q) = attrs
            .get("qpf")
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse::<f64>().ok()))
        {
            if q.is_finite() && q >= 0.0 {
                let q_r = round_n(q, 2);
                if best_qpf.map(|b| q_r > b).unwrap_or(true) {
                    best_qpf = Some(q_r);
                    valid_time = attrs
                        .get("valid_time")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                }
            }
        }
    }
    best_qpf.map(|q| (q, valid_time))
}

fn acis_num(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        if n.is_finite() && n >= 0.0 {
            return Some(n);
        }
        return None;
    }
    let s = v.as_str()?.trim();
    if s.eq_ignore_ascii_case("T") {
        return Some(0.0);
    }
    if s.is_empty() || s.eq_ignore_ascii_case("M") || s.eq_ignore_ascii_case("S") {
        return None;
    }
    s.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0)
}

#[derive(Debug, Default, Clone)]
struct AcisSummary {
    prism_annual_mean_in: Option<f64>,
    wet_days_per_yr: Option<f64>,
    mod_rain_days_ge_05_in: Option<f64>,
    heavy_rain_days_ge_1_in: Option<f64>,
    prism_max_1day_in: Option<f64>,
    station_id: Option<String>,
    station_name: Option<String>,
    station_monthly_normals_in: Option<[f64; 12]>,
    station_annual_normal_in: Option<f64>,
    max_1day_in: Option<f64>,
    max_1day_date: Option<String>,
    max_3day_in: Option<f64>,
    max_3day_date: Option<String>,
}

fn pick_best_acis_station(meta_json: &Value, lat: f64, lon: f64) -> Option<(String, String)> {
    let arr = meta_json.get("meta")?.as_array()?;
    let mut best: Option<(f64, String, String)> = None;

    for item in arr {
        let ll = item.get("ll").and_then(Value::as_array);
        let (Some(slon), Some(slat)) = (
            ll.and_then(|a| a.first()).and_then(Value::as_f64),
            ll.and_then(|a| a.get(1)).and_then(Value::as_f64),
        ) else {
            continue;
        };
        let dist_km = haversine_m(lat, lon, slat, slon) / 1000.0;
        let (start_yr, end_yr) = item
            .get("valid_daterange")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_array)
            .map(|r| {
                let s = r
                    .first()
                    .and_then(Value::as_str)
                    .and_then(|d| d.get(0..4))
                    .and_then(|y| y.parse::<i32>().ok())
                    .unwrap_or(9999);
                let e = r
                    .get(1)
                    .and_then(Value::as_str)
                    .and_then(|d| d.get(0..4))
                    .and_then(|y| y.parse::<i32>().ok())
                    .unwrap_or(0);
                (s, e)
            })
            .unwrap_or((9999, 0));

        if end_yr < 2018 {
            continue;
        }
        let sids = item.get("sids").and_then(Value::as_array);
        let mut chosen_sid: Option<String> = None;
        let mut is_ghcn_primary = false;
        if let Some(list) = sids {
            for raw_sid in list.iter().filter_map(Value::as_str) {
                let tok = raw_sid.split_whitespace().next().unwrap_or("").trim();
                if tok.starts_with("USW") || tok.starts_with("USC") {
                    chosen_sid = Some(tok.to_string());
                    is_ghcn_primary = true;
                    break;
                } else if chosen_sid.is_none() && !tok.is_empty() {
                    chosen_sid = Some(tok.to_string());
                }
            }
        }
        let Some(sid) = chosen_sid else {
            continue;
        };
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or(&sid)
            .to_string();

        // Lower score wins: strongly prefer long-record GHCN stations (1991-2020 normals + 30-yr max) near the home.
        let mut rank_cost = dist_km;
        if !is_ghcn_primary {
            rank_cost += 18.0;
        }
        if start_yr > 1995 {
            rank_cost += 20.0;
        }
        if end_yr < 2023 {
            rank_cost += 10.0;
        }

        if best.as_ref().map(|b| rank_cost < b.0).unwrap_or(true) {
            best = Some((rank_cost, sid, name));
        }
    }

    best.map(|(_, sid, name)| (sid, name))
}

async fn fetch_acis_summary(
    http: &HttpClient,
    base_url: &str,
    lat: f64,
    lon: f64,
    use_cache: bool,
) -> AcisSummary {
    let cache_key = format!("acis:{base_url}:{lat:.3}:{lon:.3}");
    if use_cache && !http.is_bypassing_cache_reads() {
        if let Some(cached) = ACIS_STATIC_CACHE.get(&cache_key).await {
            if let Ok(parsed) = serde_json::from_value::<Value>(cached) {
                return acis_summary_from_cached_json(&parsed);
            }
        }
    }

    let clean_base = base_url.trim_end_matches('/');
    let cur_yr = chrono::Utc::now().year();
    let end_complete_yr = (cur_yr - 1).max(2024);
    let start_10yr = end_complete_yr - 9;

    let grid_url = format!("{clean_base}/GridData");
    let grid_body = json!({
        "loc": format!("{lon:.6},{lat:.6}"),
        "grid": "21",
        "sdate": format!("{start_10yr}-01-01"),
        "edate": format!("{end_complete_yr}-12-31"),
        "elems": [
            { "name": "pcpn", "interval": "yly", "duration": "yly", "reduce": "sum" },
            { "name": "pcpn", "interval": "yly", "duration": 1, "reduce": "max" },
            { "name": "pcpn", "interval": "yly", "duration": "yly", "reduce": "cnt_ge_0.01" },
            { "name": "pcpn", "interval": "yly", "duration": "yly", "reduce": "cnt_ge_0.50" },
            { "name": "pcpn", "interval": "yly", "duration": "yly", "reduce": "cnt_ge_1.00" }
        ]
    });

    let meta_url = format!("{clean_base}/StnMeta");
    let meta_body = json!({
        "bbox": format!("{:.4},{:.4},{:.4},{:.4}", lon - 0.25, lat - 0.25, lon + 0.25, lat + 0.25),
        "elems": "pcpn",
        "meta": "uid,name,sids,ll,valid_daterange"
    });

    let grid_fut = async {
        http.scrape_post(&grid_url)
            .json_value_body(&grid_body)
            .timeout_ms(REQ_TIMEOUT_MS)
            .retries(1)
            .json::<Value>()
            .await
            .ok()
    };
    let stn_fut = async {
        let meta_json = http
            .scrape_post(&meta_url)
            .json_value_body(&meta_body)
            .timeout_ms(REQ_TIMEOUT_MS)
            .retries(1)
            .json::<Value>()
            .await
            .ok()?;
        let (sid, stn_name) = pick_best_acis_station(&meta_json, lat, lon)?;
        let stn_url = format!("{clean_base}/StnData");
        let norm_body = json!({
            "sid": sid,
            "sdate": "2024-01",
            "edate": "2024-12",
            "elems": [
                { "name": "pcpn", "interval": "mly", "duration": "mly", "reduce": "sum", "normal": "1" }
            ]
        });
        let max_body = json!({
            "sid": sid,
            "sdate": "1991-01-01",
            "edate": format!("{cur_yr}-12-31"),
            "elems": [
                { "name": "pcpn", "interval": "dly", "duration": 1, "reduce": "sum", "smry": { "reduce": "max", "add": "date" }, "smry_only": 1 },
                { "name": "pcpn", "interval": "dly", "duration": 3, "reduce": "sum", "smry": { "reduce": "max", "add": "date" }, "smry_only": 1 }
            ]
        });
        let (norm_opt, max_opt) = tokio::join!(
            Box::pin(async {
                http.scrape_post(&stn_url)
                    .json_value_body(&norm_body)
                    .timeout_ms(REQ_TIMEOUT_MS)
                    .retries(1)
                    .json::<Value>()
                    .await
                    .ok()
            }),
            Box::pin(async {
                http.scrape_post(&stn_url)
                    .json_value_body(&max_body)
                    .timeout_ms(REQ_TIMEOUT_MS)
                    .retries(1)
                    .json::<Value>()
                    .await
                    .ok()
            })
        );
        Some((sid, stn_name, norm_opt, max_opt))
    };

    let (grid_opt, stn_bundle) = tokio::join!(Box::pin(grid_fut), Box::pin(stn_fut));
    let mut out = AcisSummary::default();

    if let Some(ref gj) = grid_opt {
        if let Some(rows) = gj.get("data").and_then(Value::as_array) {
            let mut sums = Vec::new();
            let mut wet = Vec::new();
            let mut mod_d = Vec::new();
            let mut hvy = Vec::new();
            let mut max1d: Option<f64> = None;

            for r in rows {
                let Some(cols) = r.as_array() else {
                    continue;
                };
                if let Some(v) = cols.get(1).and_then(acis_num) {
                    sums.push(v);
                }
                if let Some(v) = cols.get(2).and_then(acis_num) {
                    max1d = Some(max1d.map(|m| m.max(v)).unwrap_or(v));
                }
                if let Some(v) = cols.get(3).and_then(acis_num) {
                    wet.push(v);
                }
                if let Some(v) = cols.get(4).and_then(acis_num) {
                    mod_d.push(v);
                }
                if let Some(v) = cols.get(5).and_then(acis_num) {
                    hvy.push(v);
                }
            }
            if !sums.is_empty() {
                out.prism_annual_mean_in =
                    Some(round_n(sums.iter().sum::<f64>() / sums.len() as f64, 2));
            }
            if !wet.is_empty() {
                out.wet_days_per_yr = Some(round_n(wet.iter().sum::<f64>() / wet.len() as f64, 1));
            }
            if !mod_d.is_empty() {
                out.mod_rain_days_ge_05_in =
                    Some(round_n(mod_d.iter().sum::<f64>() / mod_d.len() as f64, 1));
            }
            if !hvy.is_empty() {
                out.heavy_rain_days_ge_1_in =
                    Some(round_n(hvy.iter().sum::<f64>() / hvy.len() as f64, 1));
            }
            out.prism_max_1day_in = max1d.map(|v| round_n(v, 2));
        }
    }

    if let Some((sid, stn_name, norm_opt, max_opt)) = stn_bundle {
        out.station_id = Some(sid);
        out.station_name = Some(stn_name);

        if let Some(ref nj) = norm_opt {
            if let Some(rows) = nj.get("data").and_then(Value::as_array) {
                if rows.len() >= 12 {
                    let mut m_arr = [0.0_f64; 12];
                    let mut ok_cnt = 0usize;
                    for (idx, row) in rows.iter().take(12).enumerate() {
                        if let Some(v) = row
                            .as_array()
                            .and_then(|a| a.get(1))
                            .and_then(acis_num)
                        {
                            m_arr[idx] = round_n(v, 2);
                            ok_cnt += 1;
                        }
                    }
                    if ok_cnt == 12 {
                        let ann = round_n(m_arr.iter().sum::<f64>(), 2);
                        out.station_monthly_normals_in = Some(m_arr);
                        out.station_annual_normal_in = Some(ann);
                    }
                }
            }
        }

        if let Some(ref mj) = max_opt {
            if let Some(smry) = mj.get("smry").and_then(Value::as_array) {
                if let Some(s1) = smry.first().and_then(Value::as_array) {
                    out.max_1day_in = s1.first().and_then(acis_num).map(|v| round_n(v, 2));
                    out.max_1day_date = s1
                        .get(1)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("M"))
                        .map(String::from);
                }
                if let Some(s3) = smry.get(1).and_then(Value::as_array) {
                    out.max_3day_in = s3.first().and_then(acis_num).map(|v| round_n(v, 2));
                    out.max_3day_date = s3
                        .get(1)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("M"))
                        .map(String::from);
                }
            }
        }
    }

    if out.max_1day_in.is_none() {
        out.max_1day_in = out.prism_max_1day_in;
    }

    if use_cache
        && (out.prism_annual_mean_in.is_some()
            || out.station_annual_normal_in.is_some()
            || out.max_1day_in.is_some())
    {
        ACIS_STATIC_CACHE
            .insert(cache_key, acis_summary_to_cached_json(&out))
            .await;
    }

    out
}

fn acis_summary_to_cached_json(s: &AcisSummary) -> Value {
    json!({
        "prismAnnualMeanIn": s.prism_annual_mean_in,
        "wetDaysPerYr": s.wet_days_per_yr,
        "modRainDaysGe05In": s.mod_rain_days_ge_05_in,
        "heavyRainDaysGe1In": s.heavy_rain_days_ge_1_in,
        "prismMax1DayIn": s.prism_max_1day_in,
        "stationId": s.station_id,
        "stationName": s.station_name,
        "stationMonthlyNormalsIn": s.station_monthly_normals_in,
        "stationAnnualNormalIn": s.station_annual_normal_in,
        "max1DayIn": s.max_1day_in,
        "max1DayDate": s.max_1day_date,
        "max3DayIn": s.max_3day_in,
        "max3DayDate": s.max_3day_date,
    })
}

fn acis_summary_from_cached_json(v: &Value) -> AcisSummary {
    let station_monthly_normals_in = v
        .get("stationMonthlyNormalsIn")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 12)
        .and_then(|a| {
            let mut out = [0.0_f64; 12];
            for (i, item) in a.iter().enumerate() {
                out[i] = item.as_f64()?;
            }
            Some(out)
        });
    AcisSummary {
        prism_annual_mean_in: v.get("prismAnnualMeanIn").and_then(Value::as_f64),
        wet_days_per_yr: v.get("wetDaysPerYr").and_then(Value::as_f64),
        mod_rain_days_ge_05_in: v.get("modRainDaysGe05In").and_then(Value::as_f64),
        heavy_rain_days_ge_1_in: v.get("heavyRainDaysGe1In").and_then(Value::as_f64),
        prism_max_1day_in: v.get("prismMax1DayIn").and_then(Value::as_f64),
        station_id: v
            .get("stationId")
            .and_then(Value::as_str)
            .map(String::from),
        station_name: v
            .get("stationName")
            .and_then(Value::as_str)
            .map(String::from),
        station_monthly_normals_in,
        station_annual_normal_in: v.get("stationAnnualNormalIn").and_then(Value::as_f64),
        max_1day_in: v.get("max1DayIn").and_then(Value::as_f64),
        max_1day_date: v
            .get("max1DayDate")
            .and_then(Value::as_str)
            .map(String::from),
        max_3day_in: v.get("max3DayIn").and_then(Value::as_f64),
        max_3day_date: v
            .get("max3DayDate")
            .and_then(Value::as_str)
            .map(String::from),
    }
}

pub async fn fetch_rainfall_climatology(
    http: &HttpClient,
    lat: f64,
    lon: f64,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    fetch_rainfall_climatology_with_endpoints(http, &RainfallEndpoints::default(), lat, lon).await
}

pub async fn fetch_rainfall_climatology_with_endpoints(
    http: &HttpClient,
    eps: &RainfallEndpoints,
    lat: f64,
    lon: f64,
) -> Result<Option<Value>> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Ok(None);
    }

    let nasa_fut = async {
        let u = build_nasa_power_ag_url(&eps.nasa_power_url, lat, lon).ok()?;
        let j = http
            .get_json_opts(&u, REQ_TIMEOUT_MS, 1, TTL_30D_SECS, None)
            .await
            .ok()?;
        parse_nasa_power_rainfall(&j)
    };

    let atlas14_fut = async {
        let u = Url::parse_with_params(
            &eps.atlas14_csv_url,
            &[
                ("lat", format!("{lat:.6}").as_str()),
                ("lon", format!("{lon:.6}").as_str()),
                ("data", "depth"),
                ("units", "english"),
                ("series", "pds"),
            ],
        )
        .ok()?;
        let csv_txt = http
            .get_text_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_30D_SECS, None)
            .await
            .ok()?;
        Some(parse_atlas14_csv(&csv_txt))
    };

    let acis_fut = fetch_acis_summary(http, &eps.acis_base_url, lat, lon, eps.use_static_cache);

    let map_extent = format!(
        "{:.4},{:.4},{:.4},{:.4}",
        lon - 0.15,
        lat - 0.15,
        lon + 0.15,
        lat + 0.15
    );
    let geom_pt = format!("{lon:.6},{lat:.6}");

    let qpe_fut = async {
        let u = Url::parse_with_params(
            &eps.rfc_qpe_identify_url,
            &[
                ("geometry", geom_pt.as_str()),
                ("geometryType", "esriGeometryPoint"),
                ("sr", "4326"),
                ("layers", "all:28,40,56,68,88,100"),
                ("tolerance", "2"),
                ("mapExtent", map_extent.as_str()),
                ("imageDisplay", "512,512,96"),
                ("returnGeometry", "false"),
                ("f", "json"),
            ],
        )
        .ok()?;
        let j = http
            .get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_1H_SECS, None)
            .await
            .ok()?;
        parse_rfc_qpe_identify(&j)
    };

    let qpf_fut = async {
        let base = format!("{}/query", eps.wpc_qpf_layer_url.trim_end_matches('/'));
        let u = Url::parse_with_params(
            &base,
            &[
                ("geometry", geom_pt.as_str()),
                ("geometryType", "esriGeometryPoint"),
                ("inSR", "4326"),
                ("spatialRel", "esriSpatialRelIntersects"),
                ("outFields", "qpf,units,valid_time,product"),
                ("returnGeometry", "false"),
                ("f", "json"),
            ],
        )
        .ok()?;
        let j = http
            .get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_1H_SECS, None)
            .await
            .ok()?;
        parse_wpc_qpf_7d(&j)
    };

    let ffg_fut = async {
        let u = Url::parse_with_params(
            &eps.rfc_ffg_identify_url,
            &[
                ("geometry", geom_pt.as_str()),
                ("geometryType", "esriGeometryPoint"),
                ("sr", "4326"),
                ("layers", "all:3,7"),
                ("tolerance", "2"),
                ("mapExtent", map_extent.as_str()),
                ("imageDisplay", "512,512,96"),
                ("returnGeometry", "false"),
                ("f", "json"),
            ],
        )
        .ok()?;
        let j = http
            .get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 0, TTL_1H_SECS, None)
            .await
            .ok()?;
        parse_rfc_ffg_identify(&j)
    };

    let (nasa_opt, atlas14_opt, acis, qpe_opt, qpf_opt, ffg_opt) = tokio::join!(
        Box::pin(nasa_fut),
        Box::pin(atlas14_fut),
        Box::pin(acis_fut),
        Box::pin(qpe_fut),
        Box::pin(qpf_fut),
        Box::pin(ffg_fut),
    );

    let (monthly_normals_opt, annual_normal_opt, normal_src): (
        Option<Vec<f64>>,
        Option<f64>,
        Option<&'static str>,
    ) = if let Some(ref n) = nasa_opt {
        (
            Some(n.monthly_in.to_vec()),
            Some(n.annual_in),
            Some("NASA POWER 25-yr (2001–2025)"),
        )
    } else if let Some(ref stn_m) = acis.station_monthly_normals_in {
        (
            Some(stn_m.to_vec()),
            acis.station_annual_normal_in,
            Some("NOAA RCC-ACIS 1991–2020 Normal"),
        )
    } else if let Some(prism_ann) = acis.prism_annual_mean_in {
        (None, Some(prism_ann), Some("NOAA PRISM 10-yr Grid Mean"))
    } else {
        (None, None, None)
    };

    let has_atlas = atlas14_opt
        .as_ref()
        .and_then(|a| a.get("available"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_hist_max = acis.max_1day_in.is_some() || acis.max_3day_in.is_some();

    if annual_normal_opt.is_none()
        && !has_atlas
        && !has_hist_max
        && qpe_opt.is_none()
        && qpf_opt.is_none()
    {
        return Ok(None);
    }

    let historical_max = if has_hist_max || acis.prism_max_1day_in.is_some() {
        json!({
            "max1DayIn": acis.max_1day_in,
            "max1DayDate": acis.max_1day_date,
            "max3DayIn": acis.max_3day_in,
            "max3DayDate": acis.max_3day_date,
            "prismMax1Day10YrIn": acis.prism_max_1day_in,
        })
    } else {
        Value::Null
    };

    let (forecast_qpf_7d, forecast_qpf_valid) = match qpf_opt {
        Some((q, vt)) => (Some(q), vt),
        None => (None, None),
    };

    let fetched_at = now_iso();
    Ok(Some(json!({
        "fetchedAt": fetched_at,
        "referenceUrl": RAINFALL_HOMEPAGE_URL,
        "annualNormalIn": annual_normal_opt,
        "annualNormalSource": normal_src,
        "monthlyNormalsIn": monthly_normals_opt,
        "prismAnnualMean10YrIn": acis.prism_annual_mean_in,
        "stationId": acis.station_id,
        "stationName": acis.station_name,
        "stationAnnualNormalIn": acis.station_annual_normal_in,
        "stationMonthlyNormalsIn": acis.station_monthly_normals_in,
        "wetDaysPerYr": acis.wet_days_per_yr,
        "moderateRainDaysGe05In": acis.mod_rain_days_ge_05_in,
        "heavyRainDaysGe1In": acis.heavy_rain_days_ge_1_in,
        "atlas14": atlas14_opt.unwrap_or(Value::Null),
        "historicalMax": historical_max,
        "observedQpe": qpe_opt.unwrap_or(Value::Null),
        "forecastQpf7dIn": forecast_qpf_7d,
        "forecastQpfValidTime": forecast_qpf_valid,
        "flashFloodGuidance": ffg_opt.unwrap_or(Value::Null),
    })))
}

/// Native `DataExtensionAdapter` exposing rainfall climatology, Atlas 14 design storms, and live QPE/QPF.
pub struct RainfallClimatologyAdapter;

impl DataExtensionAdapter for RainfallClimatologyAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: RAINFALL_EXT_ID.to_string(),
            name: "NOAA / NASA Rainfall Climatology, Atlas 14 Design Storms & Live QPE/QPF"
                .to_string(),
            version: "1.0.0".to_string(),
            description: "Keyless NASA POWER 25-year rainfall climatology, NOAA Atlas 14 PFDS design storm depths (with RCC-ACIS PRISM/station fallback for WA/OR/ID/MT/WY), 30-year normals & historical peak storms, NOAA RFC multi-sensor observed QPE (24h–WYTD), WPC 7-day QPF, and Flash Flood Guidance.".to_string(),
            domain: CapabilityDomain::DomainNaturalHazards as i32,
            additional_domains: vec![CapabilityDomain::DomainNaturalHazards as i32],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                states: Vec::new(),
                counties: Vec::new(),
                zip_prefixes: Vec::new(),
                zip_codes: Vec::new(),
                cities: Vec::new(),
                nationwide: true,
                specificity_override: Some(500),
                country: "US".to_string(),
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                env_var: String::new(),
                header_name: String::new(),
                query_param: String::new(),
                header_prefix: None,
                optional_key: false,
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 4,
                burst: 8,
                timeout_ms: REQ_TIMEOUT_MS as u32,
                max_retries: 1,
            }),
            cache_ttl_secs: RAINFALL_CACHE_TTL_SECS,
            priority: 222,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.HazardSummaryProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some(RAINFALL_HOMEPAGE_URL.to_string()),
            documentation_url: Some(RAINFALL_DOCS_URL.to_string()),
            codeowners: vec!["@openestate/core".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            metering: Some(MeteringSpecProto {
                dimensions: vec![
                    "invocations".to_string(),
                    "wall_time_ms".to_string(),
                    "external_api_calls".to_string(),
                    "bytes_downloaded".to_string(),
                ],
            }),
            sharing: Some(SharingSpecProto {
                scope: ShareScope::Geohash5 as i32,
                ttl_secs: RAINFALL_CACHE_TTL_SECS,
                stale_while_revalidate_secs: 1800,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: RAINFALL_SOURCE_NAME.to_string(),
                url: RAINFALL_HOMEPAGE_URL.to_string(),
                reference_url_template: RAINFALL_HOMEPAGE_URL.to_string(),
                schedule_or_version: None,
                notes: None,
            }),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if http.is_offline_external_blocked() {
                return Ok(Vec::new());
            }
            let country = query
                .extra_params
                .get("country")
                .map(String::as_str)
                .unwrap_or("");
            if !is_us_country_opt(country) {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
                return Ok(Vec::new());
            };
            if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
                return Ok(Vec::new());
            }

            let started = Instant::now();
            let Some(rain_val) = fetch_rainfall_climatology(http, lat, lon).await? else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = rain_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);

            Ok(vec![ExtensionEnvelopeProto {
                extension_id: RAINFALL_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainNaturalHazards as i32,
                priority: 222,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: RAINFALL_EXT_ID.to_string(),
                    source_name: RAINFALL_SOURCE_NAME.to_string(),
                    source_url: RAINFALL_HOMEPAGE_URL.to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&rain_val)?)),
            }])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Query,
        routing::{get, post},
        Json, Router,
    };
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    const SAMPLE_ATLAS14_CAMPBELL_CSV: &str = "\
Point precipitation frequency estimates (inches)
NOAA Atlas 14 Volume 6 Version 2
Data type: Precipitation depth
Time series type: Partial duration
Project area: Southwest

PRECIPITATION FREQUENCY ESTIMATES
by duration for ARI (years):, 1,2,5,10,25,50,100,200,500,1000
60-min:, 0.422,0.512,0.636,0.741,0.891,1.01,1.14,1.28,1.47,1.62
6-hr:, 1.06,1.26,1.55,1.81,2.21,2.55,2.93,3.36,4.00,4.57
24-hr:, 1.72,2.19,2.82,3.34,4.08,4.66,5.28,5.92,6.83,7.55
3-day:, 2.50,3.19,4.08,4.79,5.74,6.45,7.16,7.88,8.84,9.56
";

    const SAMPLE_ATLAS14_SEATTLE_ERROR: &str = "\
result = 'none';
ErrorMsg =  'Error 3.0: Selected location is not within a project area';
";

    #[tokio::test]
    async fn offline_and_non_us_queries_return_empty_without_fabricating_rainfall() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let adapter = RainfallClimatologyAdapter;

        let us_q = ExtensionQueryProto {
            lat: Some(37.2821),
            lon: Some(-121.9618),
            state: "CA".to_string(),
            ..Default::default()
        };
        let envs = adapter.execute_async(&us_q, &http, &env).await.unwrap();
        assert!(envs.is_empty(), "offline mode must never fabricate rainfall");
        assert!(
            fetch_rainfall_climatology(&http, 37.2821, -121.9618)
                .await
                .unwrap()
                .is_none()
        );

        let mut extra_params = HashMap::new();
        extra_params.insert("country".to_string(), "CA".to_string());
        let non_us = ExtensionQueryProto {
            lat: Some(49.2827),
            lon: Some(-123.1207),
            extra_params,
            ..Default::default()
        };
        let unblocked = HttpClient::new(None);
        let envs2 = adapter.execute_async(&non_us, &unblocked, &env).await.unwrap();
        assert!(envs2.is_empty(), "non-US query must skip US rainfall feeds");
    }

    #[test]
    fn parses_atlas14_csv_and_handles_pnw_error_msg_gracefully() {
        let ok = parse_atlas14_csv(SAMPLE_ATLAS14_CAMPBELL_CSV);
        assert_eq!(ok["available"], json!(true));
        assert_eq!(ok["volume"], json!("NOAA Atlas 14 Volume 6 Version 2"));
        assert_eq!(ok["hr1Yr10In"], json!(0.74));
        assert_eq!(ok["hr1Yr100In"], json!(1.14));
        assert_eq!(ok["hr24Yr2In"], json!(2.19));
        assert_eq!(ok["hr24Yr10In"], json!(3.34));
        assert_eq!(ok["hr24Yr100In"], json!(5.28));
        assert_eq!(ok["day3Yr100In"], json!(7.16));

        let pnw = parse_atlas14_csv(SAMPLE_ATLAS14_SEATTLE_ERROR);
        assert_eq!(pnw["available"], json!(false));
        assert!(pnw["errorMsg"]
            .as_str()
            .unwrap_or("")
            .contains("not within a project area"));
        assert!(pnw["hr24Yr100In"].is_null());
    }

    #[test]
    fn parses_rfc_qpe_negative_sentinel_as_zero_and_ffg_mm_to_inches() {
        let qpe_raw = json!({
            "results": [
                { "layerId": 28, "attributes": { "Service Pixel Value": "-10000", "name": "nws_precip_last24hours_conus" } },
                { "layerId": 40, "attributes": { "Service Pixel Value": "0.791339", "name": "nws_precip_last3days_conus" } },
                { "layerId": 56, "attributes": { "Service Pixel Value": "NoData", "name": "nws_precip_last7days_conus" } },
                { "layerId": 68, "attributes": { "Service Pixel Value": "1.77165", "name": "nws_precip_last30days_conus" } },
                { "layerId": 88, "attributes": { "Service Pixel Value": "14.3976", "name": "nws_precip_last365days_conus" } },
                { "layerId": 100, "attributes": { "Service Pixel Value": "1.77165", "name": "nws_precip_wytd_conus" } }
            ]
        });
        let qpe = parse_rfc_qpe_identify(&qpe_raw).expect("valid QPE");
        assert_eq!(qpe["h24In"], json!(0.0));
        assert_eq!(qpe["h72In"], json!(0.79));
        assert_eq!(qpe["d7In"], json!(0.0));
        assert_eq!(qpe["d30In"], json!(1.77));
        assert_eq!(qpe["d365In"], json!(14.4));
        assert_eq!(qpe["wytdIn"], json!(1.77));

        let ffg_raw = json!({
            "results": [
                { "layerId": 3, "attributes": { "Service Pixel Value": "19.0257", "name": "5kmffg_01" } },
                { "layerId": 7, "attributes": { "Service Pixel Value": "33.8664", "name": "5kmffg_03" } }
            ]
        });
        let ffg = parse_rfc_ffg_identify(&ffg_raw).expect("valid FFG");
        assert_eq!(ffg["hr1In"], json!(0.75));
        assert_eq!(ffg["hr3In"], json!(1.33));
    }

    #[tokio::test]
    async fn full_mock_pipeline_handles_pnw_atlas14_fallback_to_acis_and_nasa() {
        let app = Router::new()
            .route(
                "/nasa",
                get(|| async {
                    Json(json!({
                        "properties": {
                            "parameter": {
                                "PRECTOTCORR": {
                                    "JAN": 4.5, "FEB": 3.8, "MAR": 3.2, "APR": 2.1,
                                    "MAY": 1.4, "JUN": 1.0, "JUL": 0.4, "AUG": 0.6,
                                    "SEP": 1.2, "OCT": 2.9, "NOV": 4.9, "DEC": 5.1,
                                    "ANN": 2.58
                                }
                            }
                        }
                    }))
                }),
            )
            .route(
                "/atlas14",
                get(|Query(params): Query<HashMap<String, String>>| async move {
                    let lat = params
                        .get("lat")
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    if lat > 45.0 {
                        SAMPLE_ATLAS14_SEATTLE_ERROR.to_string()
                    } else {
                        SAMPLE_ATLAS14_CAMPBELL_CSV.to_string()
                    }
                }),
            )
            .route(
                "/acis/GridData",
                post(|| async {
                    Json(json!({
                        "data": [
                            ["2023", 36.40, 2.85, 148.0, 24.0, 6.0],
                            ["2024", 38.20, 3.25, 154.0, 28.0, 8.0]
                        ]
                    }))
                }),
            )
            .route(
                "/acis/StnMeta",
                post(|| async {
                    Json(json!({
                        "meta": [{
                            "uid": 999,
                            "name": "SEATTLE TACOMA AP",
                            "sids": ["USW00024233 6"],
                            "ll": [-122.3144, 47.4447],
                            "valid_daterange": [["1945-01-01", "2026-11-01"]]
                        }]
                    }))
                }),
            )
            .route(
                "/acis/StnData",
                post(|Json(body): Json<Value>| async move {
                    let is_norm = body
                        .pointer("/elems/0/normal")
                        .and_then(Value::as_str)
                        .is_some();
                    if is_norm {
                        Json(json!({
                            "data": [
                                ["2024-01", "5.78"], ["2024-02", "3.76"], ["2024-03", "4.17"],
                                ["2024-04", "3.18"], ["2024-05", "1.88"], ["2024-06", "1.45"],
                                ["2024-07", "0.60"], ["2024-08", "0.97"], ["2024-09", "1.61"],
                                ["2024-10", "3.91"], ["2024-11", "6.31"], ["2024-12", "5.72"]
                            ]
                        }))
                    } else {
                        Json(json!({
                            "smry": [["5.02", "2003-10-20"], ["6.41", "2007-12-04"]]
                        }))
                    }
                }),
            )
            .route(
                "/qpe",
                get(|| async {
                    Json(json!({
                        "results": [
                            { "layerId": 28, "attributes": { "Service Pixel Value": "-10000" } },
                            { "layerId": 40, "attributes": { "Service Pixel Value": "1.25" } },
                            { "layerId": 56, "attributes": { "Service Pixel Value": "2.40" } }
                        ]
                    }))
                }),
            )
            .route(
                "/qpf/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": { "qpf": 1.75, "units": "Inches", "valid_time": "7d" }
                        }]
                    }))
                }),
            )
            .route(
                "/ffg",
                get(|| async {
                    Json(json!({
                        "results": [
                            { "layerId": 3, "attributes": { "Service Pixel Value": "25.4" } },
                            { "layerId": 7, "attributes": { "Service Pixel Value": "50.8" } }
                        ]
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base = format!("http://{addr}");
        let eps = RainfallEndpoints {
            nasa_power_url: format!("{base}/nasa"),
            atlas14_csv_url: format!("{base}/atlas14"),
            acis_base_url: format!("{base}/acis"),
            rfc_qpe_identify_url: format!("{base}/qpe"),
            wpc_qpf_layer_url: format!("{base}/qpf"),
            rfc_ffg_identify_url: format!("{base}/ffg"),
            use_static_cache: false,
        };
        let http = HttpClient::offline_loopback_only(None);

        // Seattle (lat 47.6062) -> Atlas 14 returns ErrorMsg, ACIS + NASA fill in historical max & normals!
        let seattle = fetch_rainfall_climatology_with_endpoints(&http, &eps, 47.6062, -122.3321)
            .await
            .unwrap()
            .expect("Seattle rainfall payload");
        assert_eq!(seattle["atlas14"]["available"], json!(false));
        assert_eq!(seattle["historicalMax"]["max1DayIn"], json!(5.02));
        assert_eq!(seattle["historicalMax"]["max1DayDate"], json!("2003-10-20"));
        assert_eq!(seattle["historicalMax"]["max3DayIn"], json!(6.41));
        assert_eq!(seattle["wetDaysPerYr"], json!(151.0));
        assert_eq!(seattle["heavyRainDaysGe1In"], json!(7.0));
        assert_eq!(seattle["observedQpe"]["h24In"], json!(0.0));
        assert_eq!(seattle["observedQpe"]["h72In"], json!(1.25));
        assert_eq!(seattle["forecastQpf7dIn"], json!(1.75));
        assert_eq!(seattle["flashFloodGuidance"]["hr1In"], json!(1.0));
        assert_eq!(seattle["flashFloodGuidance"]["hr3In"], json!(2.0));
        assert_eq!(
            seattle["monthlyNormalsIn"].as_array().unwrap().len(),
            12
        );
    }
}
