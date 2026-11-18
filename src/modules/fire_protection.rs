//! Fire protection infrastructure, hydrant verification & wildfire/roof insurability (`fire_protection`).
//!
//! Queries keyless public-domain US federal and OpenStreetMap fire-suppression feeds:
//! - **Primary Fire Stations**: FEMA RAPT / HIFLD Critical Infrastructure Layer 12
//! - **Federal `.gov` Backup**: USGS National Map Structures Layer 16 (`fcode = 74026`)
//! - **OSM Supplement**: Esri Hosted North America OSM Amenities (`amenity = 'fire_station'`)
//! - **Hydrants**: Esri Hosted North America OSM POIs (`emergency = 'fire_hydrant'`) + short
//!   hedged (`2.5s`) Overpass API `node["emergency"="fire_hydrant"]` supplement
//!
//! Computes an open-data **ISO Public Protection Class (PPC 1–10) proxy** from verified station
//! distance (`<= 5` estimated road miles) and `<= 1,000 ft` (`304.8 m`) hydrant / municipal water
//! availability, fuses `house3d` 3DEP LiDAR `metrics.canopyNearRoof` overhang and
//! `permit_roof_extractor` reroof age vs. `15/20`-yr carrier underwriting cliffs, and emits a
//! Leaflet-ready `fire-protection` GeoJSON map layer.
//!
//! Never fabricates fire stations, hydrants, or proprietary commercial ISO filings when offline
//! or when upstream layers lack coverage.

use anyhow::{anyhow, Result};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use url::Url;

use crate::extensions::adapter::{BoxFuture, DataExtensionAdapter};
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind,
    ExecutionMode, ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto,
    ExtensionTier, GeoScopeProto, LicenseClass, ManifestSourceAttributionProto,
    MeteringSpecProto, ProvenanceTier, RateLimitSpecProto, ShareScope, SharingSpecProto,
    SourceAttributionProto,
};
use crate::metering::add_bytes_downloaded;
use crate::property::permit_pipeline::extract_roof_lifecycle;
use crate::shared::{
    bearing_deg, compass8, haversine_m, now_iso, round_n, unwrap_data, EnvConfig, HttpClient,
    MILE_M,
};

pub const FIRE_PROTECTION_EXT_ID: &str = "fire_protection_hydrants";
pub const FIRE_PROTECTION_SOURCE_NAME: &str =
    "FEMA RAPT / HIFLD Fire Stations, USGS National Map Structures & OpenStreetMap Contributors (ODbL)";
pub const FIRE_PROTECTION_HOMEPAGE_URL: &str =
    "https://www.fema.gov/emergency-managers/practitioners/resilience-analysis-and-planning-tool";
pub const FIRE_PROTECTION_DOCS_URL: &str =
    "https://www.usgs.gov/programs/national-geospatial-program/national-map";

pub const FEMA_RAPT_FIRE_STATIONS_URL: &str =
    "https://services.arcgis.com/XG15cJAlne2vxtgt/arcgis/rest/services/Critical_Infrastructure_Map_Service/FeatureServer/12/query";
pub const USGS_STRUCTURES_FIRE_STATIONS_URL: &str =
    "https://carto.nationalmap.gov/arcgis/rest/services/structures/MapServer/16/query";
pub const ESRI_OSM_FIRE_STATIONS_URL: &str =
    "https://services6.arcgis.com/Do88DoK2xjTUCXd1/arcgis/rest/services/OSM_NA_Amenities/FeatureServer/0/query";
pub const ESRI_OSM_HYDRANTS_URL: &str =
    "https://services6.arcgis.com/Do88DoK2xjTUCXd1/arcgis/rest/services/OSM_NA_POIs/FeatureServer/0/query";
pub const DEFAULT_OVERPASS_HYDRANT_ENDPOINTS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

pub const FIRE_PROTECTION_CACHE_TTL_SECS: i64 = 7 * 86_400;
pub const FIRE_PROTECTION_SHARE_TTL_SECS: u64 = 30 * 86_400;

/// Search radius for fire stations (`10.0 mi = 16,093 m`) so we reliably test the `5.0` road-mile ISO PPC 10 boundary.
const STATION_SEARCH_RADIUS_M: f64 = 16_093.0;
/// Search radius for fire hydrants (`~2,000 ft = 610 m`) around the `1,000 ft` (`304.8 m`) ISO hydrant threshold.
const HYDRANT_SEARCH_RADIUS_M: f64 = 610.0;
/// ISO Public Protection Class hydrant threshold: `1,000 ft = 304.8 m`.
pub const ISO_HYDRANT_THRESHOLD_M: f64 = 304.8;
/// Road-network tortuosity multiplier converting straight-line miles to estimated road response miles.
const ROAD_TORTUOSITY_FACTOR: f64 = 1.28;
/// ISO PPC 10 unprotected threshold (`5.0` road miles).
pub const ISO_MAX_PROTECTED_ROAD_MI: f64 = 5.0;

pub const ISO_PPC_PROXY_DISCLAIMER: &str =
    "Open-data fire-protection & ISO Public Protection Class (PPC 1–10) proxy derived from FEMA RAPT / USGS / OpenStreetMap station & hydrant coordinates and public water records — NOT a commercial Verisk/ISO Community Mitigation filing.";

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

fn is_loopback_host(host: &str) -> bool {
    let lc = host.trim().to_ascii_lowercase();
    lc == "localhost" || lc == "127.0.0.1" || lc == "::1" || lc == "[::1]"
}

fn url_is_loopback(raw: &str) -> bool {
    Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(is_loopback_host))
        .unwrap_or(false)
}

fn has_loopback_override(env: &EnvConfig) -> bool {
    [
        "FIRE_STATIONS_FEMA_URL",
        "FIRE_STATIONS_USGS_URL",
        "FIRE_STATIONS_OSM_URL",
        "FIRE_HYDRANTS_OSM_URL",
        "OVERPASS_ENDPOINTS",
    ]
    .iter()
    .filter_map(|k| env.get(k))
    .flat_map(|v| v.split([',', ' ']))
    .any(|u| url_is_loopback(u.trim()))
}

fn json_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn json_f64(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    if let Some(n) = val.as_f64() {
        return n.is_finite().then_some(n);
    }
    if let Some(i) = val.as_i64() {
        return Some(i as f64);
    }
    val.as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite())
}

/// Normalizes `(x, y)` coordinates that may arrive in either WGS84 degrees (`outSR=4326`)
/// or EPSG:3857 Web Mercator meters into `(lat, lon)` in WGS84 degrees.
pub fn normalize_xy_to_lat_lon(x: f64, y: f64) -> Option<(f64, f64)> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    if x.abs() <= 180.0 && y.abs() <= 90.0 {
        if x == 0.0 && y == 0.0 {
            return None;
        }
        return Some((y, x));
    }
    // EPSG:3857 Web Mercator meters -> WGS84 degrees
    const ORIGIN_SHIFT: f64 = 20_037_508.342_789_244;
    if x.abs() <= ORIGIN_SHIFT * 1.05 && y.abs() <= ORIGIN_SHIFT * 1.05 {
        let lon = (x / ORIGIN_SHIFT) * 180.0;
        let lat_rad = (std::f64::consts::PI * (y / ORIGIN_SHIFT)).exp().atan() * 2.0
            - std::f64::consts::FRAC_PI_2;
        let lat = lat_rad.to_degrees();
        if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
            return Some((lat, lon));
        }
    }
    None
}

fn extract_arcgis_point(feat: &Value) -> Option<(f64, f64)> {
    if let Some(geom) = feat.get("geometry") {
        if let (Some(x), Some(y)) = (json_f64(geom.get("x")), json_f64(geom.get("y"))) {
            if let Some(pt) = normalize_xy_to_lat_lon(x, y) {
                return Some(pt);
            }
        }
        if let Some(rings) = geom.get("rings").and_then(Value::as_array) {
            if let Some(outer) = rings.first().and_then(Value::as_array) {
                let mut sx = 0.0;
                let mut sy = 0.0;
                let mut n = 0usize;
                for p in outer {
                    if let Some(arr) = p.as_array() {
                        if let (Some(x), Some(y)) = (json_f64(arr.first()), json_f64(arr.get(1))) {
                            if let Some((la, lo)) = normalize_xy_to_lat_lon(x, y) {
                                sx += lo;
                                sy += la;
                                n += 1;
                            }
                        }
                    }
                }
                if n > 0 {
                    return Some((sy / n as f64, sx / n as f64));
                }
            }
        }
    }
    if let Some(attrs) = feat.get("attributes") {
        if let (Some(x), Some(y)) = (
            json_f64(attrs.get("X").or_else(|| attrs.get("lon"))),
            json_f64(attrs.get("Y").or_else(|| attrs.get("lat"))),
        ) {
            return normalize_xy_to_lat_lon(x, y);
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FireStationRecord {
    pub name: String,
    pub department: Option<String>,
    pub address: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    pub fdid: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub distance_meters: i64,
    pub distance_straight_mi: f64,
    pub est_road_mi: f64,
    pub direction: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FireHydrantRecord {
    pub id: String,
    pub name: Option<String>,
    pub operator: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub distance_meters: i64,
    pub distance_ft: i64,
    pub within_1000_ft: bool,
    pub direction: String,
    pub source: String,
}

fn build_station_record(
    subject_lat: f64,
    subject_lon: f64,
    st_lat: f64,
    st_lon: f64,
    name: String,
    department: Option<String>,
    address: Option<String>,
    city: Option<String>,
    state: Option<String>,
    zip: Option<String>,
    fdid: Option<String>,
    source: &str,
) -> Option<FireStationRecord> {
    let dist_m = haversine_m(subject_lat, subject_lon, st_lat, st_lon);
    if !dist_m.is_finite() || dist_m > STATION_SEARCH_RADIUS_M * 1.25 {
        return None;
    }
    let straight_mi = dist_m / MILE_M;
    let road_mi = straight_mi * ROAD_TORTUOSITY_FACTOR;
    let dir = compass8(bearing_deg(subject_lat, subject_lon, st_lat, st_lon)).to_string();
    Some(FireStationRecord {
        name,
        department,
        address,
        city,
        state,
        zip,
        fdid,
        lat: round_n(st_lat, 6),
        lon: round_n(st_lon, 6),
        distance_meters: dist_m.round() as i64,
        distance_straight_mi: round_n(straight_mi, 2),
        est_road_mi: round_n(road_mi, 2),
        direction: dir,
        source: source.to_string(),
    })
}

fn parse_fema_stations(v: &Value, lat: f64, lon: f64) -> Vec<FireStationRecord> {
    let Some(feats) = v.get("features").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in feats {
        let Some((st_lat, st_lon)) = extract_arcgis_point(f) else {
            continue;
        };
        let Some(a) = f.get("attributes") else {
            continue;
        };
        let name = json_str(a.get("NAME"))
            .unwrap_or("Fire Station")
            .to_string();
        let dept = json_str(a.get("OWNER"))
            .filter(|s| !s.eq_ignore_ascii_case("NOT AVAILABLE"))
            .map(String::from);
        let addr = json_str(a.get("ADDRESS"))
            .filter(|s| !s.eq_ignore_ascii_case("NOT AVAILABLE"))
            .map(String::from);
        let city = json_str(a.get("CITY")).map(String::from);
        let state = json_str(a.get("STATE")).map(String::from);
        let zip = json_str(a.get("ZIP")).map(String::from);
        let fdid = json_str(a.get("FDID"))
            .filter(|s| !s.eq_ignore_ascii_case("NOT AVAILABLE"))
            .map(String::from);
        if let Some(rec) = build_station_record(
            lat,
            lon,
            st_lat,
            st_lon,
            name,
            dept,
            addr,
            city,
            state,
            zip,
            fdid,
            "fema_rapt_hifld",
        ) {
            out.push(rec);
        }
    }
    out
}

fn parse_usgs_stations(v: &Value, lat: f64, lon: f64) -> Vec<FireStationRecord> {
    let Some(feats) = v.get("features").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in feats {
        let Some((st_lat, st_lon)) = extract_arcgis_point(f) else {
            continue;
        };
        let Some(a) = f.get("attributes") else {
            continue;
        };
        let name = json_str(a.get("name").or_else(|| a.get("NAME")))
            .unwrap_or("Fire Station")
            .to_string();
        let addr = json_str(a.get("address").or_else(|| a.get("ADDRESS"))).map(String::from);
        let city = json_str(a.get("city").or_else(|| a.get("CITY"))).map(String::from);
        let state = json_str(a.get("state").or_else(|| a.get("STATE"))).map(String::from);
        let zip = json_str(a.get("zipcode").or_else(|| a.get("ZIP"))).map(String::from);
        if let Some(rec) = build_station_record(
            lat,
            lon,
            st_lat,
            st_lon,
            name,
            None,
            addr,
            city,
            state,
            zip,
            None,
            "usgs_national_map_structures",
        ) {
            out.push(rec);
        }
    }
    out
}

fn parse_osm_arcgis_stations(v: &Value, lat: f64, lon: f64) -> Vec<FireStationRecord> {
    let Some(feats) = v.get("features").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in feats {
        let Some((st_lat, st_lon)) = extract_arcgis_point(f) else {
            continue;
        };
        let Some(a) = f.get("attributes") else {
            continue;
        };
        let name = json_str(a.get("name").or_else(|| a.get("operator")))
            .unwrap_or("Fire Station")
            .to_string();
        let dept = json_str(a.get("operator")).map(String::from);
        let street = match (json_str(a.get("addr_housenumber")), json_str(a.get("addr_street"))) {
            (Some(hn), Some(st)) => Some(format!("{hn} {st}")),
            (None, Some(st)) => Some(st.to_string()),
            _ => None,
        };
        let city = json_str(a.get("addr_city")).map(String::from);
        let state = json_str(a.get("addr_state")).map(String::from);
        let zip = json_str(a.get("addr_postcode")).map(String::from);
        if let Some(rec) = build_station_record(
            lat,
            lon,
            st_lat,
            st_lon,
            name,
            dept,
            street,
            city,
            state,
            zip,
            None,
            "osm_na_amenities",
        ) {
            out.push(rec);
        }
    }
    out
}

fn merge_dedup_stations(groups: Vec<Vec<FireStationRecord>>) -> Vec<FireStationRecord> {
    let mut merged: Vec<FireStationRecord> = Vec::new();
    for group in groups {
        for cand in group {
            let dup = merged
                .iter()
                .any(|existing| haversine_m(existing.lat, existing.lon, cand.lat, cand.lon) <= 135.0);
            if !dup {
                merged.push(cand);
            }
        }
    }
    merged.sort_by(|a, b| a.distance_meters.cmp(&b.distance_meters));
    merged.truncate(12);
    merged
}

fn build_hydrant_record(
    subject_lat: f64,
    subject_lon: f64,
    h_lat: f64,
    h_lon: f64,
    id: String,
    name: Option<String>,
    operator: Option<String>,
    source: &str,
) -> Option<FireHydrantRecord> {
    let dist_m = haversine_m(subject_lat, subject_lon, h_lat, h_lon);
    if !dist_m.is_finite() || dist_m > HYDRANT_SEARCH_RADIUS_M * 1.25 {
        return None;
    }
    let dist_ft = (dist_m * 3.280_84).round() as i64;
    let dir = compass8(bearing_deg(subject_lat, subject_lon, h_lat, h_lon)).to_string();
    Some(FireHydrantRecord {
        id,
        name,
        operator,
        lat: round_n(h_lat, 6),
        lon: round_n(h_lon, 6),
        distance_meters: dist_m.round() as i64,
        distance_ft: dist_ft,
        within_1000_ft: dist_m <= ISO_HYDRANT_THRESHOLD_M,
        direction: dir,
        source: source.to_string(),
    })
}

fn parse_osm_arcgis_hydrants(v: &Value, lat: f64, lon: f64) -> Vec<FireHydrantRecord> {
    let Some(feats) = v.get("features").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (idx, f) in feats.iter().enumerate() {
        let Some((h_lat, h_lon)) = extract_arcgis_point(f) else {
            continue;
        };
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let id = json_str(a.get("osm_id2"))
            .map(|s| format!("osm:{s}"))
            .or_else(|| {
                a.get("OBJECTID")
                    .and_then(Value::as_i64)
                    .map(|n| format!("poi:{n}"))
            })
            .unwrap_or_else(|| format!("poi_idx:{idx}"));
        let name = json_str(a.get("name").or_else(|| a.get("description"))).map(String::from);
        let operator = json_str(a.get("operator")).map(String::from);
        if let Some(rec) = build_hydrant_record(
            lat,
            lon,
            h_lat,
            h_lon,
            id,
            name,
            operator,
            "osm_na_pois",
        ) {
            out.push(rec);
        }
    }
    out
}

fn parse_overpass_hydrants(v: &Value, lat: f64, lon: f64) -> Vec<FireHydrantRecord> {
    let Some(elems) = v.get("elements").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for el in elems {
        let h_lat = json_f64(el.get("lat"))
            .or_else(|| json_f64(el.pointer("/center/lat")));
        let h_lon = json_f64(el.get("lon"))
            .or_else(|| json_f64(el.pointer("/center/lon")));
        let (Some(hla), Some(hlo)) = (h_lat, h_lon) else {
            continue;
        };
        let Some((norm_lat, norm_lon)) = normalize_xy_to_lat_lon(hlo, hla) else {
            continue;
        };
        let id = el
            .get("id")
            .and_then(Value::as_i64)
            .map(|n| format!("osm_node:{n}"))
            .unwrap_or_else(|| "osm_node".to_string());
        let tags = el.get("tags").unwrap_or(&Value::Null);
        let name = json_str(tags.get("ref").or_else(|| tags.get("name"))).map(String::from);
        let operator = json_str(tags.get("operator")).map(String::from);
        if let Some(rec) = build_hydrant_record(
            lat,
            lon,
            norm_lat,
            norm_lon,
            id,
            name,
            operator,
            "osm_overpass_live",
        ) {
            out.push(rec);
        }
    }
    out
}

fn merge_dedup_hydrants(groups: Vec<Vec<FireHydrantRecord>>) -> Vec<FireHydrantRecord> {
    let mut merged: Vec<FireHydrantRecord> = Vec::new();
    for group in groups {
        for cand in group {
            let dup = merged
                .iter()
                .any(|ex| haversine_m(ex.lat, ex.lon, cand.lat, cand.lon) <= 8.5);
            if !dup {
                merged.push(cand);
            }
        }
    }
    merged.sort_by(|a, b| a.distance_meters.cmp(&b.distance_meters));
    merged.truncate(24);
    merged
}

async fn query_arcgis_point_radius(
    http: &HttpClient,
    base_url: &str,
    lat: f64,
    lon: f64,
    distance_m: f64,
    where_clause: Option<&str>,
    out_fields: &str,
    max_records: usize,
    timeout_ms: u64,
) -> Result<Value> {
    let mut url = Url::parse(base_url)?;
    let host = url.host_str().unwrap_or("");
    if http.is_offline_external_blocked() && !is_loopback_host(host) {
        return Err(anyhow!("offline external blocked"));
    }
    {
        let mut qp = url.query_pairs_mut();
        if let Some(w) = where_clause {
            qp.append_pair("where", w);
        }
        qp.append_pair("geometry", &format!("{lon:.6},{lat:.6}"));
        qp.append_pair("geometryType", "esriGeometryPoint");
        qp.append_pair("inSR", "4326");
        qp.append_pair("spatialRel", "esriSpatialRelIntersects");
        qp.append_pair("distance", &format!("{}", distance_m.round() as i64));
        qp.append_pair("units", "esriSRUnit_Meter");
        qp.append_pair("outFields", out_fields);
        qp.append_pair("outSR", "4326");
        qp.append_pair("returnGeometry", "true");
        qp.append_pair("resultRecordCount", &max_records.to_string());
        qp.append_pair("f", "json");
    }

    let cache_key = format!("fire_arcgis:v1:{}", url.as_str());
    if !http.is_bypassing_cache_reads() {
        if let Some(cached) = http.read_cache(&cache_key, FIRE_PROTECTION_CACHE_TTL_SECS).await {
            if let Ok(v) = serde_json::from_str::<Value>(&cached) {
                return Ok(v);
            }
        }
    }

    let client = http.direct_client_for_url(url.as_str())?;
    let req = client
        .get(url)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(timeout_ms));
    let resp = tokio::time::timeout(Duration::from_millis(timeout_ms + 400), req.send()).await??;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await?;
    add_bytes_downloaded(bytes.len() as u64);
    let val = serde_json::from_slice::<Value>(&bytes)?;
    if let Some(err) = val.get("error") {
        return Err(anyhow!("ArcGIS error: {err}"));
    }
    if let Ok(raw) = serde_json::to_string(&val) {
        http.write_cache(&cache_key, &raw, FIRE_PROTECTION_CACHE_TTL_SECS)
            .await;
    }
    Ok(val)
}

async fn query_hedged_overpass_hydrants(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
) -> Vec<FireHydrantRecord> {
    let endpoints: Vec<String> = if let Some(custom) = env.get("OVERPASS_ENDPOINTS") {
        custom
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    } else {
        DEFAULT_OVERPASS_HYDRANT_ENDPOINTS
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    if endpoints.is_empty() {
        return Vec::new();
    }

    let cache_key = format!("fire_overpass_hyd:v1:{lat:.4}:{lon:.4}");
    if !http.is_bypassing_cache_reads() {
        if let Some(cached) = http.read_cache(&cache_key, FIRE_PROTECTION_CACHE_TTL_SECS).await {
            if let Ok(v) = serde_json::from_str::<Value>(&cached) {
                return parse_overpass_hydrants(&v, lat, lon);
            }
        }
    }

    let ql = format!(
        "[out:json][timeout:2];node[\"emergency\"=\"fire_hydrant\"](around:{},{lat:.5},{lon:.5});out body 50;",
        HYDRANT_SEARCH_RADIUS_M.round() as i64
    );

    let fut = async {
        for ep in endpoints.iter().take(2) {
            let Ok(mut url) = Url::parse(ep) else {
                continue;
            };
            let host = url.host_str().unwrap_or("");
            if http.is_offline_external_blocked() && !is_loopback_host(host) {
                continue;
            }
            url.query_pairs_mut().append_pair("data", &ql);
            let Ok(client) = http.direct_client_for_url(url.as_str()) else {
                continue;
            };
            let req = client
                .get(url)
                .header("Accept", "application/json")
                .header("User-Agent", "OpenEstate/1.0")
                .timeout(Duration::from_millis(2_200));
            if let Ok(Ok(resp)) = tokio::time::timeout(Duration::from_millis(2_300), req.send()).await
            {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes().await {
                        add_bytes_downloaded(bytes.len() as u64);
                        if let Ok(val) = serde_json::from_slice::<Value>(&bytes) {
                            if val.get("elements").is_some() {
                                if let Ok(raw) = serde_json::to_string(&val) {
                                    http.write_cache(&cache_key, &raw, FIRE_PROTECTION_CACHE_TTL_SECS)
                                        .await;
                                }
                                return parse_overpass_hydrants(&val, lat, lon);
                            }
                        }
                    }
                }
            }
        }
        Vec::new()
    };

    tokio::time::timeout(Duration::from_millis(2_500), fut)
        .await
        .unwrap_or_default()
}

/// Computes the open-data ISO Public Protection Class (PPC 1–10) proxy from station & hydrant proximity.
pub fn compute_iso_ppc_proxy(
    stations: &[FireStationRecord],
    hydrants: &[FireHydrantRecord],
    municipal_water_confirmed: bool,
    is_likely_private_well: bool,
    is_rural_area: bool,
) -> Value {
    let nearest_station = stations.first();
    let stations_within_5_road_mi = stations
        .iter()
        .filter(|s| s.est_road_mi <= ISO_MAX_PROTECTED_ROAD_MI)
        .count();
    let station_within_5_mi = stations_within_5_road_mi > 0;

    let hydrants_1000_ft = hydrants.iter().filter(|h| h.within_1000_ft).count();
    let any_hydrants_mapped = !hydrants.is_empty();

    let (hydrant_within_1000_ft, water_supply_status): (Option<bool>, &'static str) =
        if hydrants_1000_ft > 0 {
            (Some(true), "verified_hydrant_within_1000ft")
        } else if is_likely_private_well {
            (Some(false), "private_well_or_unverified_rural_water")
        } else if any_hydrants_mapped && is_rural_area && !municipal_water_confirmed {
            (Some(false), "hydrant_beyond_1000ft")
        } else if municipal_water_confirmed {
            (None, "municipal_water_hydrant_unmapped_in_osm")
        } else if is_rural_area {
            (Some(false), "private_well_or_unverified_rural_water")
        } else {
            (None, "unverified_osm_hydrant_gap")
        };

    let (ppc_class, ppc_band, basis): (i64, &'static str, String) = match nearest_station {
        None => (
            10,
            "PPC 10 Proxy (Unprotected / >5 Est. Travel Miles)",
            "No mapped fire station found within 10 straight-line miles (>5 road miles); verify rural/volunteer fire district substation."
                .to_string(),
        ),
        Some(st) if st.est_road_mi > ISO_MAX_PROTECTED_ROAD_MI => (
            10,
            "PPC 10 Proxy (Unprotected / >5 Est. Travel Miles)",
            format!(
                "Nearest mapped fire station ({}) is ~{:.2} est. road mi ({:.2} mi straight-line), exceeding the 5-road-mile ISO protected threshold.",
                st.name, st.est_road_mi, st.distance_straight_mi
            ),
        ),
        Some(st) if hydrant_within_1000_ft == Some(false) => (
            9,
            "PPC 8B/9 Proxy (Station <=5 Road Mi, Hydrant >1,000 ft / Rural Water)",
            format!(
                "Fire station ({}) is within ~{:.2} est. road mi, but parcel lacks a mapped fire hydrant within 1,000 ft / municipal pressurized main ({water_supply_status}).",
                st.name, st.est_road_mi
            ),
        ),
        Some(st) if hydrant_within_1000_ft == Some(true) => {
            let nearest_hyd_ft = hydrants
                .first()
                .map(|h| h.distance_ft)
                .unwrap_or(0);
            if st.est_road_mi <= 1.5 && stations_within_5_road_mi >= 3 {
                (
                    2,
                    "PPC 1–3 Proxy (Urban Hydrant <=1,000 ft + Multi-Station Coverage)",
                    format!(
                        "Nearest station ({}) ~{:.2} est. road mi ({} stations <=5 road mi) + mapped fire hydrant at {} ft (<=1,000 ft).",
                        st.name, st.est_road_mi, stations_within_5_road_mi, nearest_hyd_ft
                    ),
                )
            } else if st.est_road_mi <= 2.5 && stations_within_5_road_mi >= 2 {
                (
                    3,
                    "PPC 2–4 Proxy (Protected Hydrant <=1,000 ft + Close Station)",
                    format!(
                        "Nearest station ({}) ~{:.2} est. road mi ({} stations <=5 road mi) + mapped fire hydrant at {} ft (<=1,000 ft).",
                        st.name, st.est_road_mi, stations_within_5_road_mi, nearest_hyd_ft
                    ),
                )
            } else if st.est_road_mi <= 3.5 {
                (
                    4,
                    "PPC 3–5 Proxy (Protected Hydrant <=1,000 ft + Station <=3.5 Road Mi)",
                    format!(
                        "Nearest station ({}) ~{:.2} est. road mi + mapped fire hydrant at {} ft (<=1,000 ft).",
                        st.name, st.est_road_mi, nearest_hyd_ft
                    ),
                )
            } else {
                (
                    5,
                    "PPC 4–6 Proxy (Protected Hydrant <=1,000 ft + Outer Station Ring <=5 Road Mi)",
                    format!(
                        "Nearest station ({}) ~{:.2} est. road mi (<=5 road mi) + mapped fire hydrant at {} ft (<=1,000 ft).",
                        st.name, st.est_road_mi, nearest_hyd_ft
                    ),
                )
            }
        }
        Some(st) => {
            let water_note = if municipal_water_confirmed {
                "municipal public water confirmed (confirm curb hydrant <=1,000 ft on site)"
            } else {
                "curb hydrants not yet mapped in OpenStreetMap (confirm hydrant <=1,000 ft on site)"
            };
            if st.est_road_mi <= 2.0 && stations_within_5_road_mi >= 2 {
                (
                    3,
                    "PPC 3–4 Proxy (Close Station <=2 Road Mi; Confirm Hydrant <=1,000 ft)",
                    format!(
                        "Nearest station ({}) ~{:.2} est. road mi ({} stations <=5 road mi); {}.",
                        st.name, st.est_road_mi, stations_within_5_road_mi, water_note
                    ),
                )
            } else {
                (
                    4,
                    "PPC 4–5 Proxy (Station <=5 Road Mi; Confirm Hydrant <=1,000 ft)",
                    format!(
                        "Nearest station ({}) ~{:.2} est. road mi; {}.",
                        st.name, st.est_road_mi, water_note
                    ),
                )
            }
        }
    };

    json!({
        "ppcClass": ppc_class,
        "ppcBand": ppc_band,
        "basis": basis,
        "stationWithin5RoadMi": station_within_5_mi,
        "stationsWithin5RoadMiCount": stations_within_5_road_mi,
        "hydrantWithin1000Ft": hydrant_within_1000_ft,
        "municipalWaterConfirmed": municipal_water_confirmed,
        "waterSupplyStatus": water_supply_status,
        "disclaimer": ISO_PPC_PROXY_DISCLAIMER,
    })
}

/// Builds the Leaflet-ready `fire-protection` GeoJSON map layer containing fire stations and mapped hydrants.
pub fn build_fire_protection_map_layer(
    stations: &[FireStationRecord],
    hydrants: &[FireHydrantRecord],
) -> Value {
    let mut features = Vec::with_capacity(stations.len().min(8) + hydrants.len().min(16));

    for st in stations.iter().take(8) {
        let popup = format!(
            "Fire Station: {} · {:.2} mi straight (~{:.2} est. road mi) {}",
            st.name, st.distance_straight_mi, st.est_road_mi, st.direction
        );
        features.push(json!({
            "type": "Feature",
            "geometry": {
                "type": "Point",
                "coordinates": [st.lon, st.lat],
            },
            "properties": {
                "featureKind": "fire_station",
                "name": st.name,
                "department": st.department,
                "address": st.address,
                "distanceMeters": st.distance_meters,
                "distanceMi": st.distance_straight_mi,
                "estRoadMi": st.est_road_mi,
                "direction": st.direction,
                "source": st.source,
                "color": "#e11d48",
                "popup": popup,
            }
        }));
    }

    for hyd in hydrants.iter().take(16) {
        let tag = if hyd.within_1000_ft {
            "within 1,000 ft ISO threshold"
        } else {
            ">1,000 ft"
        };
        let popup = format!(
            "Fire Hydrant · {} ft ({} m, {}) {}",
            hyd.distance_ft, hyd.distance_meters, tag, hyd.direction
        );
        features.push(json!({
            "type": "Feature",
            "geometry": {
                "type": "Point",
                "coordinates": [hyd.lon, hyd.lat],
            },
            "properties": {
                "featureKind": "fire_hydrant",
                "id": hyd.id,
                "operator": hyd.operator,
                "distanceMeters": hyd.distance_meters,
                "distanceFt": hyd.distance_ft,
                "within1000Ft": hyd.within_1000_ft,
                "direction": hyd.direction,
                "source": hyd.source,
                "color": if hyd.within_1000_ft { "#0ea5e9" } else { "#f59e0b" },
                "popup": popup,
            }
        }));
    }

    json!({
        "id": "fire-protection",
        "title": "Fire Stations & Mapped Hydrants",
        "kind": "points",
        "color": "#e11d48",
        "type": "FeatureCollection",
        "features": features,
    })
}

/// Evaluates 3DEP LiDAR `canopyNearRoof` (`>=3m` tree canopy within `2m` of roof edge) for wildfire & limb overhang underwriting.
pub fn evaluate_lidar_canopy_overhang(
    house3d: Option<&Value>,
    facts: &Value,
    hazards: &Value,
    condition: Option<&Value>,
) -> Value {
    let f = unwrap_data(facts);
    let h = unwrap_data(hazards);
    let c = condition.map(unwrap_data);
    let h3d = house3d
        .map(unwrap_data)
        .or_else(|| f.get("house3d").map(unwrap_data))
        .or_else(|| h.get("house3d").map(unwrap_data))
        .or_else(|| c.and_then(|x| x.get("house3d").map(unwrap_data)));

    let metrics = h3d.and_then(|m| m.get("metrics").or(Some(m)));
    let canopy_near_roof = metrics
        .and_then(|m| m.get("canopyNearRoof"))
        .and_then(Value::as_bool)
        .or_else(|| {
            f.pointer("/lidar/canopyNearRoof")
                .and_then(Value::as_bool)
        });

    let tallest_tree_m = metrics.and_then(|m| json_f64(m.get("tallestTreeM")));
    let tallest_dist_m = metrics.and_then(|m| json_f64(m.get("tallestTreeDistM")));
    let trees_mapped = metrics
        .and_then(|m| m.get("treesMapped"))
        .and_then(Value::as_i64);

    let evaluated = canopy_near_roof.is_some();
    let flag = canopy_near_roof == Some(true);
    let trim_note = if flag {
        Some(
            "USGS 3DEP airborne LiDAR detected >=3 m vegetation canopy within 2 m (~6.5 ft) of the roof perimeter. Carriers routinely flag overhanging limbs on aerial underwriting imagery for Zone 0/1 ember ignition and wind-thrown roof impact; an ISA-certified arborist 6–10 ft roof/chimney clearance trim with dated photos clears most holds."
                .to_string(),
        )
    } else if canopy_near_roof == Some(false) {
        Some(
            "USGS 3DEP airborne LiDAR shows >=2 m horizontal clearance between mapped >=3 m tree canopy and the roof perimeter."
                .to_string(),
        )
    } else {
        None
    };

    json!({
        "evaluated": evaluated,
        "canopyNearRoof": canopy_near_roof,
        "tallestTreeM": tallest_tree_m,
        "tallestTreeDistM": tallest_dist_m,
        "treesMapped": trees_mapped,
        "underwritingFlag": flag,
        "arboristTrimCreditNote": trim_note,
    })
}

/// Evaluates roof lifecycle via `permit_roof_extractor` (`extract_roof_lifecycle`) against 15/20-year admitted-carrier underwriting cutoffs.
pub fn evaluate_roof_cliff_check(
    facts: &Value,
    condition: Option<&Value>,
    is_coastal_wind: bool,
    now_year: i32,
) -> Value {
    let f = unwrap_data(facts);
    let c = condition.map(unwrap_data);

    let yb_opt = f
        .get("yearBuilt")
        .or_else(|| f.pointer("/countyRecords/buildingRecord/yearBuilt"))
        .and_then(Value::as_i64)
        .map(|y| y as i32)
        .filter(|&y| y >= 1700 && y <= now_year + 1);

    let roof_raw = json_str(f.get("roof")).unwrap_or("");
    let roof_lc = roof_raw.to_ascii_lowercase();
    let material_class = if roof_lc.contains("wood") || roof_lc.contains("shake") {
        "wood_shake"
    } else if roof_lc.contains("tile") || roof_lc.contains("clay") || roof_lc.contains("slate") {
        "tile_or_slate"
    } else if roof_lc.contains("metal") || roof_lc.contains("standing seam") {
        "metal"
    } else if roof_lc.contains("flat")
        || roof_lc.contains("tar")
        || roof_lc.contains("foam")
        || roof_lc.contains("tpo")
    {
        "flat_membrane"
    } else if roof_lc.contains("class 4") || roof_lc.contains("impact") {
        "impact_class4"
    } else {
        "composition_shingle"
    };

    let cutoff_yrs: i32 = match material_class {
        "tile_or_slate" | "metal" => 25,
        "wood_shake" | "flat_membrane" => 15,
        _ => {
            if is_coastal_wind {
                15
            } else {
                20
            }
        }
    };

    let mut permits_vec: Vec<Value> = Vec::new();
    for arr_opt in [
        f.get("permits").and_then(Value::as_array),
        f.pointer("/buildingRegister/permits")
            .and_then(Value::as_array),
        c.and_then(|x| x.get("permits")).and_then(Value::as_array),
    ] {
        if let Some(arr) = arr_opt {
            permits_vec.extend(arr.iter().cloned());
        }
    }

    let roof_intel = if crate::extensions::ExtensionHub::global().is_enabled("permit_roof_extractor")
    {
        Some(extract_roof_lifecycle(
            &permits_vec,
            yb_opt,
            None,
            (!roof_raw.is_empty()).then_some(roof_raw),
            now_year,
        ))
    } else {
        None
    };

    let mut verified_reroof_year = f
        .get("roofYear")
        .and_then(Value::as_i64)
        .map(|y| y as i32)
        .filter(|&y| y >= yb_opt.unwrap_or(1940) && y <= now_year);

    if let Some(ref ri) = roof_intel {
        if let Some(vy) = ri.verified_reroof_year {
            verified_reroof_year = Some(verified_reroof_year.map_or(vy, |cur| cur.max(vy)));
        }
    }
    if let Some(cond_val) = c {
        let roof_sys = cond_val.pointer("/systems/roof");
        if let Some(vy) = roof_sys
            .and_then(|r| r.get("verifiedReroofYear"))
            .or_else(|| cond_val.pointer("/roofReplacement/verifiedReroofYear"))
            .and_then(Value::as_i64)
            .map(|y| y as i32)
            .filter(|&y| y >= yb_opt.unwrap_or(1940) && y <= now_year)
        {
            verified_reroof_year = Some(verified_reroof_year.map_or(vy, |cur| cur.max(vy)));
        }
    }

    let explicit_age = f
        .get("roofAgeYears")
        .and_then(Value::as_i64)
        .map(|a| a as i32)
        .filter(|&a| (0..=100).contains(&a));

    let effective_age = explicit_age
        .or_else(|| verified_reroof_year.map(|ry| (now_year - ry).max(0)))
        .or_else(|| {
            roof_intel
                .as_ref()
                .and_then(|ri| ri.new_construction_permit_year)
                .map(|ny| (now_year - ny).max(0))
        })
        .or_else(|| yb_opt.map(|yb| (now_year - yb).max(0)));

    let exceeds = effective_age.map(|a| a > cutoff_yrs).unwrap_or(false)
        || material_class == "wood_shake";
    let approaching = !exceeds
        && effective_age
            .map(|a| a >= (cutoff_yrs - 4).max(10))
            .unwrap_or(false);

    let (status, summary) = if material_class == "wood_shake" {
        (
            "exceeds_cutoff",
            "Wood shake roof material triggers admitted-carrier declination across fire and wind markets."
                .to_string(),
        )
    } else if let Some(age) = effective_age {
        if exceeds {
            (
                "exceeds_cutoff",
                format!(
                    "Estimated roof age (~{age} yrs{}) exceeds the {cutoff_yrs}-yr carrier underwriting cliff — expect ACV depreciation schedule, roof-life certification requirement, or non-renewal.",
                    verified_reroof_year
                        .map(|y| format!(", last permitted reroof {y}"))
                        .unwrap_or_else(|| ", no verified reroof permit on file".to_string())
                ),
            )
        } else if approaching {
            (
                "approaching_cutoff",
                format!(
                    "Estimated roof age (~{age} yrs) is approaching the {cutoff_yrs}-yr carrier inspection cliff; obtain a 3–5 yr roof certification during contingency."
                ),
            )
        } else {
            (
                "within_cutoff",
                format!(
                    "Roof age (~{age} yrs{}) sits within the {cutoff_yrs}-yr standard Replacement Cost Value (RCV) carrier window.",
                    verified_reroof_year
                        .map(|y| format!(", permitted reroof {y}"))
                        .unwrap_or_default()
                ),
            )
        }
    } else {
        (
            "unverified",
            "Roof installation year is absent from public parcel records; verify roof age and permit final card during inspection."
                .to_string(),
        )
    };

    json!({
        "evaluated": effective_age.is_some() || verified_reroof_year.is_some(),
        "materialClass": material_class,
        "verifiedReroofYear": verified_reroof_year,
        "hasVerifiedReroofPermit": verified_reroof_year.is_some(),
        "effectiveRoofAgeYears": effective_age,
        "carrierCutoffYears": cutoff_yrs,
        "exceedsCarrierCutoff": exceeds,
        "approachingCarrierCutoff": approaching,
        "acvOrNonRenewalRisk": exceeds || approaching,
        "status": status,
        "summary": summary,
    })
}

/// Recomputes exact parcel-to-station and parcel-to-hydrant distances on a `geohash6`-cached
/// `fire_protection_hydrants` payload and fuses subject-parcel `house3d` LiDAR `canopyNearRoof`
/// and `permit_roof_extractor` roof-cliff signals.
pub fn fuse_parcel_fire_protection(
    raw_cell_json: &Value,
    parcel_lat: Option<f64>,
    parcel_lon: Option<f64>,
    facts: &Value,
    hazards: &Value,
    environment: &Value,
    condition: Option<&Value>,
    house3d: Option<&Value>,
    is_coastal_wind: bool,
    now_year: i32,
) -> Value {
    let mut stations: Vec<FireStationRecord> = raw_cell_json
        .get("fireStations")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let mut hydrants: Vec<FireHydrantRecord> = raw_cell_json
        .get("hydrants")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    if let (Some(la), Some(lo)) = (
        parcel_lat.filter(|x| x.is_finite()),
        parcel_lon.filter(|x| x.is_finite()),
    ) {
        for st in &mut stations {
            let d = haversine_m(la, lo, st.lat, st.lon);
            st.distance_meters = d.round() as i64;
            st.distance_straight_mi = round_n(d / MILE_M, 2);
            st.est_road_mi = round_n((d / MILE_M) * ROAD_TORTUOSITY_FACTOR, 2);
            st.direction = compass8(bearing_deg(la, lo, st.lat, st.lon)).to_string();
        }
        stations.sort_by(|a, b| a.distance_meters.cmp(&b.distance_meters));

        for hy in &mut hydrants {
            let d = haversine_m(la, lo, hy.lat, hy.lon);
            hy.distance_meters = d.round() as i64;
            hy.distance_ft = (d * 3.280_84).round() as i64;
            hy.within_1000_ft = d <= ISO_HYDRANT_THRESHOLD_M;
            hy.direction = compass8(bearing_deg(la, lo, hy.lat, hy.lon)).to_string();
        }
        hydrants.sort_by(|a, b| a.distance_meters.cmp(&b.distance_meters));
    }

    let env_u = unwrap_data(environment);
    let facts_u = unwrap_data(facts);
    let muni_water = json_str(env_u.pointer("/water/primary/pwsid")).is_some()
        || json_str(env_u.pointer("/water/primary/name")).is_some()
        || raw_cell_json
            .pointer("/isoPpcProxy/municipalWaterConfirmed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let private_well = facts_u
        .get("privateWell")
        .and_then(Value::as_bool)
        .or_else(|| {
            raw_cell_json
                .get("isLikelyPrivateWell")
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    let is_rural = raw_cell_json
        .get("isRuralArea")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let iso_ppc = compute_iso_ppc_proxy(&stations, &hydrants, muni_water, private_well, is_rural);
    let lidar_canopy = evaluate_lidar_canopy_overhang(house3d, facts, hazards, condition);
    let roof_cliff = evaluate_roof_cliff_check(facts, condition, is_coastal_wind, now_year);
    let map_layer = build_fire_protection_map_layer(&stations, &hydrants);

    let hydrants_1000 = hydrants.iter().filter(|h| h.within_1000_ft).count();
    let stations_5mi = stations
        .iter()
        .filter(|s| s.est_road_mi <= ISO_MAX_PROTECTED_ROAD_MI)
        .count();

    json!({
        "status": raw_cell_json.get("status").and_then(Value::as_str).unwrap_or("live_verified"),
        "isoPpcProxy": iso_ppc,
        "nearestFireStation": stations.first(),
        "fireStationsWithin5RoadMi": stations_5mi,
        "fireStations": stations,
        "nearestHydrant": hydrants.first(),
        "hydrantsWithin1000FtCount": hydrants_1000,
        "hydrantsWithin2000FtCount": hydrants.len(),
        "hydrants": hydrants,
        "osmHydrantCoverageNote": if hydrants.is_empty() {
            Some("OpenStreetMap hydrant mapping is volunteer-contributed and incomplete in many US cities; verify physical curb hydrant within 1,000 ft during inspection.")
        } else {
            None
        },
        "lidarCanopyOverhang": lidar_canopy,
        "roofCliffCheck": roof_cliff,
        "mapLayer": map_layer,
        "sourceName": raw_cell_json.get("sourceName").and_then(Value::as_str).unwrap_or(FIRE_PROTECTION_SOURCE_NAME),
        "sourceUrl": raw_cell_json.get("sourceUrl").and_then(Value::as_str).unwrap_or(FIRE_PROTECTION_HOMEPAGE_URL),
        "checkedAt": raw_cell_json.get("checkedAt").and_then(Value::as_str).map(String::from).unwrap_or_else(now_iso),
    })
}

/// Executes live fire-station and fire-hydrant spatial queries for `(lat, lon)`.
pub async fn fetch_fire_protection_cell(
    http: &HttpClient,
    env: &EnvConfig,
    query: &ExtensionQueryProto,
    lat: f64,
    lon: f64,
) -> Result<Option<Value>> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Ok(None);
    }
    if http.is_offline_external_blocked() && !has_loopback_override(env) {
        return Ok(None);
    }

    let fema_url = env
        .get("FIRE_STATIONS_FEMA_URL")
        .unwrap_or(FEMA_RAPT_FIRE_STATIONS_URL);
    let usgs_url = env
        .get("FIRE_STATIONS_USGS_URL")
        .unwrap_or(USGS_STRUCTURES_FIRE_STATIONS_URL);
    let osm_st_url = env
        .get("FIRE_STATIONS_OSM_URL")
        .unwrap_or(ESRI_OSM_FIRE_STATIONS_URL);
    let osm_hyd_url = env
        .get("FIRE_HYDRANTS_OSM_URL")
        .unwrap_or(ESRI_OSM_HYDRANTS_URL);

    // Wave 1 concurrent fetches: FEMA RAPT Fire Stations + Esri OSM Fire Stations + Esri OSM Hydrants + Hedged Overpass Hydrants
    let fema_fut = query_arcgis_point_radius(
        http,
        fema_url,
        lat,
        lon,
        STATION_SEARCH_RADIUS_M,
        None,
        "NAME,OWNER,ADDRESS,CITY,STATE,ZIP,FDID",
        80,
        5_000,
    );
    let osm_st_fut = query_arcgis_point_radius(
        http,
        osm_st_url,
        lat,
        lon,
        STATION_SEARCH_RADIUS_M,
        Some("amenity='fire_station'"),
        "name,operator,addr_housenumber,addr_street,addr_city,addr_state,addr_postcode",
        60,
        5_000,
    );
    let osm_hyd_fut = query_arcgis_point_radius(
        http,
        osm_hyd_url,
        lat,
        lon,
        HYDRANT_SEARCH_RADIUS_M,
        Some("emergency='fire_hydrant'"),
        "OBJECTID,emergency,name,operator,water,description,osm_id2",
        60,
        4_500,
    );
    let overpass_hyd_fut = query_hedged_overpass_hydrants(http, env, lat, lon);

    let (fema_res, osm_st_res, osm_hyd_res, overpass_hyds) =
        tokio::join!(fema_fut, osm_st_fut, osm_hyd_fut, overpass_hyd_fut);

    let fema_ok = fema_res.is_ok();
    let osm_st_ok = osm_st_res.is_ok();
    let osm_hyd_ok = osm_hyd_res.is_ok();

    let fema_stations = fema_res
        .as_ref()
        .map(|v| parse_fema_stations(v, lat, lon))
        .unwrap_or_default();
    let osm_stations = osm_st_res
        .as_ref()
        .map(|v| parse_osm_arcgis_stations(v, lat, lon))
        .unwrap_or_default();

    // Federal `.gov` USGS Structures Layer 16 backup when FEMA fails or returns <2 stations
    let mut usgs_ok = false;
    let usgs_stations = if !fema_ok || fema_stations.len() < 2 {
        match query_arcgis_point_radius(
            http,
            usgs_url,
            lat,
            lon,
            STATION_SEARCH_RADIUS_M,
            None,
            "name,address,city,state,zipcode,fcode",
            80,
            5_000,
        )
        .await
        {
            Ok(v) => {
                usgs_ok = true;
                parse_usgs_stations(&v, lat, lon)
            }
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    if !fema_ok && !usgs_ok && !osm_st_ok && !osm_hyd_ok && overpass_hyds.is_empty() {
        return Err(anyhow!(
            "All fire station and hydrant upstream endpoints failed or were unreachable"
        ));
    }

    let stations = merge_dedup_stations(vec![fema_stations, usgs_stations, osm_stations]);
    let osm_poi_hyds = osm_hyd_res
        .as_ref()
        .map(|v| parse_osm_arcgis_hydrants(v, lat, lon))
        .unwrap_or_default();
    let hydrants = merge_dedup_hydrants(vec![osm_poi_hyds, overpass_hyds]);

    let muni_water = query
        .sdwis_pwsid
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
        || query
            .sdwis_water_name
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
    let private_well = query.is_likely_private_well.unwrap_or(false);
    let is_rural = query.is_rural_area.unwrap_or(false);

    let iso_ppc = compute_iso_ppc_proxy(&stations, &hydrants, muni_water, private_well, is_rural);
    let map_layer = build_fire_protection_map_layer(&stations, &hydrants);

    let now_yr = chrono::Utc::now().year();
    let canopy_flag = query
        .extra_params
        .get("canopy_near_roof")
        .and_then(|s| match s.trim() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        });
    let init_house3d = canopy_flag.map(|b| json!({ "metrics": { "canopyNearRoof": b } }));
    let init_facts = json!({
        "yearBuilt": query.year_built,
    });
    let lidar_canopy = evaluate_lidar_canopy_overhang(
        init_house3d.as_ref(),
        &init_facts,
        &Value::Null,
        None,
    );
    let roof_cliff = evaluate_roof_cliff_check(&init_facts, None, false, now_yr);

    let hydrants_1000 = hydrants.iter().filter(|h| h.within_1000_ft).count();
    let stations_5mi = stations
        .iter()
        .filter(|s| s.est_road_mi <= ISO_MAX_PROTECTED_ROAD_MI)
        .count();
    let ref_url =
        format!("https://www.openstreetmap.org/?mlat={lat:.5}&mlon={lon:.5}#map=16/{lat:.5}/{lon:.5}");

    Ok(Some(json!({
        "status": "live_verified",
        "isoPpcProxy": iso_ppc,
        "nearestFireStation": stations.first(),
        "fireStationsWithin5RoadMi": stations_5mi,
        "fireStations": stations,
        "nearestHydrant": hydrants.first(),
        "hydrantsWithin1000FtCount": hydrants_1000,
        "hydrantsWithin2000FtCount": hydrants.len(),
        "hydrants": hydrants,
        "isLikelyPrivateWell": private_well,
        "isRuralArea": is_rural,
        "osmHydrantCoverageNote": if hydrants.is_empty() {
            Some("OpenStreetMap hydrant mapping is volunteer-contributed and incomplete in many US cities; verify physical curb hydrant within 1,000 ft during inspection.")
        } else {
            None
        },
        "lidarCanopyOverhang": lidar_canopy,
        "roofCliffCheck": roof_cliff,
        "mapLayer": map_layer,
        "sourceName": FIRE_PROTECTION_SOURCE_NAME,
        "sourceUrl": ref_url,
        "checkedAt": now_iso(),
    })))
}

/// Native `DataExtensionAdapter` implementing `fire_protection_hydrants`.
pub struct FireProtectionHydrantsAdapter;

impl DataExtensionAdapter for FireProtectionHydrantsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: FIRE_PROTECTION_EXT_ID.to_string(),
            name: "FEMA RAPT / USGS Fire Stations, Hydrants & ISO PPC Insurability".to_string(),
            version: "1.0.0".to_string(),
            description: "Screens fire-suppression infrastructure and wildfire/roof insurability from FEMA RAPT / HIFLD Fire Stations, USGS National Map Structures, and OpenStreetMap fire hydrants — computing an ISO Public Protection Class (PPC 1–10) proxy, 1,000-ft hydrant and municipal water verification, 3DEP LiDAR roof-canopy overhang risk, and reroof permit carrier cliff checks.".to_string(),
            domain: CapabilityDomain::DomainInsurance as i32,
            additional_domains: vec![
                CapabilityDomain::DomainInsurance as i32,
                CapabilityDomain::DomainNaturalHazards as i32,
                CapabilityDomain::DomainLivability as i32,
            ],
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
                requests_per_second: 5,
                burst: 10,
                timeout_ms: 8_000,
                max_retries: 1,
            }),
            cache_ttl_secs: FIRE_PROTECTION_CACHE_TTL_SECS as u64,
            priority: 260,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.InsuranceQuoteProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some(FIRE_PROTECTION_HOMEPAGE_URL.to_string()),
            documentation_url: Some(FIRE_PROTECTION_DOCS_URL.to_string()),
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
                scope: ShareScope::Geohash6 as i32,
                ttl_secs: FIRE_PROTECTION_SHARE_TTL_SECS,
                stale_while_revalidate_secs: FIRE_PROTECTION_CACHE_TTL_SECS as u64,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: FIRE_PROTECTION_SOURCE_NAME.to_string(),
                url: FIRE_PROTECTION_HOMEPAGE_URL.to_string(),
                reference_url_template: "https://www.openstreetmap.org/?mlat={{lat}}&mlon={{lon}}#map=16/{{lat}}/{{lon}}".to_string(),
                schedule_or_version: Some("FEMA Critical Infrastructure Layer 12 + USGS Structures Layer 16 + OSM Hydrants".to_string()),
                notes: Some(ISO_PPC_PROXY_DISCLAIMER.to_string()),
            }),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
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
            if http.is_offline_external_blocked() && !has_loopback_override(env) {
                return Ok(Vec::new());
            }

            let started = Instant::now();
            let Some(cell_val) = fetch_fire_protection_cell(http, env, query, lat, lon).await?
            else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let checked_at = cell_val
                .get("checkedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            let src_url = cell_val
                .get("sourceUrl")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| FIRE_PROTECTION_HOMEPAGE_URL.to_string());

            Ok(vec![ExtensionEnvelopeProto {
                extension_id: FIRE_PROTECTION_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainInsurance as i32,
                priority: 260,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: FIRE_PROTECTION_EXT_ID.to_string(),
                    source_name: FIRE_PROTECTION_SOURCE_NAME.to_string(),
                    source_url: src_url,
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: checked_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: Some(ISO_PPC_PROXY_DISCLAIMER.to_string()),
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&cell_val)?)),
            }])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, routing::get, Json, Router};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn offline_and_non_us_queries_return_empty_without_inventing_fire_data() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let adapter = FireProtectionHydrantsAdapter;

        let us_q = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            state: "CA".to_string(),
            ..Default::default()
        };
        let off = adapter
            .execute_async(&us_q, &http, &env)
            .await
            .expect("offline returns empty");
        assert!(off.is_empty());

        let mut ca_q = us_q.clone();
        ca_q.extra_params
            .insert("country".to_string(), "CA".to_string());
        let non_us = adapter
            .execute_async(&ca_q, &http, &env)
            .await
            .expect("non-US returns empty");
        assert!(non_us.is_empty());
    }

    #[test]
    fn iso_ppc_proxy_distinguishes_urban_hydrant_ppc9_and_ppc10_unprotected() {
        let (lat, lon) = (37.282147, -121.961797);
        let close_st = build_station_record(
            lat,
            lon,
            37.2860,
            -121.9580,
            "Campbell Station 1".into(),
            Some("Santa Clara County Fire".into()),
            Some("123 Synth Union Ave".into()),
            Some("Campbell".into()),
            Some("CA".into()),
            Some("95008".into()),
            None,
            "fema_rapt_hifld",
        )
        .unwrap();
        let second_st = build_station_record(
            lat,
            lon,
            37.2700,
            -121.9500,
            "Winchester Station".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            "osm_na_amenities",
        )
        .unwrap();
        let third_st = build_station_record(
            lat,
            lon,
            37.2950,
            -121.9700,
            "San Jose Station 14".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            "usgs_national_map_structures",
        )
        .unwrap();
        let close_hyd = build_hydrant_record(
            lat,
            lon,
            37.2828,
            -121.9615,
            "osm:1".into(),
            None,
            Some("SJWC".into()),
            "osm_na_pois",
        )
        .unwrap();
        assert!(close_hyd.within_1000_ft);

        let urban = compute_iso_ppc_proxy(
            &[close_st.clone(), second_st, third_st],
            &[close_hyd],
            true,
            false,
            false,
        );
        assert_eq!(urban["ppcClass"].as_i64(), Some(2));
        assert_eq!(urban["hydrantWithin1000Ft"].as_bool(), Some(true));

        // Semi-protected: station <= 5 road mi, private well (no hydrant) -> PPC 9
        let ppc9 = compute_iso_ppc_proxy(&[close_st], &[], false, true, true);
        assert_eq!(ppc9["ppcClass"].as_i64(), Some(9));
        assert_eq!(ppc9["hydrantWithin1000Ft"].as_bool(), Some(false));

        // Unprotected: nearest station > 5 est. road miles -> PPC 10
        let far_st = build_station_record(
            lat,
            lon,
            37.3650,
            -121.961797,
            "Distant Rural Station".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            "fema_rapt_hifld",
        )
        .unwrap();
        assert!(far_st.est_road_mi > 5.0);
        let ppc10 = compute_iso_ppc_proxy(&[far_st], &[], false, true, true);
        assert_eq!(ppc10["ppcClass"].as_i64(), Some(10));
        assert_eq!(ppc10["stationWithin5RoadMi"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn fema_failure_falls_back_to_usgs_and_osm_and_fuses_lidar_and_roof_permits() {
        let app = Router::new()
            .route(
                "/fema_down",
                get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "upstream down") }),
            )
            .route(
                "/usgs_stations",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "name": "Sunnyoaks Fire Station",
                                "address": "485 Synth Sunnyoaks Ave",
                                "city": "Campbell",
                                "state": "CA",
                                "zipcode": "95008",
                                "fcode": 74026
                            },
                            "geometry": { "x": -121.9600, "y": 37.2750 }
                        }]
                    }))
                }),
            )
            .route(
                "/osm_stations",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "name": "Winchester Fire Station",
                                "operator": "Santa Clara County Fire Department",
                                "addr_housenumber": "14850",
                                "addr_street": "Winchester Boulevard",
                                "addr_city": "Los Gatos",
                                "addr_state": "CA",
                                "addr_postcode": "95030"
                            },
                            "geometry": { "x": -121.9650, "y": 37.2690 }
                        }]
                    }))
                }),
            )
            .route(
                "/osm_hydrants",
                get(|| async {
                    // Web Mercator coordinates for (-121.9615, 37.2825) to verify EPSG:3857 conversion too!
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "OBJECTID": 101,
                                "emergency": "fire_hydrant",
                                "operator": "San Jose Water",
                                "osm_id2": "998877"
                            },
                            "geometry": { "x": -13576689.5, "y": 4478556.0 }
                        }]
                    }))
                }),
            )
            .route(
                "/overpass",
                get(|| async {
                    Json(json!({
                        "elements": [{
                            "type": "node",
                            "id": 777111,
                            "lat": 37.2824,
                            "lon": -121.9616,
                            "tags": { "emergency": "fire_hydrant", "ref": "H-42" }
                        }]
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let env = EnvConfig::from_map(HashMap::from([
            (
                "FIRE_STATIONS_FEMA_URL".to_string(),
                format!("http://{addr}/fema_down"),
            ),
            (
                "FIRE_STATIONS_USGS_URL".to_string(),
                format!("http://{addr}/usgs_stations"),
            ),
            (
                "FIRE_STATIONS_OSM_URL".to_string(),
                format!("http://{addr}/osm_stations"),
            ),
            (
                "FIRE_HYDRANTS_OSM_URL".to_string(),
                format!("http://{addr}/osm_hydrants"),
            ),
            (
                "OVERPASS_ENDPOINTS".to_string(),
                format!("http://{addr}/overpass"),
            ),
        ]));
        let http = HttpClient::offline_loopback_only(None);
        let adapter = FireProtectionHydrantsAdapter;

        let q = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            state: "CA".to_string(),
            sdwis_pwsid: Some("CA4310011".to_string()),
            ..Default::default()
        };
        let envs = adapter
            .execute_async(&q, &http, &env)
            .await
            .expect("USGS + OSM fallback succeeds");
        assert_eq!(envs.len(), 1);
        let Payload::RawJson(ref raw) = envs[0].payload.as_ref().unwrap() else {
            panic!("expected RawJson");
        };
        let cell: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(cell["fireStations"].as_array().unwrap().len(), 2);
        assert!(cell["hydrantsWithin1000FtCount"].as_u64().unwrap() >= 1);
        assert_eq!(cell["mapLayer"]["id"].as_str(), Some("fire-protection"));
        assert!(cell["mapLayer"]["features"].as_array().unwrap().len() >= 3);

        // Fuse parcel with LiDAR canopy overhang + Solar PV permit (must NOT fake reroof) on 1998 roof
        let facts_aged = json!({
            "yearBuilt": 1998,
            "roof": "Composition shingle",
            "permits": [{
                "category": "solar",
                "description": "Roof Mounted PV 6.4 kW",
                "year": 2024,
                "status": "Finaled",
                "finaled": true,
                "verified": true
            }]
        });
        let house3d = json!({
            "metrics": {
                "canopyNearRoof": true,
                "tallestTreeM": 14.6,
                "tallestTreeDistM": 1.4,
                "treesMapped": 5
            }
        });
        let fused = fuse_parcel_fire_protection(
            &cell,
            Some(37.282147),
            Some(-121.961797),
            &facts_aged,
            &Value::Null,
            &json!({"water": {"primary": {"pwsid": "CA4310011"}}}),
            None,
            Some(&house3d),
            false,
            2026,
        );
        assert_eq!(
            fused["lidarCanopyOverhang"]["underwritingFlag"].as_bool(),
            Some(true)
        );
        assert_eq!(
            fused["roofCliffCheck"]["exceedsCarrierCutoff"].as_bool(),
            Some(true)
        );
        assert_eq!(
            fused["roofCliffCheck"]["hasVerifiedReroofPermit"].as_bool(),
            Some(false)
        );

        // Now fuse with verified 2023 finaled reroof permit -> clears roof cliff!
        let facts_reroofed = json!({
            "yearBuilt": 1998,
            "roof": "Composition shingle",
            "permits": [{
                "category": "reroof",
                "description": "Tear off and reroof comp shingle",
                "year": 2023,
                "status": "Finaled",
                "finaled": true,
                "verified": true,
                "sourceVerified": true
            }]
        });
        let fused_ok = fuse_parcel_fire_protection(
            &cell,
            Some(37.282147),
            Some(-121.961797),
            &facts_reroofed,
            &Value::Null,
            &Value::Null,
            None,
            None,
            false,
            2026,
        );
        assert_eq!(
            fused_ok["roofCliffCheck"]["verifiedReroofYear"].as_i64(),
            Some(2023)
        );
        assert_eq!(
            fused_ok["roofCliffCheck"]["exceedsCarrierCutoff"].as_bool(),
            Some(false)
        );
        assert_eq!(
            fused_ok["roofCliffCheck"]["status"].as_str(),
            Some("within_cutoff")
        );

        srv.abort();
    }
}
