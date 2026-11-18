//! Official Open Feed Noise & Route Extensions (`faa_flight_noise`, `highway_traffic_noise`,
//! `rail_transit_noise`, `military_airspace_noise`).
//!
//! Queries 100% keyless US federal open-data endpoints (FAA AIS, USDOT BTS/FHWA HPMS, FRA Grade
//! Crossing Inventory & NARN, FTA National Transit Map, Caltrans AADT, and live ADS-B) to model
//! source-specific acoustic metrics (`Ldn`, peak `Lmax`, peak-hour `Leq`), schedule windows,
//! Quiet Zone horn suppression, and GeoJSON corridor/crossing map layers.
//! Never fabricates noise metrics when offline or when upstream services are unreachable.

use anyhow::Result;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use url::Url;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::manifest::parse_and_validate_package;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind, ExecutionMode,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    GeoScopeProto, LicenseClass, ManifestSourceAttributionProto, MeteringSpecProto, ProvenanceTier,
    RateLimitSpecProto, ShareScope, SharingSpecProto, SourceAttributionProto,
};
use crate::shared::{
    arcgis_representative_point, arcgis_to_geojson_geometry, bearing_deg, compass16, haversine_m,
    haversine_mi, nearest_on_polyline, now_iso, point_in_ring, round_n, EnvConfig, HttpClient,
    FOOT_M, MILE_M,
};

pub const FAA_FLIGHT_EXT_ID: &str = "faa_flight_noise";
pub const HIGHWAY_TRAFFIC_EXT_ID: &str = "highway_traffic_noise";
pub const RAIL_TRANSIT_EXT_ID: &str = "rail_transit_noise";
pub const MILITARY_AIRSPACE_EXT_ID: &str = "military_airspace_noise";

pub const FAA_FLIGHT_SOURCE_NAME: &str =
    "FAA Aeronautical Information Services (AIS), USDOT BTS Runway Ends & Live ADS-B";
pub const HIGHWAY_TRAFFIC_SOURCE_NAME: &str =
    "USDOT FHWA HPMS Traffic Volumes & BTS National Transportation Noise Map";
pub const RAIL_TRANSIT_SOURCE_NAME: &str =
    "USDOT FRA Grade Crossing Inventory, North American Rail Network & National Transit Map";
pub const MILITARY_AIRSPACE_SOURCE_NAME: &str =
    "FAA Special Use Airspace (SUA), Military Training Routes (MTR) & NTAD Military Bases";

pub const BTS_NOISE_MAP_REF_URL: &str = "https://maps.dot.gov/BTS/NationalTransportationNoiseMap/";
pub const BTS_TRANSIT_MAP_REF_URL: &str = "https://www.bts.gov/national-transit-map";
pub const FAA_AERO_DATA_REF_URL: &str =
    "https://www.faa.gov/air_traffic/flight_info/aeronav/aero_data/";
pub const FHWA_HPMS_REF_URL: &str = "https://www.fhwa.dot.gov/policyinformation/hpms.cfm";

const FAA_AIS_SERVICES: &str = "https://services6.arcgis.com/ssFJjBXIUyZDrSYZ/arcgis/rest/services";
const BTS_NTAD_SERVICES: &str = "https://services.arcgis.com/xOi1kZaI0eWDREZv/arcgis/rest/services";
const BTS_TILES_SERVICES: &str =
    "https://tiles.arcgis.com/tiles/xOi1kZaI0eWDREZv/arcgis/rest/services";
const USDOT_GEO_HOSTED: &str = "https://geo.dot.gov/server/rest/services/Hosted";
const CALTRANS_AADT_LAYER: &str =
    "https://caltrans-gis.dot.ca.gov/arcgis/rest/services/CHhighway/Traffic_AADT/FeatureServer/0";
const ADSB_LOL_BASE: &str = "https://api.adsb.lol";
const OPENSKY_BASE: &str = "https://opensky-network.org";

const REQ_TIMEOUT_MS: u64 = 8_500;
const ADSB_TIMEOUT_MS: u64 = 5_000;
const NM_M: f64 = 1_852.0;
const EARTH_RADIUS_M: f64 = 6_371_008.8;

const FAA_MANIFEST_YAML: &str = include_str!("../../extensions/faa_flight_noise/manifest.yaml");
const HIGHWAY_MANIFEST_YAML: &str =
    include_str!("../../extensions/highway_traffic_noise/manifest.yaml");
const RAIL_MANIFEST_YAML: &str = include_str!("../../extensions/rail_transit_noise/manifest.yaml");
const MILITARY_MANIFEST_YAML: &str =
    include_str!("../../extensions/military_airspace_noise/manifest.yaml");

/// Returns all 4 built-in noise & route native adapters for registration on [`ExtensionHub`].
pub fn all_noise_feed_adapters() -> Vec<Arc<dyn DataExtensionAdapter>> {
    vec![
        Arc::new(FaaFlightNoiseAdapter),
        Arc::new(HighwayTrafficNoiseAdapter),
        Arc::new(RailTransitNoiseAdapter),
        Arc::new(MilitaryAirspaceNoiseAdapter),
    ]
}

fn build_manifest_from_embedded(
    yaml: &str,
    fallback_id: &str,
    fallback_name: &str,
    fallback_desc: &str,
    priority: i32,
    ttl_secs: u64,
    scope: ShareScope,
    swr_secs: u64,
    homepage_url: &str,
    docs_url: &str,
    source_name: &str,
    source_url: &str,
    ref_url: &str,
) -> ExtensionManifestProto {
    if let Ok(pkg) = parse_and_validate_package(yaml, None, Some(true)) {
        return pkg.to_manifest_proto(pkg.doc.enabled_by_default, pkg.doc.priority);
    }
    ExtensionManifestProto {
        extension_id: fallback_id.to_string(),
        name: fallback_name.to_string(),
        version: "1.0.0".to_string(),
        description: fallback_desc.to_string(),
        domain: CapabilityDomain::DomainEnvironmental as i32,
        additional_domains: vec![
            CapabilityDomain::DomainEnvironmental as i32,
            CapabilityDomain::DomainLivability as i32,
        ],
        driver: DriverKind::DriverNativeAdapter as i32,
        geo_scope: Some(GeoScopeProto {
            country: "US".to_string(),
            nationwide: true,
            ..Default::default()
        }),
        auth: Some(AuthSpecProto {
            auth_type: "none".to_string(),
            optional_key: false,
            ..Default::default()
        }),
        rate_limit: Some(RateLimitSpecProto {
            requests_per_second: 4,
            burst: 8,
            timeout_ms: REQ_TIMEOUT_MS as u32,
            max_retries: 1,
        }),
        cache_ttl_secs: ttl_secs,
        priority,
        enabled: true,
        builtin: true,
        tier: ExtensionTier::Basic as i32,
        license_class: LicenseClass::PublicOpenData as i32,
        execution_mode: ExecutionMode::Eager as i32,
        enabled_by_default: Some(true),
        proto_message_type: "openestate.extension.v1.EnvironmentalSummaryProto".to_string(),
        homepage_url: Some(homepage_url.to_string()),
        documentation_url: Some(docs_url.to_string()),
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
            scope: scope as i32,
            ttl_secs,
            stale_while_revalidate_secs: swr_secs,
            background_refresh: false,
            key_fields: Vec::new(),
        }),
        source_attribution: Some(ManifestSourceAttributionProto {
            name: source_name.to_string(),
            url: source_url.to_string(),
            reference_url_template: ref_url.to_string(),
            schedule_or_version: None,
            notes: None,
        }),
        ..Default::default()
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

fn valid_us_coords(lat: Option<f64>, lon: Option<f64>, query: &ExtensionQueryProto) -> Option<(f64, f64)> {
    let country = query
        .extra_params
        .get("country")
        .map(String::as_str)
        .unwrap_or("");
    if !is_us_country_opt(country) {
        return None;
    }
    let (la, lo) = (lat?, lon?);
    if !la.is_finite() || !lo.is_finite() || la.abs() > 90.0 || lo.abs() > 180.0 {
        return None;
    }
    Some((la, lo))
}

fn clean_str(v: Option<&Value>) -> Option<String> {
    let raw = match v? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

fn val_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn get_ci<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    let m = obj.as_object()?;
    if let Some(v) = m.get(key) {
        return Some(v);
    }
    let low = key.to_ascii_lowercase();
    if let Some(v) = m.get(&low) {
        return Some(v);
    }
    let up = key.to_ascii_uppercase();
    if let Some(v) = m.get(&up) {
        return Some(v);
    }
    m.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

fn sanitize_alnum_code(raw: &str) -> Option<String> {
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(8)
        .collect();
    if s.is_empty() {
        None
    } else {
        Some(s.to_ascii_uppercase())
    }
}

fn bts_region_for_state(state: &str) -> &'static str {
    match state.trim().to_ascii_uppercase().as_str() {
        "AK" => "Alaska",
        "HI" => "Hawaii",
        _ => "CONUS",
    }
}

/// Builds USDOT BTS 2022 National Transportation Noise Map `L.tileLayer` metadata for a mode
/// (`"aviation"`, `"road"`, `"rail"`, or `"combined"`).
pub fn bts_noise_tile_meta(state: &str, mode: &str) -> Option<Value> {
    let region = bts_region_for_state(state);
    if region == "Hawaii" && mode == "rail" {
        return None;
    }
    let suffix = match mode {
        "aviation" => "aviation",
        "road" => "road",
        "rail" => "rail",
        _ => {
            if region == "Hawaii" {
                "aviation_road"
            } else {
                "aviation_rail_road"
            }
        }
    };
    let svc = format!("NTAD_Noise_2022_{region}_{suffix}");
    Some(json!({
        "service": svc,
        "mode": mode,
        "region": region,
        "tileUrl": format!("{BTS_TILES_SERVICES}/{svc}/MapServer/tile/{{z}}/{{y}}/{{x}}"),
        "referenceUrl": BTS_NOISE_MAP_REF_URL,
        "attribution": "US DOT/BTS National Transportation Noise Map 2022",
        "maxNativeZoom": 12,
        "maxZoom": 19,
        "opacity": 0.44,
    }))
}

/// Forward geodesic destination point from `(lat_deg, lon_deg)` along `bearing_deg` by `dist_m`.
pub fn dest_point_latlon(lat_deg: f64, lon_deg: f64, bearing_deg_true: f64, dist_m: f64) -> (f64, f64) {
    let delta = dist_m / EARTH_RADIUS_M;
    let theta = bearing_deg_true.to_radians();
    let phi1 = lat_deg.to_radians();
    let lam1 = lon_deg.to_radians();
    let sin_phi2 = phi1.sin() * delta.cos() + phi1.cos() * delta.sin() * theta.cos();
    let phi2 = sin_phi2.clamp(-1.0, 1.0).asin();
    let y = theta.sin() * delta.sin() * phi1.cos();
    let x = delta.cos() - phi1.sin() * phi2.sin();
    let lam2 = lam1 + y.atan2(x);
    let lon_out = ((lam2.to_degrees() + 540.0).rem_euclid(360.0)) - 180.0;
    (round_n(phi2.to_degrees(), 6), round_n(lon_out, 6))
}

/// Computes `(along_track_m, abs_cross_track_m)` of `(home_lat, home_lon)` relative to a ray
/// originating at `(origin_lat, origin_lon)` pointing along `ray_bearing_deg` (° true).
/// Positive `along_track_m` means the property lies outbound along the ray.
pub fn ray_along_and_cross_m(
    origin_lat: f64,
    origin_lon: f64,
    ray_bearing_deg: f64,
    home_lat: f64,
    home_lon: f64,
) -> (f64, f64) {
    let d_m = haversine_m(origin_lat, origin_lon, home_lat, home_lon);
    if d_m < 0.5 {
        return (0.0, 0.0);
    }
    let brg_to_home = bearing_deg(origin_lat, origin_lon, home_lat, home_lon);
    let diff_rad = (brg_to_home - ray_bearing_deg).to_radians();
    let along = d_m * diff_rad.cos();
    let cross = (d_m * diff_rad.sin()).abs();
    (along, cross)
}

/// Logarithmic decibel power summation: $10 \log_{10}(\sum_i 10^{L_i / 10})$.
pub fn energetic_sum_dba(levels: &[f64]) -> Option<f64> {
    let mut sum = 0.0_f64;
    let mut count = 0usize;
    for &lv in levels {
        if lv.is_finite() && lv > 0.0 {
            sum += 10.0_f64.powf(lv / 10.0);
            count += 1;
        }
    }
    if count == 0 || sum <= 0.0 {
        None
    } else {
        Some(round_n(10.0 * sum.log10(), 1))
    }
}

fn polyline_distance_m(lat: f64, lon: f64, arcgis_geom: &Value) -> Option<f64> {
    let paths = arcgis_geom.get("paths")?.as_array()?;
    let mut best = f64::INFINITY;
    for path in paths {
        let Some(pts) = path.as_array() else { continue };
        let coords: Vec<(f64, f64)> = pts
            .iter()
            .filter_map(|p| Some((p.get(1)?.as_f64()?, p.get(0)?.as_f64()?)))
            .collect();
        if let Some(hit) = nearest_on_polyline(lat, lon, &coords) {
            if hit.dist_m < best {
                best = hit.dist_m;
            }
        }
    }
    best.is_finite().then_some(best)
}

fn polygon_distance_and_inside(lat: f64, lon: f64, arcgis_geom: &Value) -> Option<(f64, bool)> {
    let rings = arcgis_geom.get("rings")?.as_array()?;
    if rings.is_empty() {
        return None;
    }
    let mut inside_cnt = 0usize;
    let mut best = f64::INFINITY;
    for ring in rings {
        let Some(pts) = ring.as_array() else { continue };
        let coords: Vec<(f64, f64)> = pts
            .iter()
            .filter_map(|p| Some((p.get(1)?.as_f64()?, p.get(0)?.as_f64()?)))
            .collect();
        if coords.len() < 3 {
            continue;
        }
        if point_in_ring(lat, lon, &coords) {
            inside_cnt += 1;
        }
        if let Some(hit) = nearest_on_polyline(lat, lon, &coords) {
            if hit.dist_m < best {
                best = hit.dist_m;
            }
        }
    }
    if !best.is_finite() {
        return None;
    }
    let inside = inside_cnt % 2 == 1;
    Some((if inside { 0.0 } else { best }, inside))
}

fn simplified_arcgis_polygon_geojson(arcgis_geom: &Value, max_pts_per_ring: usize) -> Option<Value> {
    let rings = arcgis_geom.get("rings")?.as_array()?;
    let mut out_rings = Vec::new();
    for ring in rings.iter().take(6) {
        let Some(pts) = ring.as_array() else { continue };
        if pts.len() < 4 {
            continue;
        }
        let step = (pts.len() / max_pts_per_ring.max(8)).max(1);
        let mut coords: Vec<Value> = pts
            .iter()
            .step_by(step)
            .filter_map(|p| {
                let x = round_n(p.get(0)?.as_f64()?, 5);
                let y = round_n(p.get(1)?.as_f64()?, 5);
                Some(json!([x, y]))
            })
            .collect();
        if coords.len() < 3 {
            continue;
        }
        if let Some(first) = coords.first().cloned() {
            if coords.last() != Some(&first) {
                coords.push(first);
            }
        }
        if coords.len() >= 4 {
            out_rings.push(Value::Array(coords));
        }
    }
    if out_rings.is_empty() {
        None
    } else {
        Some(json!({
            "type": "Polygon",
            "coordinates": out_rings,
        }))
    }
}

fn fmt_mi(mi: f64) -> String {
    if mi < 0.1 {
        format!("{} ft", (mi * 5280.0).round() as i64)
    } else if mi < 10.0 {
        format!("{:.2} mi", mi)
    } else {
        format!("{:.1} mi", mi)
    }
}

// ============================================================================
// 1. FAA Flight Paths, Runways & Live Aviation Noise (`faa_flight_noise`)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FlightNoiseEndpoints {
    pub airports_layer: String,
    pub runway_ends_layer: String,
    pub class_airspace_layer: String,
    pub adsb_lol_base: String,
    pub opensky_base: String,
}

impl Default for FlightNoiseEndpoints {
    fn default() -> Self {
        Self {
            airports_layer: format!("{FAA_AIS_SERVICES}/US_Airport/FeatureServer/0"),
            runway_ends_layer: format!("{BTS_NTAD_SERVICES}/Runway_Ends_Table/FeatureServer/1"),
            class_airspace_layer: format!("{FAA_AIS_SERVICES}/Class_Airspace/FeatureServer/0"),
            adsb_lol_base: ADSB_LOL_BASE.to_string(),
            opensky_base: OPENSKY_BASE.to_string(),
        }
    }
}

/// FAA AEDT screening model for a single extended runway approach/departure corridor.
/// Returns `(est_ldn_dba, peak_flyover_lmax_dba, glidepath_alt_agl_ft, slant_dist_ft)`.
pub fn model_faa_aedt_runway_corridor(
    along_track_mi: f64,
    cross_track_mi: f64,
    glidepath_deg: f64,
    tch_ft: f64,
    sel_1000_dba: f64,
    day_ops: f64,
    night_ops: f64,
) -> Option<(f64, f64, i64, i64)> {
    if !along_track_mi.is_finite()
        || !cross_track_mi.is_finite()
        || !(-1.5..=14.0).contains(&along_track_mi)
        || cross_track_mi > 6.5
    {
        return None;
    }
    let eff_along_ft = (along_track_mi.max(0.15)) * 5280.0;
    let cross_ft = cross_track_mi.abs() * 5280.0;
    let gpa = glidepath_deg.clamp(2.5, 6.5).to_radians();
    let h_agl_ft = eff_along_ft * gpa.tan() + tch_ft.clamp(20.0, 120.0);
    let slant_ft = (cross_ft * cross_ft + h_agl_ft * h_agl_ft).sqrt().max(250.0);
    let beta_deg = h_agl_ft.atan2(cross_ft.max(1.0)).to_degrees();
    let lateral_atten = if beta_deg < 35.0 {
        10.0 * (1.0 - beta_deg / 35.0)
    } else {
        0.0
    };
    let sel = sel_1000_dba
        - 20.0 * (slant_ft / 1000.0).log10()
        - 0.0015 * (slant_ft - 1000.0).max(0.0)
        - lateral_atten;
    let weighted_ops = (day_ops.max(0.0) + 10.0 * night_ops.max(0.0)).max(0.5);
    let ldn = (sel + 10.0 * weighted_ops.log10() - 49.4).clamp(25.0, 85.0);
    let lmax = (sel - 10.5).clamp(32.0, 98.0);
    Some((
        round_n(ldn, 1),
        round_n(lmax, 1),
        h_agl_ft.round() as i64,
        slant_ft.round() as i64,
    ))
}

#[derive(Debug, Clone)]
struct ParsedAircraft {
    hex: String,
    callsign: Option<String>,
    aircraft_type: Option<String>,
    alt_baro_ft: Option<i64>,
    ground_speed_kt: Option<i64>,
    heading_deg: Option<f64>,
    vert_rate_fpm: Option<i64>,
    lat: f64,
    lon: f64,
    dist_nm: f64,
}

fn parse_adsb_lol_aircraft(val: &Value, home_lat: f64, home_lon: f64) -> Vec<ParsedAircraft> {
    let Some(arr) = val.get("ac").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr.iter().take(40) {
        let (Some(la), Some(lo)) = (val_f64(item.get("lat")), val_f64(item.get("lon"))) else {
            continue;
        };
        if let Some(s) = item.get("alt_baro").and_then(Value::as_str) {
            if s.eq_ignore_ascii_case("ground") {
                continue;
            }
        }
        let alt_ft = val_f64(item.get("alt_baro")).map(|a| a.round() as i64);
        if let Some(a) = alt_ft {
            if a <= 0 || a > 45_000 {
                continue;
            }
        }
        let dist_nm = val_f64(item.get("dst"))
            .unwrap_or_else(|| haversine_m(home_lat, home_lon, la, lo) / NM_M);
        let hex = clean_str(item.get("hex")).unwrap_or_else(|| "aircraft".to_string());
        let callsign = clean_str(item.get("flight")).or_else(|| clean_str(item.get("r")));
        out.push(ParsedAircraft {
            hex,
            callsign,
            aircraft_type: clean_str(item.get("t")),
            alt_baro_ft: alt_ft,
            ground_speed_kt: val_f64(item.get("gs")).map(|v| v.round() as i64),
            heading_deg: val_f64(item.get("track")),
            vert_rate_fpm: val_f64(item.get("baro_rate")).map(|v| v.round() as i64),
            lat: round_n(la, 5),
            lon: round_n(lo, 5),
            dist_nm: round_n(dist_nm, 2),
        });
    }
    out.sort_by(|a, b| {
        a.dist_nm
            .partial_cmp(&b.dist_nm)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn parse_opensky_aircraft(val: &Value, home_lat: f64, home_lon: f64) -> Vec<ParsedAircraft> {
    let Some(states) = val.get("states").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for st in states.iter().take(40) {
        let Some(tuple) = st.as_array() else { continue };
        let on_ground = tuple.get(8).and_then(Value::as_bool).unwrap_or(false);
        if on_ground {
            continue;
        }
        let (Some(lo), Some(la)) = (
            val_f64(tuple.get(5)),
            val_f64(tuple.get(6)),
        ) else {
            continue;
        };
        let alt_ft = val_f64(tuple.get(7)).map(|m| (m * 3.28084).round() as i64);
        if let Some(a) = alt_ft {
            if a <= 0 || a > 45_000 {
                continue;
            }
        }
        let hex = clean_str(tuple.first()).unwrap_or_else(|| "aircraft".to_string());
        let callsign = clean_str(tuple.get(1));
        let gs_kt = val_f64(tuple.get(9)).map(|ms| (ms * 1.94384).round() as i64);
        let track = val_f64(tuple.get(10));
        let vr_fpm = val_f64(tuple.get(11)).map(|ms| (ms * 196.85).round() as i64);
        let dist_nm = haversine_m(home_lat, home_lon, la, lo) / NM_M;
        out.push(ParsedAircraft {
            hex,
            callsign,
            aircraft_type: None,
            alt_baro_ft: alt_ft,
            ground_speed_kt: gs_kt,
            heading_deg: track,
            vert_rate_fpm: vr_fpm,
            lat: round_n(la, 5),
            lon: round_n(lo, 5),
            dist_nm: round_n(dist_nm, 2),
        });
    }
    out.sort_by(|a, b| {
        a.dist_nm
            .partial_cmp(&b.dist_nm)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

async fn fetch_live_adsb_snapshot(
    http: &HttpClient,
    eps: &FlightNoiseEndpoints,
    lat: f64,
    lon: f64,
) -> Option<(Vec<ParsedAircraft>, &'static str)> {
    let primary_url = format!(
        "{}/v2/point/{lat:.5}/{lon:.5}/15",
        eps.adsb_lol_base.trim_end_matches('/')
    );
    if let Ok(j) = http
        .get_json_opts(&primary_url, ADSB_TIMEOUT_MS, 0, 120, None)
        .await
    {
        return Some((parse_adsb_lol_aircraft(&j, lat, lon), "adsb.lol"));
    }
    let fb_url = format!(
        "{}/api/states/all?lamin={:.4}&lomin={:.4}&lamax={:.4}&lomax={:.4}",
        eps.opensky_base.trim_end_matches('/'),
        lat - 0.22,
        lon - 0.28,
        lat + 0.22,
        lon + 0.28
    );
    if let Ok(j) = http
        .get_json_opts(&fb_url, ADSB_TIMEOUT_MS, 0, 120, None)
        .await
    {
        return Some((parse_opensky_aircraft(&j, lat, lon), "opensky-network.org"));
    }
    None
}

pub async fn fetch_faa_flight_noise_with_endpoints(
    http: &HttpClient,
    eps: &FlightNoiseEndpoints,
    lat: f64,
    lon: f64,
    state: &str,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }

    let airports_fut = http.arcgis_radius_query(
        &eps.airports_layer,
        lat,
        lon,
        25_000.0,
        "IDENT,ICAO_ID,NAME,TYPE_CODE,ELEVATION,OPERSTATUS,SERVCITY,STATE,IAPEXISTS,MIL_CODE",
        &[],
        REQ_TIMEOUT_MS,
    );
    let airspace_fut = http.arcgis_point_query(
        &eps.class_airspace_layer,
        lat,
        lon,
        "IDENT,NAME,CLASS,LOCAL_TYPE,LOWER_VAL,LOWER_UOM,LOWER_CODE,UPPER_VAL,UPPER_UOM,UPPER_CODE",
        &[],
        REQ_TIMEOUT_MS,
    );
    let adsb_fut = fetch_live_adsb_snapshot(http, eps, lat, lon);

    let (airports_res, airspace_res, adsb_opt) = tokio::join!(airports_fut, airspace_fut, adsb_fut);

    let airports_ok = airports_res.is_ok();
    let airspace_ok = airspace_res.is_ok();
    if !airports_ok && !airspace_ok && adsb_opt.is_none() {
        return Ok(None);
    }

    #[derive(Clone)]
    struct AirportInfo {
        ident: String,
        icao: Option<String>,
        name: String,
        type_code: String,
        elev_ft: Option<f64>,
        iap_exists: bool,
        mil_code: Option<String>,
        lat: f64,
        lon: f64,
        dist_mi: f64,
        direction: String,
    }

    let mut airports: Vec<AirportInfo> = Vec::new();
    if let Ok(feats) = airports_res {
        let mut seen = HashSet::new();
        for f in &feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let Some(ident) = clean_str(get_ci(a, "IDENT")).and_then(|s| sanitize_alnum_code(&s))
            else {
                continue;
            };
            if !seen.insert(ident.clone()) {
                continue;
            }
            let Some((la, lo)) = f.get("geometry").and_then(arcgis_representative_point) else {
                continue;
            };
            let dist_mi = round_n(haversine_mi(lat, lon, la, lo), 2);
            let dir = compass16(bearing_deg(lat, lon, la, lo)).to_string();
            let iap = val_f64(get_ci(a, "IAPEXISTS"))
                .map(|v| v >= 1.0)
                .or_else(|| {
                    clean_str(get_ci(a, "IAPEXISTS"))
                        .map(|s| matches!(s.to_ascii_uppercase().as_str(), "Y" | "YES" | "TRUE" | "1"))
                })
                .unwrap_or(false);
            airports.push(AirportInfo {
                ident: ident.clone(),
                icao: clean_str(get_ci(a, "ICAO_ID")),
                name: clean_str(get_ci(a, "NAME")).unwrap_or_else(|| ident.clone()),
                type_code: clean_str(get_ci(a, "TYPE_CODE")).unwrap_or_else(|| "AD".to_string()),
                elev_ft: val_f64(get_ci(a, "ELEVATION")),
                iap_exists: iap,
                mil_code: clean_str(get_ci(a, "MIL_CODE")),
                lat: round_n(la, 5),
                lon: round_n(lo, 5),
                dist_mi,
                direction: dir,
            });
        }
    }
    airports.sort_by(|a, b| {
        a.dist_mi
            .partial_cmp(&b.dist_mi)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Airspace shelves at property
    let mut airspace_shelves = Vec::new();
    let mut class_b_or_c_idents = HashSet::new();
    if let Ok(rows) = airspace_res {
        let mut seen = HashSet::new();
        for a in rows {
            let name = clean_str(get_ci(&a, "NAME")).unwrap_or_default();
            let cls = clean_str(get_ci(&a, "CLASS")).unwrap_or_default();
            let ltype = clean_str(get_ci(&a, "LOCAL_TYPE")).unwrap_or_default();
            let ident = clean_str(get_ci(&a, "IDENT")).unwrap_or_default();
            if matches!(cls.as_str(), "B" | "C") && !ident.is_empty() {
                class_b_or_c_idents.insert(ident.to_ascii_uppercase());
            }
            let lower = clean_str(get_ci(&a, "LOWER_VAL")).unwrap_or_else(|| "SFC".to_string());
            let upper = clean_str(get_ci(&a, "UPPER_VAL")).unwrap_or_default();
            let key = format!("{name}|{cls}|{lower}|{upper}");
            if name.is_empty() || !seen.insert(key) {
                continue;
            }
            airspace_shelves.push(json!({
                "ident": if ident.is_empty() { Value::Null } else { json!(ident) },
                "name": name,
                "airspaceClass": if cls.is_empty() { ltype } else { format!("Class {cls}") },
                "floorFtMsl": lower,
                "ceilingFtMsl": upper,
            }));
        }
    }

    // Query BTS Runway_Ends_Table for non-heliport airports within 25 km
    let rwy_airport_ids: Vec<String> = airports
        .iter()
        .filter(|ap| ap.type_code != "HP")
        .take(10)
        .map(|ap| ap.ident.clone())
        .collect();

    let airport_map: HashMap<String, AirportInfo> = airports
        .iter()
        .map(|ap| (ap.ident.clone(), ap.clone()))
        .collect();

    let mut corridors = Vec::new();
    let mut corridor_ldns = Vec::new();
    let mut peak_lmax: Option<f64> = None;
    let mut map_features = Vec::new();

    if !rwy_airport_ids.is_empty() {
        let in_clause = rwy_airport_ids
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(",");
        let params = vec![
            ("where", format!("ARPT_ID IN ({in_clause})")),
            (
                "outFields",
                "ARPT_ID,RWY_ID,RWY_END_ID,TRUE_ALIGNMENT,ILS_TYPE,VISUAL_GLIDE_PATH_ANGLE,THR_CROSSING_HGT,RWY_END_ELEV,LAT_DECIMAL,LONG_DECIMAL".to_string(),
            ),
            ("returnGeometry", "false".to_string()),
        ];
        if let Ok(rwy_json) = http
            .arcgis_query(&eps.runway_ends_layer, &params, REQ_TIMEOUT_MS)
            .await
        {
            if let Some(feats) = rwy_json.get("features").and_then(Value::as_array) {
                for f in feats {
                    let a = f.get("attributes").unwrap_or(&Value::Null);
                    let Some(arpt_id) =
                        clean_str(get_ci(a, "ARPT_ID")).and_then(|s| sanitize_alnum_code(&s))
                    else {
                        continue;
                    };
                    let Some(rwy_end_id) = clean_str(get_ci(a, "RWY_END_ID")) else {
                        continue;
                    };
                    let (Some(r_lat), Some(r_lon), Some(hdg)) = (
                        val_f64(get_ci(a, "LAT_DECIMAL")),
                        val_f64(get_ci(a, "LONG_DECIMAL")),
                        val_f64(get_ci(a, "TRUE_ALIGNMENT")),
                    ) else {
                        continue;
                    };
                    if !(-90.0..=90.0).contains(&r_lat) || !(-180.0..=180.0).contains(&r_lon) {
                        continue;
                    }
                    let outbound_brg = (hdg + 180.0).rem_euclid(360.0);
                    let (along_m, cross_m) =
                        ray_along_and_cross_m(r_lat, r_lon, outbound_brg, lat, lon);
                    let along_mi = along_m / MILE_M;
                    let cross_mi = cross_m / MILE_M;
                    let gpa = val_f64(get_ci(a, "VISUAL_GLIDE_PATH_ANGLE")).unwrap_or(3.0);
                    let tch = val_f64(get_ci(a, "THR_CROSSING_HGT")).unwrap_or(50.0);
                    let ils_type = clean_str(get_ci(a, "ILS_TYPE"));

                    let ap_meta = airport_map.get(&arpt_id);
                    let has_ils = ils_type
                        .as_deref()
                        .map(|s| !s.eq_ignore_ascii_case("NONE"))
                        .unwrap_or(false);
                    let (sel_1000, day_ops, night_ops) = if class_b_or_c_idents.contains(&arpt_id) {
                        (95.0, 75.0, 10.0)
                    } else if has_ils {
                        (93.0, 35.0, 3.0)
                    } else if ap_meta.map(|m| m.iap_exists).unwrap_or(false) {
                        (85.0, 25.0, 1.5)
                    } else {
                        (78.0, 15.0, 0.5)
                    };

                    if let Some((ldn, lmax, alt_agl_ft, slant_ft)) = model_faa_aedt_runway_corridor(
                        along_mi, cross_mi, gpa, tch, sel_1000, day_ops, night_ops,
                    ) {
                        corridor_ldns.push(ldn);
                        peak_lmax = Some(peak_lmax.map_or(lmax, |p| p.max(lmax)));
                        corridors.push(json!({
                            "airportId": arpt_id,
                            "airportName": ap_meta.map(|m| m.name.as_str()).unwrap_or(&arpt_id),
                            "runwayId": clean_str(get_ci(a, "RWY_ID")),
                            "runwayEndId": rwy_end_id,
                            "trueAlignmentDeg": round_n(hdg, 1),
                            "outboundCourseDeg": round_n(outbound_brg, 1),
                            "ilsType": ils_type,
                            "glidepathAngleDeg": round_n(gpa, 2),
                            "thresholdCrossingHeightFt": round_n(tch, 0),
                            "alongTrackMi": round_n(along_mi, 2),
                            "crossTrackMi": round_n(cross_mi, 2),
                            "estGlidepathAltAglFt": alt_agl_ft,
                            "slantDistanceFt": slant_ft,
                            "estLdnDba": ldn,
                            "peakFlyoverLmaxDba": lmax,
                            "thresholdLat": round_n(r_lat, 5),
                            "thresholdLon": round_n(r_lon, 5),
                        }));
                    }
                }
            }
        }
    }

    // Heliport proximity check (within 2.0 mi)
    for hp in airports.iter().filter(|a| a.type_code == "HP" && a.dist_mi <= 2.0) {
        let d_ft = (hp.dist_mi * 5280.0).hypot(650.0).max(300.0);
        let sel = 86.0 - 20.0 * (d_ft / 1000.0).log10() - 0.0012 * (d_ft - 1000.0).max(0.0);
        let ldn = round_n((sel + 10.0 * (4.0_f64 + 10.0 * 0.5).log10() - 49.4).clamp(25.0, 72.0), 1);
        let lmax = round_n((sel - 9.5).clamp(35.0, 88.0), 1);
        corridor_ldns.push(ldn);
        peak_lmax = Some(peak_lmax.map_or(lmax, |p| p.max(lmax)));
    }

    corridors.sort_by(|a, b| {
        let la = val_f64(a.get("estLdnDba")).unwrap_or(0.0);
        let lb = val_f64(b.get("estLdnDba")).unwrap_or(0.0);
        lb.partial_cmp(&la).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Build GeoJSON features for top 6 runway corridors (10-mi centerline + ±3° lateral cone)
    for c in corridors.iter().take(6) {
        let (Some(r_lat), Some(r_lon), Some(out_brg)) = (
            val_f64(c.get("thresholdLat")),
            val_f64(c.get("thresholdLon")),
            val_f64(c.get("outboundCourseDeg")),
        ) else {
            continue;
        };
        let arpt = c.get("airportId").and_then(Value::as_str).unwrap_or("");
        let rwy_end = c.get("runwayEndId").and_then(Value::as_str).unwrap_or("");
        let ils = c.get("ilsType").and_then(Value::as_str).unwrap_or("Visual");
        let alt_ft = c.get("estGlidepathAltAglFt").and_then(Value::as_i64).unwrap_or(0);
        let cross_mi = val_f64(c.get("crossTrackMi")).unwrap_or(0.0);
        let ldn = val_f64(c.get("estLdnDba")).unwrap_or(0.0);
        let lmax = val_f64(c.get("peakFlyoverLmaxDba")).unwrap_or(0.0);

        let end_10mi = dest_point_latlon(r_lat, r_lon, out_brg, 10.0 * MILE_M);
        let cone_left = dest_point_latlon(r_lat, r_lon, (out_brg - 3.0).rem_euclid(360.0), 10.0 * MILE_M);
        let cone_right = dest_point_latlon(r_lat, r_lon, (out_brg + 3.0).rem_euclid(360.0), 10.0 * MILE_M);
        let label = format!("{arpt} Rwy {rwy_end} ({ils})");
        let popup = format!(
            "Approach/departure corridor · {cross_mi:.2} mi cross-track · ~{alt_ft} ft AGL · Est. {ldn:.1} dBA Ldn ({lmax:.0} dBA peak)"
        );

        if cross_mi <= 2.5 {
            map_features.push(json!({
                "type": "Feature",
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[
                        [r_lon, r_lat],
                        [cone_left.1, cone_left.0],
                        [cone_right.1, cone_right.0],
                        [r_lon, r_lat]
                    ]]
                },
                "properties": {
                    "featureKind": "glidepath_cone",
                    "name": format!("{label} ±3° approach cone"),
                    "airportId": arpt,
                    "runwayEndId": rwy_end,
                    "ilsType": ils,
                    "estGlidepathAltAglFt": alt_ft,
                    "estLdnDba": ldn,
                    "peakFlyoverLmaxDba": lmax,
                    "color": "#0284c7",
                    "popup": popup,
                }
            }));
        }

        map_features.push(json!({
            "type": "Feature",
            "geometry": {
                "type": "LineString",
                "coordinates": [[r_lon, r_lat], [end_10mi.1, end_10mi.0]]
            },
            "properties": {
                "featureKind": "centerline",
                "name": format!("{label} 10-mi extended centerline"),
                "airportId": arpt,
                "runwayEndId": rwy_end,
                "ilsType": ils,
                "estGlidepathAltAglFt": alt_ft,
                "estLdnDba": ldn,
                "peakFlyoverLmaxDba": lmax,
                "color": "#0284c7",
                "dash": "6 4",
                "popup": popup,
            }
        }));
    }

    // Airport & heliport pins
    for ap in airports.iter().take(10) {
        let is_hp = ap.type_code == "HP";
        map_features.push(json!({
            "type": "Feature",
            "geometry": { "type": "Point", "coordinates": [ap.lon, ap.lat] },
            "properties": {
                "featureKind": if is_hp { "heliport" } else { "airport" },
                "name": format!("{} ({})", ap.name, ap.ident),
                "airportId": ap.ident,
                "typeCode": ap.type_code,
                "distanceMi": ap.dist_mi,
                "color": if is_hp { "#7c3aed" } else { "#1d4ed8" },
                "popup": format!(
                    "{} · {} {}{}",
                    if is_hp { "Heliport" } else { "Airport" },
                    fmt_mi(ap.dist_mi),
                    ap.direction,
                    ap.elev_ft.map(|e| format!(" · Elev {} ft", e.round() as i64)).unwrap_or_default()
                ),
            }
        }));
    }

    // Live ADS-B aircraft snapshot
    let (live_aircraft_json, adsb_source, within_5nm_cnt) = match adsb_opt {
        Some((list, src_host)) => {
            let mut w5 = 0usize;
            let mut items = Vec::new();
            for ac in list.iter().take(18) {
                if ac.dist_nm <= 5.0 {
                    w5 += 1;
                }
                let alt_col = match ac.alt_baro_ft {
                    Some(a) if a < 3_000 => "#dc2626",
                    Some(a) if a < 7_000 => "#d97706",
                    _ => "#0284c7",
                };
                let label = ac
                    .callsign
                    .clone()
                    .unwrap_or_else(|| ac.hex.to_ascii_uppercase());
                let popup = format!(
                    "Live ADS-B {}{} · {} · {:.1} NM",
                    label,
                    ac.aircraft_type
                        .as_deref()
                        .map(|t| format!(" ({t})"))
                        .unwrap_or_default(),
                    ac.alt_baro_ft
                        .map(|a| format!("{a} ft MSL"))
                        .unwrap_or_else(|| "airborne".to_string()),
                    ac.dist_nm
                );
                map_features.push(json!({
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": [ac.lon, ac.lat] },
                    "properties": {
                        "featureKind": "aircraft",
                        "name": format!("✈ {label}"),
                        "callsign": ac.callsign,
                        "aircraftType": ac.aircraft_type,
                        "altitudeFt": ac.alt_baro_ft,
                        "groundSpeedKt": ac.ground_speed_kt,
                        "headingDeg": ac.heading_deg,
                        "verticalRateFpm": ac.vert_rate_fpm,
                        "distanceNm": ac.dist_nm,
                        "color": alt_col,
                        "popup": popup,
                    }
                }));
                if let Some(hdg) = ac.heading_deg {
                    let tip = dest_point_latlon(ac.lat, ac.lon, hdg, 950.0);
                    map_features.push(json!({
                        "type": "Feature",
                        "geometry": {
                            "type": "LineString",
                            "coordinates": [[ac.lon, ac.lat], [tip.1, tip.0]]
                        },
                        "properties": {
                            "featureKind": "aircraft_vector",
                            "name": format!("✈ {label} track"),
                            "color": alt_col,
                            "popup": popup,
                        }
                    }));
                }
                items.push(json!({
                    "hex": ac.hex,
                    "callsign": ac.callsign,
                    "aircraftType": ac.aircraft_type,
                    "altitudeFt": ac.alt_baro_ft,
                    "groundSpeedKt": ac.ground_speed_kt,
                    "headingDeg": ac.heading_deg,
                    "verticalRateFpm": ac.vert_rate_fpm,
                    "lat": ac.lat,
                    "lon": ac.lon,
                    "distanceNm": ac.dist_nm,
                }));
            }
            (items, Some(src_host), w5)
        }
        None => (Vec::new(), None, 0usize),
    };

    let est_ldn = energetic_sum_dba(&corridor_ldns);
    let mut map_layers = Vec::new();
    if !map_features.is_empty() {
        map_layers.push(json!({
            "id": "noise-flight-corridors",
            "title": "Flight approach corridors, airports & live aircraft",
            "kind": "mixed",
            "color": "#0284c7",
            "features": map_features,
        }));
    }

    let airports_json: Vec<Value> = airports
        .iter()
        .take(8)
        .map(|ap| {
            json!({
                "ident": ap.ident,
                "icao": ap.icao,
                "name": ap.name,
                "typeCode": ap.type_code,
                "elevationFt": ap.elev_ft.map(|e| e.round() as i64),
                "iapExists": ap.iap_exists,
                "milCode": ap.mil_code,
                "distanceMi": ap.dist_mi,
                "direction": ap.direction,
                "lat": ap.lat,
                "lon": ap.lon,
            })
        })
        .collect();

    Ok(Some(json!({
        "extensionId": FAA_FLIGHT_EXT_ID,
        "fetchedAt": now_iso(),
        "model": "FAA AEDT Extended Runway Glidepath Screening + Live ADS-B",
        "estLdnDba": est_ldn,
        "peakFlyoverLmaxDba": peak_lmax,
        "nearestCorridor": corridors.first().cloned().unwrap_or(Value::Null),
        "runwayCorridors": corridors.into_iter().take(6).collect::<Vec<_>>(),
        "airports": airports_json,
        "airspaceShelves": airspace_shelves,
        "liveAdsb": {
            "available": adsb_source.is_some(),
            "provider": adsb_source,
            "radiusNm": 15,
            "airborneCount": live_aircraft_json.len(),
            "within5NmCount": within_5nm_cnt,
            "aircraft": live_aircraft_json,
        },
        "btsTileLayer": bts_noise_tile_meta(state, "aviation"),
        "referenceUrl": BTS_NOISE_MAP_REF_URL,
        "faaReferenceUrl": FAA_AERO_DATA_REF_URL,
        "mapLayers": map_layers,
    })))
}

pub struct FaaFlightNoiseAdapter;

impl DataExtensionAdapter for FaaFlightNoiseAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_manifest_from_embedded(
            FAA_MANIFEST_YAML,
            FAA_FLIGHT_EXT_ID,
            "FAA Flight Paths, Runways & Live Aviation Noise",
            "Keyless FAA AIS airports and Class airspace shelves, USDOT BTS runway alignments/glidepaths, live ADS-B traffic, and FAA AEDT screening Ldn/Lmax.",
            240,
            300,
            ShareScope::Geohash5,
            900,
            FAA_AERO_DATA_REF_URL,
            BTS_NOISE_MAP_REF_URL,
            FAA_FLIGHT_SOURCE_NAME,
            FAA_AERO_DATA_REF_URL,
            BTS_NOISE_MAP_REF_URL,
        )
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
            let Some((lat, lon)) = valid_us_coords(query.lat, query.lon, query) else {
                return Ok(Vec::new());
            };
            let started = Instant::now();
            let eps = FlightNoiseEndpoints::default();
            let Some(payload_val) =
                fetch_faa_flight_noise_with_endpoints(http, &eps, lat, lon, &query.state).await?
            else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = payload_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            Ok(vec![ExtensionEnvelopeProto {
                extension_id: FAA_FLIGHT_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainEnvironmental as i32,
                priority: 240,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: FAA_FLIGHT_EXT_ID.to_string(),
                    source_name: FAA_FLIGHT_SOURCE_NAME.to_string(),
                    source_url: BTS_NOISE_MAP_REF_URL.to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&payload_val)?)),
            }])
        })
    }
}

// ============================================================================
// 2. Highway / Arterial AADT, Truck % & FHWA TNM Road Noise (`highway_traffic_noise`)
// ============================================================================

#[derive(Debug, Clone)]
pub struct HighwayNoiseEndpoints {
    pub state_hpms_template: String,
    pub national_hpms_fallback_layer: String,
    pub caltrans_aadt_layer: String,
}

impl Default for HighwayNoiseEndpoints {
    fn default() -> Self {
        Self {
            state_hpms_template: format!(
                "{USDOT_GEO_HOSTED}/HPMS_FULL_{{STATE}}_2025/FeatureServer/0"
            ),
            national_hpms_fallback_layer: format!(
                "{BTS_NTAD_SERVICES}/HPMS_FULL_US_2022_Sysnomulti_view/FeatureServer/0"
            ),
            caltrans_aadt_layer: CALTRANS_AADT_LAYER.to_string(),
        }
    }
}

/// FHWA TNM 2.5 screening line-source model for a single road corridor:
/// returns `(est_ldn_dba, peak_hour_leq_dba, peak_truck_lmax_dba)`.
pub fn model_fhwa_tnm_corridor(
    aadt: f64,
    single_unit_trucks: f64,
    combo_trucks: f64,
    speed_mph: f64,
    distance_m: f64,
    peak_hour_k_factor: Option<f64>,
) -> Option<(f64, f64, f64)> {
    if !aadt.is_finite() || aadt <= 0.0 || !distance_m.is_finite() || distance_m < 0.0 {
        return None;
    }
    let s = speed_mph.clamp(20.0, 75.0);
    let su = single_unit_trucks.clamp(0.0, aadt * 0.45);
    let ht = combo_trucks.clamp(0.0, (aadt - su).max(0.0) * 0.50);
    let autos = (aadt - su - ht).max(0.0);

    let l0_auto = 38.1 * s.log10() - 2.4;
    let l0_mt = 33.9 * s.log10() + 16.4;
    let l0_ht = 24.6 * s.log10() + 38.5;

    let d_eff = distance_m.max(15.0);
    let dist_atten = 15.0 * (d_eff / 15.0).log10() + 0.0018 * (d_eff - 150.0).max(0.0);
    // Suburban intervening row-of-buildings shielding (FHWA TNM 2.5: 0 dB <= 45 m, 4.5 dB @ 95 m, 9.0 dB >= 160 m)
    let shield_db = if d_eff <= 45.0 {
        0.0
    } else if d_eff < 160.0 {
        9.0 * ((d_eff - 45.0) / 115.0)
    } else {
        9.0
    };

    let hourly_leq_for_scale = |hourly_fraction: f64| -> f64 {
        let mut pwr = 0.0_f64;
        for (daily_vol, l0) in [(autos, l0_auto), (su, l0_mt), (ht, l0_ht)] {
            let v_hr = daily_vol * hourly_fraction;
            if v_hr > 0.01 {
                let leq_i = l0 + 10.0 * (v_hr / (1.609 * s)).log10() - dist_atten - shield_db + 0.12;
                pwr += 10.0_f64.powf(leq_i / 10.0);
            }
        }
        if pwr <= 0.0 {
            25.0
        } else {
            (10.0 * pwr.log10()).clamp(25.0, 88.0)
        }
    };

    let leq_24h = hourly_leq_for_scale(1.0 / 24.0);
    // Urban diurnal traffic split (~87% daytime 07:00-22:00, ~13% nighttime w/ +10 dB penalty -> +2.7 dB over Leq,24h)
    let ldn = round_n((leq_24h + 2.7).clamp(28.0, 90.0), 1);
    let k_pk = peak_hour_k_factor.unwrap_or(0.085).clamp(0.05, 0.16);
    let leq_pk = round_n(hourly_leq_for_scale(k_pk), 1);

    let ref_peak_veh = if ht >= 25.0 {
        l0_ht
    } else if su >= 25.0 {
        l0_mt
    } else {
        l0_auto + 4.0
    };
    let lmax_truck = round_n(
        (ref_peak_veh - 20.0 * (d_eff / 15.24).log10() - shield_db * 0.6).clamp(ldn, 92.0),
        1,
    );
    Some((ldn, leq_pk, lmax_truck))
}

fn default_speed_and_truck_pct_for_f_system(f_system: i64) -> (f64, f64, f64) {
    match f_system {
        1 => (65.0, 0.045, 0.075),
        2 => (55.0, 0.050, 0.040),
        3 => (45.0, 0.045, 0.020),
        4 => (35.0, 0.035, 0.010),
        _ => (30.0, 0.025, 0.005),
    }
}

pub async fn fetch_highway_traffic_noise_with_endpoints(
    http: &HttpClient,
    eps: &HighwayNoiseEndpoints,
    lat: f64,
    lon: f64,
    state: &str,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }

    let st_code = state.trim().to_ascii_uppercase();
    let extra = vec![
        ("where", "aadt > 0".to_string()),
        ("resultRecordCount", "80".to_string()),
        ("maxAllowableOffset", "0.00008".to_string()),
    ];
    let hpms_fut = async {
        if st_code.len() == 2 && st_code.chars().all(|c| c.is_ascii_alphabetic()) {
            let state_url = eps.state_hpms_template.replace("{STATE}", &st_code);
            if let Ok(feats) = http
                .arcgis_radius_query(&state_url, lat, lon, 1_650.0, "*", &extra, REQ_TIMEOUT_MS)
                .await
            {
                if !feats.is_empty() {
                    return Ok((feats, "HPMS_STATE_2025"));
                }
            }
        }
        let fb_extra = vec![
            ("where", "AADT > 0".to_string()),
            ("resultRecordCount", "80".to_string()),
            ("maxAllowableOffset", "0.00008".to_string()),
        ];
        http.arcgis_radius_query(
            &eps.national_hpms_fallback_layer,
            lat,
            lon,
            1_650.0,
            "ROUTE_ID,F_SYSTEM,THROUGH_LANES,AADT",
            &fb_extra,
            REQ_TIMEOUT_MS,
        )
        .await
        .map(|f| (f, "HPMS_US_2022"))
    };

    let caltrans_fut = async {
        if st_code == "CA" {
            http.arcgis_radius_query(
                &eps.caltrans_aadt_layer,
                lat,
                lon,
                2_000.0,
                "RTE,DESCRIPTION,BACK_PEAK_HOUR,BACK_AADT,AHEAD_PEAK_HOUR,AHEAD_AADT",
                &[],
                REQ_TIMEOUT_MS,
            )
            .await
            .ok()
        } else {
            None
        }
    };

    let (hpms_res, caltrans_opt) = tokio::join!(hpms_fut, caltrans_fut);

    let mut caltrans_peaks = Vec::new();
    let mut caltrans_k_by_rte: HashMap<String, (f64, i64)> = HashMap::new();
    if let Some(ct_feats) = caltrans_opt {
        for f in ct_feats.iter().take(12) {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let rte = clean_str(get_ci(a, "RTE")).unwrap_or_default();
            let desc = clean_str(get_ci(a, "DESCRIPTION"));
            let aadt_v = val_f64(get_ci(a, "AHEAD_AADT"))
                .or_else(|| val_f64(get_ci(a, "BACK_AADT")))
                .unwrap_or(0.0);
            let pk_v = val_f64(get_ci(a, "AHEAD_PEAK_HOUR"))
                .or_else(|| val_f64(get_ci(a, "BACK_PEAK_HOUR")))
                .unwrap_or(0.0);
            if aadt_v > 0.0 && pk_v > 0.0 {
                let k = (pk_v / aadt_v).clamp(0.05, 0.16);
                let norm_rte = rte.trim_start_matches('0').to_string();
                if !norm_rte.is_empty() {
                    caltrans_k_by_rte.insert(norm_rte, (k, pk_v.round() as i64));
                }
                caltrans_peaks.push(json!({
                    "route": rte,
                    "location": desc,
                    "aadt": aadt_v.round() as i64,
                    "peakHourVolume": pk_v.round() as i64,
                    "kFactor": round_n(k, 3),
                }));
            }
        }
    }

    let Ok((feats, dataset_tag)) = hpms_res else {
        if caltrans_peaks.is_empty() {
            return Ok(None);
        }
        return Ok(Some(json!({
            "extensionId": HIGHWAY_TRAFFIC_EXT_ID,
            "fetchedAt": now_iso(),
            "model": "FHWA Traffic Noise Model (TNM 2.5) Line-Source Screening",
            "estLdnDba": null,
            "peakHourLeqDba": null,
            "peakTruckLmaxDba": null,
            "corridors": [],
            "caltransPeakHours": caltrans_peaks,
            "btsTileLayer": bts_noise_tile_meta(state, "road"),
            "referenceUrl": BTS_NOISE_MAP_REF_URL,
            "fhwaReferenceUrl": FHWA_HPMS_REF_URL,
            "mapLayers": [],
        })));
    };

    struct CorridorEntry {
        key: String,
        route_id: String,
        f_system: i64,
        lanes: Option<i64>,
        aadt: f64,
        su_trucks: f64,
        combo_trucks: f64,
        trucks_estimated: bool,
        speed_mph: f64,
        speed_estimated: bool,
        dist_m: f64,
        peak_hour_vol: Option<i64>,
        k_factor: Option<f64>,
        geoms: Vec<Value>,
    }

    let mut by_corridor: HashMap<String, CorridorEntry> = HashMap::new();

    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let Some(aadt) = val_f64(get_ci(a, "aadt")).filter(|&v| v > 0.0) else {
            continue;
        };
        let Some(geom) = f.get("geometry") else {
            continue;
        };
        let Some(dist_m) = polyline_distance_m(lat, lon, geom) else {
            continue;
        };
        if dist_m > 1_650.0 {
            continue;
        }
        let raw_route = clean_str(get_ci(a, "route_name"))
            .or_else(|| clean_str(get_ci(a, "route_id")))
            .unwrap_or_else(|| "Arterial Segment".to_string());
        let f_sys = val_f64(get_ci(a, "f_system")).map(|v| v.round() as i64).unwrap_or(4);
        let lanes = val_f64(get_ci(a, "through_lanes")).map(|v| v.round() as i64);

        let (def_spd, def_su_pct, def_ht_pct) = default_speed_and_truck_pct_for_f_system(f_sys);
        let su_opt = val_f64(get_ci(a, "aadt_single_unit")).filter(|&v| v >= 0.0);
        let ht_opt = val_f64(get_ci(a, "aadt_combination")).filter(|&v| v >= 0.0);
        let spd_opt = val_f64(get_ci(a, "speed_limit")).filter(|&v| (15.0..=85.0).contains(&v));

        let trucks_estimated = su_opt.is_none() && ht_opt.is_none();
        let su = su_opt.unwrap_or(aadt * def_su_pct);
        let ht = ht_opt.unwrap_or(aadt * def_ht_pct);
        let speed_estimated = spd_opt.is_none();
        let spd = spd_opt.unwrap_or(def_spd);

        // Match Caltrans peak hour if route digits match
        let digits: String = raw_route
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .trim_start_matches('0')
            .to_string();
        let ct_match = (!digits.is_empty())
            .then(|| caltrans_k_by_rte.get(&digits).copied())
            .flatten();

        let norm_key = raw_route
            .to_ascii_uppercase()
            .replace(['.', '_'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let gj_opt = arcgis_to_geojson_geometry(geom);
        match by_corridor.get_mut(&norm_key) {
            Some(existing) => {
                if dist_m < existing.dist_m {
                    existing.dist_m = dist_m;
                    existing.aadt = existing.aadt.max(aadt);
                    existing.su_trucks = existing.su_trucks.max(su);
                    existing.combo_trucks = existing.combo_trucks.max(ht);
                    existing.speed_mph = spd;
                }
                if let Some(gj) = gj_opt {
                    if existing.geoms.len() < 8 {
                        existing.geoms.push(gj);
                    }
                }
            }
            None => {
                let mut geoms = Vec::new();
                if let Some(gj) = gj_opt {
                    geoms.push(gj);
                }
                by_corridor.insert(
                    norm_key.clone(),
                    CorridorEntry {
                        key: norm_key,
                        route_id: raw_route,
                        f_system: f_sys,
                        lanes,
                        aadt,
                        su_trucks: su,
                        combo_trucks: ht,
                        trucks_estimated,
                        speed_mph: spd,
                        speed_estimated,
                        dist_m,
                        peak_hour_vol: ct_match.map(|(_, pk)| pk),
                        k_factor: ct_match.map(|(k, _)| k),
                        geoms,
                    },
                );
            }
        }
    }

    let mut corridors_json = Vec::new();
    let mut ldn_list = Vec::new();
    let mut pk_leq_list = Vec::new();
    let mut max_truck_lmax: Option<f64> = None;
    let mut map_features = Vec::new();

    let mut entries: Vec<CorridorEntry> = by_corridor.into_values().collect();
    entries.sort_by(|a, b| {
        a.dist_m
            .partial_cmp(&b.dist_m)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for ent in entries {
        let Some((ldn, pk_leq, lmax_tr)) = model_fhwa_tnm_corridor(
            ent.aadt,
            ent.su_trucks,
            ent.combo_trucks,
            ent.speed_mph,
            ent.dist_m,
            ent.k_factor,
        ) else {
            continue;
        };
        ldn_list.push(ldn);
        pk_leq_list.push(pk_leq);
        max_truck_lmax = Some(max_truck_lmax.map_or(lmax_tr, |p| p.max(lmax_tr)));

        let total_truck_pct = round_n(((ent.su_trucks + ent.combo_trucks) / ent.aadt) * 100.0, 1);
        let combo_pct = round_n((ent.combo_trucks / ent.aadt) * 100.0, 1);
        let dist_mi = round_n(ent.dist_m / MILE_M, 2);
        let color = if ent.aadt >= 80_000.0 || ldn >= 62.0 {
            "#b91c1c"
        } else if ent.aadt >= 20_000.0 || ldn >= 54.0 {
            "#ea580c"
        } else if ent.aadt >= 8_000.0 {
            "#d97706"
        } else {
            "#0284c7"
        };

        for gj in &ent.geoms {
            map_features.push(json!({
                "type": "Feature",
                "geometry": gj,
                "properties": {
                    "featureKind": "highway_aadt",
                    "name": ent.route_id,
                    "aadt": ent.aadt.round() as i64,
                    "singleUnitTrucks": ent.su_trucks.round() as i64,
                    "comboTrucks": ent.combo_trucks.round() as i64,
                    "heavyTruckPct": total_truck_pct,
                    "speedLimitMph": ent.speed_mph.round() as i64,
                    "lanes": ent.lanes,
                    "distanceM": ent.dist_m.round() as i64,
                    "estLdnDba": ldn,
                    "peakHourLeqDba": pk_leq,
                    "color": color,
                    "weight": if ent.aadt >= 80_000.0 { 5 } else if ent.aadt >= 20_000.0 { 4 } else { 3 },
                    "popup": format!(
                        "AADT {} · Trucks {:.1}% · {} mph · {} m away · Est. {:.1} dBA Ldn",
                        ent.aadt.round() as i64,
                        total_truck_pct,
                        ent.speed_mph.round() as i64,
                        ent.dist_m.round() as i64,
                        ldn
                    ),
                }
            }));
        }

        corridors_json.push(json!({
            "key": ent.key,
            "routeId": ent.route_id,
            "functionalClass": ent.f_system,
            "lanes": ent.lanes,
            "aadt": ent.aadt.round() as i64,
            "singleUnitTrucks": ent.su_trucks.round() as i64,
            "comboTrucks": ent.combo_trucks.round() as i64,
            "heavyTruckPct": total_truck_pct,
            "comboTruckPct": combo_pct,
            "truckShareEstimated": ent.trucks_estimated,
            "speedLimitMph": ent.speed_mph.round() as i64,
            "speedEstimated": ent.speed_estimated,
            "distanceM": ent.dist_m.round() as i64,
            "distanceMi": dist_mi,
            "peakHourVolume": ent.peak_hour_vol,
            "estLdnDba": ldn,
            "peakHourLeqDba": pk_leq,
            "peakTruckLmaxDba": lmax_tr,
        }));
    }

    corridors_json.sort_by(|a, b| {
        let la = val_f64(a.get("estLdnDba")).unwrap_or(0.0);
        let lb = val_f64(b.get("estLdnDba")).unwrap_or(0.0);
        lb.partial_cmp(&la).unwrap_or(std::cmp::Ordering::Equal)
    });

    let est_ldn = energetic_sum_dba(&ldn_list);
    let est_pk_leq = energetic_sum_dba(&pk_leq_list);
    let mut map_layers = Vec::new();
    if !map_features.is_empty() {
        map_layers.push(json!({
            "id": "noise-highway-aadt",
            "title": "HPMS highway & arterial AADT / truck corridors",
            "kind": "lines",
            "color": "#ea580c",
            "features": map_features,
        }));
    }

    Ok(Some(json!({
        "extensionId": HIGHWAY_TRAFFIC_EXT_ID,
        "fetchedAt": now_iso(),
        "dataset": dataset_tag,
        "model": "FHWA Traffic Noise Model (TNM 2.5) Line-Source Screening",
        "estLdnDba": est_ldn,
        "peakHourLeqDba": est_pk_leq,
        "peakTruckLmaxDba": max_truck_lmax,
        "nearestCorridor": corridors_json.first().cloned().unwrap_or(Value::Null),
        "corridors": corridors_json.into_iter().take(10).collect::<Vec<_>>(),
        "caltransPeakHours": caltrans_peaks,
        "btsTileLayer": bts_noise_tile_meta(state, "road"),
        "referenceUrl": BTS_NOISE_MAP_REF_URL,
        "fhwaReferenceUrl": FHWA_HPMS_REF_URL,
        "mapLayers": map_layers,
    })))
}

pub struct HighwayTrafficNoiseAdapter;

impl DataExtensionAdapter for HighwayTrafficNoiseAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_manifest_from_embedded(
            HIGHWAY_MANIFEST_YAML,
            HIGHWAY_TRAFFIC_EXT_ID,
            "USDOT FHWA Highway AADT, Truck Share & TNM Road Noise",
            "Keyless USDOT FHWA HPMS corridor polylines, measured AADT, heavy-truck share, Caltrans peak-hour volumes, and FHWA TNM 2.5 Ldn/Leq modeling.",
            238,
            604_800,
            ShareScope::Geohash6,
            86_400,
            FHWA_HPMS_REF_URL,
            BTS_NOISE_MAP_REF_URL,
            HIGHWAY_TRAFFIC_SOURCE_NAME,
            FHWA_HPMS_REF_URL,
            BTS_NOISE_MAP_REF_URL,
        )
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
            let Some((lat, lon)) = valid_us_coords(query.lat, query.lon, query) else {
                return Ok(Vec::new());
            };
            let started = Instant::now();
            let eps = HighwayNoiseEndpoints::default();
            let Some(payload_val) =
                fetch_highway_traffic_noise_with_endpoints(http, &eps, lat, lon, &query.state)
                    .await?
            else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = payload_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            Ok(vec![ExtensionEnvelopeProto {
                extension_id: HIGHWAY_TRAFFIC_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainEnvironmental as i32,
                priority: 238,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: HIGHWAY_TRAFFIC_EXT_ID.to_string(),
                    source_name: HIGHWAY_TRAFFIC_SOURCE_NAME.to_string(),
                    source_url: FHWA_HPMS_REF_URL.to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&payload_val)?)),
            }])
        })
    }
}

// ============================================================================
// 3. FRA / FTA Rail Schedules, Quiet Zones & Horn Noise (`rail_transit_noise`)
// ============================================================================

#[derive(Debug, Clone)]
pub struct RailNoiseEndpoints {
    pub grade_crossings_layer: String,
    pub rail_lines_layer: String,
    pub transit_routes_layer: String,
}

impl Default for RailNoiseEndpoints {
    fn default() -> Self {
        Self {
            grade_crossings_layer: format!(
                "{BTS_NTAD_SERVICES}/NTAD_Railroad_Grade_Crossings/FeatureServer/0"
            ),
            rail_lines_layer: format!(
                "{BTS_NTAD_SERVICES}/NTAD_North_American_Rail_Network_Lines/FeatureServer/0"
            ),
            transit_routes_layer: format!(
                "{BTS_NTAD_SERVICES}/NTAD_National_Transit_Map_Routes/FeatureServer/0"
            ),
        }
    }
}

/// FTA 2018 wayside rail `Ldn` model for `rail_kind` (`"light_rail"`, `"passenger"`, `"freight"`).
pub fn model_fta_wayside_rail_ldn(
    rail_kind: &str,
    speed_mph: f64,
    day_trains: f64,
    night_trains: f64,
    distance_m: f64,
) -> Option<f64> {
    let weighted_trains = day_trains.max(0.0) + 10.0 * night_trains.max(0.0);
    if weighted_trains <= 0.0 || !distance_m.is_finite() || distance_m < 0.0 {
        return None;
    }
    let s = speed_mph.clamp(10.0, 90.0);
    let base_sel50 = match rail_kind {
        "light_rail" => 82.0,
        "passenger" => 92.0,
        _ => 96.0,
    };
    let sel50 = base_sel50 + 20.0 * (s / 50.0).log10();
    let d_eff = distance_m.max(15.24);
    let ldn = sel50 - 15.0 * (d_eff / 15.24).log10() + 10.0 * weighted_trains.log10() - 49.4;
    Some(round_n(ldn.clamp(22.0, 88.0), 1))
}

/// 49 CFR Part 222 grade crossing horn impulse `Lmax` & `Ldn` contribution.
/// Strictly suppresses locomotive horn impulses when `whistle_ban` indicates a `'24 hr'` Quiet Zone.
pub fn model_crossing_horn_noise(
    rail_kind: &str,
    whistle_ban: Option<&str>,
    is_at_grade: bool,
    day_trains: f64,
    night_trains: f64,
    distance_m: f64,
) -> (bool, Option<f64>, Option<f64>) {
    let wb = whistle_ban.unwrap_or("").trim().to_ascii_lowercase();
    let is_24hr_quiet_zone = wb.contains("24") || wb == "1" || wb.contains("quiet");
    if !is_at_grade || is_24hr_quiet_zone {
        return (is_24hr_quiet_zone, None, None);
    }
    let total_trains = day_trains.max(0.0) + night_trains.max(0.0);
    if total_trains <= 0.0 || !distance_m.is_finite() {
        return (false, None, None);
    }
    let l_horn_100ft = if rail_kind == "light_rail" { 88.0 } else { 108.0 };
    let d_eff = distance_m.max(15.0);
    let lmax = (l_horn_100ft
        - 20.0 * (d_eff / 30.48).log10()
        - 0.002 * (d_eff - 100.0).max(0.0))
    .clamp(30.0, 110.0);

    // Partial (nighttime 10pm-6am) whistle ban suppresses the 10x night horn penalty
    let eff_night = if wb.contains("partial") {
        0.0
    } else {
        night_trains.max(0.0)
    };
    let weighted = (day_trains.max(0.0) + 10.0 * eff_night).max(0.0);
    let horn_ldn = if weighted > 0.0 {
        let sel_xing = lmax + 4.0;
        Some(round_n(
            (sel_xing + 10.0 * weighted.log10() - 49.4).clamp(22.0, 88.0),
            1,
        ))
    } else {
        None
    };
    (false, Some(round_n(lmax, 1)), horn_ldn)
}

fn safe_fra_url(raw: Option<String>) -> Option<String> {
    let s = raw?;
    let parsed = Url::parse(s.trim()).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.ends_with(".gov") {
        Some(parsed.to_string())
    } else {
        None
    }
}

pub async fn fetch_rail_transit_noise_with_endpoints(
    http: &HttpClient,
    eps: &RailNoiseEndpoints,
    lat: f64,
    lon: f64,
    state: &str,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }

    let line_extra = [("maxAllowableOffset", "0.00008".to_string())];
    let xings_fut = http.arcgis_radius_query(
        &eps.grade_crossings_layer,
        lat,
        lon,
        2_500.0,
        "CrossingID,STREET,RailroadCode,ReportingAgencyName,RailroadSubdivision,CrossingPosition,WhistleBan,TotalDaylightThruTrains,TotalNighttimeThruTrains,TotalSwitchingTrains,MaximumTimetableSpeed,WDCODE,NumberOfMainTracks,NumberOfTransitTracks,NumberOfIndistryTracks,NearestTimetableStation,URL",
        &[],
        REQ_TIMEOUT_MS,
    );
    let rail_fut = http.arcgis_radius_query(
        &eps.rail_lines_layer,
        lat,
        lon,
        2_500.0,
        "FRAARCID,RROWNER1,TRKRGHTS1,SUBDIV,TRACKS,NET,PASSNGR",
        &line_extra,
        REQ_TIMEOUT_MS,
    );
    let transit_fut = http.arcgis_radius_query(
        &eps.transit_routes_layer,
        lat,
        lon,
        2_000.0,
        "agency_name,route_id,route_short_name,route_long_name,route_type,route_color",
        &line_extra,
        REQ_TIMEOUT_MS,
    );

    let (xings_res, rail_res, transit_res) = tokio::join!(xings_fut, rail_fut, transit_fut);
    if xings_res.is_err() && rail_res.is_err() && transit_res.is_err() {
        return Ok(None);
    }

    let mut line_features = Vec::new();
    let mut rail_corridors = Vec::new();
    let mut min_freight_dist_m: Option<f64> = None;
    let mut min_lrt_dist_m: Option<f64> = None;
    let mut min_pass_dist_m: Option<f64> = None;

    if let Ok(r_feats) = rail_res {
        let mut seen = HashSet::new();
        for f in &r_feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let Some(geom) = f.get("geometry") else { continue };
            let Some(dist_m) = polyline_distance_m(lat, lon, geom) else { continue };
            let owner = clean_str(get_ci(a, "RROWNER1")).unwrap_or_else(|| "Rail".to_string());
            let subdiv = clean_str(get_ci(a, "SUBDIV")).unwrap_or_default();
            let tracks = val_f64(get_ci(a, "TRACKS")).map(|v| v.round() as i64);
            let pass = clean_str(get_ci(a, "PASSNGR")).unwrap_or_default();
            let is_pass = !pass.is_empty() && pass != "N" && pass != "0";
            if is_pass {
                min_pass_dist_m = Some(min_pass_dist_m.map_or(dist_m, |p| p.min(dist_m)));
            } else {
                min_freight_dist_m = Some(min_freight_dist_m.map_or(dist_m, |p| p.min(dist_m)));
            }
            if let Some(gj) = arcgis_to_geojson_geometry(geom) {
                let lbl = if subdiv.is_empty() {
                    owner.clone()
                } else {
                    format!("{owner} — {subdiv}")
                };
                line_features.push(json!({
                    "type": "Feature",
                    "geometry": gj,
                    "properties": {
                        "featureKind": if is_pass { "passenger_rail" } else { "freight_rail" },
                        "name": lbl,
                        "owner": owner,
                        "subdivision": subdiv,
                        "tracks": tracks,
                        "distanceM": dist_m.round() as i64,
                        "color": if is_pass { "#2563eb" } else { "#7c3aed" },
                        "popup": format!(
                            "{} · {} m away{}",
                            if is_pass { "Passenger/Commuter Rail" } else { "FRA Rail Line" },
                            dist_m.round() as i64,
                            tracks.map(|t| format!(" · {t} track(s)")).unwrap_or_default()
                        ),
                    }
                }));
                let key = format!("{owner}|{subdiv}");
                if seen.insert(key) {
                    rail_corridors.push(json!({
                        "kind": if is_pass { "passenger" } else { "freight" },
                        "owner": owner,
                        "subdivision": if subdiv.is_empty() { Value::Null } else { json!(subdiv) },
                        "tracks": tracks,
                        "distanceM": dist_m.round() as i64,
                        "distanceMi": round_n(dist_m / MILE_M, 2),
                    }));
                }
            }
        }
    }

    let mut transit_routes = Vec::new();
    if let Ok(t_feats) = transit_res {
        let mut seen = HashSet::new();
        for f in &t_feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let Some(geom) = f.get("geometry") else { continue };
            let Some(dist_m) = polyline_distance_m(lat, lon, geom) else { continue };
            let rtype = clean_str(get_ci(a, "route_type")).unwrap_or_else(|| "3".to_string());
            let agency = clean_str(get_ci(a, "agency_name")).unwrap_or_else(|| "Transit".to_string());
            let short_n = clean_str(get_ci(a, "route_short_name"));
            let long_n = clean_str(get_ci(a, "route_long_name"));
            let rname = match (&short_n, &long_n) {
                (Some(s), Some(l)) if !l.contains(s) => format!("{s} — {l}"),
                (None, Some(l)) => l.clone(),
                (Some(s), _) => s.clone(),
                _ => clean_str(get_ci(a, "route_id")).unwrap_or_else(|| "Route".to_string()),
            };
            let is_rail_transit = matches!(rtype.as_str(), "0" | "1" | "2");
            if rtype == "0" || rtype == "1" {
                min_lrt_dist_m = Some(min_lrt_dist_m.map_or(dist_m, |p| p.min(dist_m)));
            } else if rtype == "2" {
                min_pass_dist_m = Some(min_pass_dist_m.map_or(dist_m, |p| p.min(dist_m)));
            }
            if is_rail_transit || dist_m <= 600.0 {
                if let Some(gj) = arcgis_to_geojson_geometry(geom) {
                    let mode_lbl = match rtype.as_str() {
                        "0" => "Light Rail / Streetcar",
                        "1" => "Metro / Subway",
                        "2" => "Commuter Rail",
                        _ => "Bus Transit",
                    };
                    line_features.push(json!({
                        "type": "Feature",
                        "geometry": gj,
                        "properties": {
                            "featureKind": if is_rail_transit { "rail_transit" } else { "bus_transit" },
                            "name": format!("{agency}: {rname}"),
                            "mode": mode_lbl,
                            "routeType": rtype,
                            "distanceM": dist_m.round() as i64,
                            "color": if is_rail_transit { "#0d9488" } else { "#64748b" },
                            "dash": if is_rail_transit { Value::Null } else { json!("4 4") },
                            "popup": format!("{mode_lbl} · {agency} · {} m away", dist_m.round() as i64),
                        }
                    }));
                }
            }
            let dedup = format!("{agency}|{rname}|{rtype}");
            if seen.insert(dedup) && transit_routes.len() < 12 {
                transit_routes.push(json!({
                    "agency": agency,
                    "routeName": rname,
                    "routeType": rtype,
                    "isRail": is_rail_transit,
                    "distanceM": dist_m.round() as i64,
                    "distanceMi": round_n(dist_m / MILE_M, 2),
                }));
            }
        }
    }

    let mut crossings_json = Vec::new();
    let mut xing_features = Vec::new();
    let mut horn_ldns = Vec::new();
    let mut peak_horn_lmax: Option<f64> = None;
    let mut quiet_zone_count = 0usize;
    let mut active_horn_count = 0usize;

    // Track schedule stats per rail kind from crossing inventory
    let mut best_schedule_by_kind: HashMap<&'static str, (f64, f64, f64, f64)> = HashMap::new();

    if let Ok(x_feats) = xings_res {
        let mut seen = HashSet::new();
        for f in &x_feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let xid = clean_str(get_ci(a, "CrossingID")).unwrap_or_default();
            if !xid.is_empty() && !seen.insert(xid.clone()) {
                continue;
            }
            let Some((la, lo)) = f.get("geometry").and_then(arcgis_representative_point) else {
                continue;
            };
            let dist_m = haversine_m(lat, lon, la, lo);
            if dist_m > 2_550.0 {
                continue;
            }
            let street = clean_str(get_ci(a, "STREET")).unwrap_or_else(|| "Crossing".to_string());
            let rr_code = clean_str(get_ci(a, "RailroadCode")).unwrap_or_default();
            let agency = clean_str(get_ci(a, "ReportingAgencyName"));
            let subdiv = clean_str(get_ci(a, "RailroadSubdivision"));
            let pos = clean_str(get_ci(a, "CrossingPosition")).unwrap_or_else(|| "At Grade".to_string());
            let wb = clean_str(get_ci(a, "WhistleBan"));
            let day_tr = val_f64(get_ci(a, "TotalDaylightThruTrains")).unwrap_or(0.0);
            let night_tr = val_f64(get_ci(a, "TotalNighttimeThruTrains")).unwrap_or(0.0);
            let switch_tr = val_f64(get_ci(a, "TotalSwitchingTrains")).unwrap_or(0.0);
            let speed = val_f64(get_ci(a, "MaximumTimetableSpeed")).unwrap_or(35.0);
            let transit_tracks = val_f64(get_ci(a, "NumberOfTransitTracks")).unwrap_or(0.0);

            let rr_hay = format!("{} {}", rr_code, agency.as_deref().unwrap_or("")).to_ascii_lowercase();
            let rail_kind = if transit_tracks > 0.0
                || rr_hay.contains("vta")
                || rr_hay.contains("light rail")
                || rr_hay.contains("metro")
                || rr_hay.contains("trolley")
                || rr_hay.contains("trimet")
                || rr_hay.contains("muni")
                || rr_hay.contains("darta")
            {
                "light_rail"
            } else if rr_hay.contains("amtrak")
                || rr_hay.contains("caltrain")
                || rr_hay.contains("metra")
                || rr_hay.contains("metrolink")
                || rr_hay.contains("njt")
                || rr_hay.contains("lirr")
                || rr_hay.contains("mbta")
                || rr_hay.contains("septa")
            {
                "passenger"
            } else {
                "freight"
            };

            let total_daily = day_tr + night_tr + switch_tr;
            if total_daily > 0.0 {
                let entry = best_schedule_by_kind
                    .entry(rail_kind)
                    .or_insert((0.0, 0.0, speed, dist_m));
                if day_tr + night_tr > entry.0 + entry.1 || dist_m < entry.3 {
                    entry.0 = entry.0.max(day_tr + switch_tr * 0.5);
                    entry.1 = entry.1.max(night_tr);
                    entry.2 = entry.2.max(speed);
                    entry.3 = entry.3.min(dist_m);
                }
            }

            let pos_low = pos.to_ascii_lowercase();
            let is_at_grade = pos_low.contains("at grade")
                || (!pos_low.contains("over") && !pos_low.contains("under"));
            let (is_qz, horn_lmax, horn_ldn) = model_crossing_horn_noise(
                rail_kind,
                wb.as_deref(),
                is_at_grade,
                day_tr + switch_tr * 0.5,
                night_tr,
                dist_m,
            );
            if is_at_grade && total_daily > 0.0 {
                if is_qz {
                    quiet_zone_count += 1;
                } else {
                    active_horn_count += 1;
                }
            }
            if let Some(hl) = horn_ldn {
                horn_ldns.push(hl);
            }
            if let Some(hm) = horn_lmax {
                peak_horn_lmax = Some(peak_horn_lmax.map_or(hm, |p| p.max(hm)));
            }

            let pin_color = if !is_at_grade || total_daily <= 0.0 {
                "#64748b"
            } else if is_qz {
                "#0d9488"
            } else {
                "#dc2626"
            };
            let status_tag = if !is_at_grade {
                pos.clone()
            } else if is_qz {
                "24 hr Quiet Zone (horn ban)".to_string()
            } else if total_daily <= 0.0 {
                "0 daily thru trains".to_string()
            } else {
                format!(
                    "Active horn{}",
                    horn_lmax
                        .map(|m| format!(" (~{m:.0} dBA peak at home)"))
                        .unwrap_or_default()
                )
            };
            let fra_url = safe_fra_url(clean_str(get_ci(a, "URL")));
            xing_features.push(json!({
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [round_n(lo, 6), round_n(la, 6)] },
                "properties": {
                    "featureKind": "grade_crossing",
                    "crossingId": xid,
                    "name": format!("{street} ({})", if rr_code.is_empty() { "Rail" } else { &rr_code }),
                    "street": street,
                    "railroad": agency.clone().unwrap_or_else(|| rr_code.clone()),
                    "crossingPosition": pos,
                    "whistleBan": wb,
                    "isQuietZone": is_qz,
                    "dayTrains": day_tr.round() as i64,
                    "nightTrains": night_tr.round() as i64,
                    "maxSpeedMph": speed.round() as i64,
                    "estHornLmaxDba": horn_lmax,
                    "distanceM": dist_m.round() as i64,
                    "url": fra_url,
                    "color": pin_color,
                    "popup": format!(
                        "{status_tag} · {} day / {} night trains · {} mph · {}",
                        day_tr.round() as i64,
                        night_tr.round() as i64,
                        speed.round() as i64,
                        fmt_mi(dist_m / MILE_M)
                    ),
                }
            }));

            crossings_json.push(json!({
                "crossingId": if xid.is_empty() { Value::Null } else { json!(xid) },
                "street": street,
                "railroadCode": if rr_code.is_empty() { Value::Null } else { json!(rr_code) },
                "agencyName": agency,
                "subdivision": subdiv,
                "railKind": rail_kind,
                "crossingPosition": pos,
                "isAtGrade": is_at_grade,
                "whistleBan": wb,
                "isQuietZone": is_qz,
                "daylightThruTrains": day_tr.round() as i64,
                "nighttimeThruTrains": night_tr.round() as i64,
                "switchingTrains": switch_tr.round() as i64,
                "maxSpeedMph": speed.round() as i64,
                "estHornLmaxDba": horn_lmax,
                "estHornLdnDba": horn_ldn,
                "distanceM": dist_m.round() as i64,
                "distanceMi": round_n(dist_m / MILE_M, 2),
                "url": fra_url,
                "lat": round_n(la, 5),
                "lon": round_n(lo, 5),
            }));
        }
    }

    crossings_json.sort_by(|a, b| {
        let da = val_f64(a.get("distanceM")).unwrap_or(f64::INFINITY);
        let db = val_f64(b.get("distanceM")).unwrap_or(f64::INFINITY);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Compute wayside Ldn across active rail kinds
    let mut wayside_ldns = Vec::new();
    for (kind, (day_n, night_n, spd, xing_dist_m)) in &best_schedule_by_kind {
        let line_dist = match *kind {
            "light_rail" => min_lrt_dist_m.unwrap_or(*xing_dist_m),
            "passenger" => min_pass_dist_m.unwrap_or(*xing_dist_m),
            _ => min_freight_dist_m.unwrap_or(*xing_dist_m),
        };
        if let Some(w_ldn) = model_fta_wayside_rail_ldn(kind, *spd, *day_n, *night_n, line_dist) {
            wayside_ldns.push(w_ldn);
        }
    }
    let wayside_ldn = energetic_sum_dba(&wayside_ldns);
    let mut all_rail_ldns = wayside_ldns;
    all_rail_ldns.extend(horn_ldns);
    let total_rail_ldn = energetic_sum_dba(&all_rail_ldns);

    let mut map_layers = Vec::new();
    if !line_features.is_empty() {
        map_layers.push(json!({
            "id": "noise-rail-lines",
            "title": "FRA rail lines & FTA rail/transit routes",
            "kind": "lines",
            "color": "#7c3aed",
            "features": line_features,
        }));
    }
    if !xing_features.is_empty() {
        map_layers.push(json!({
            "id": "noise-rail-crossings",
            "title": "Rail grade crossings (Red = Horn · Teal = 24h Quiet Zone)",
            "kind": "points",
            "color": "#dc2626",
            "features": xing_features,
        }));
    }

    Ok(Some(json!({
        "extensionId": RAIL_TRANSIT_EXT_ID,
        "fetchedAt": now_iso(),
        "model": "FTA 2018 Wayside Rail Ldn + 49 CFR 222 Grade Crossing Horn Impulse",
        "estLdnDba": total_rail_ldn,
        "waysideLdnDba": wayside_ldn,
        "peakHornLmaxDba": peak_horn_lmax,
        "quietZoneCrossingCount": quiet_zone_count,
        "activeHornCrossingCount": active_horn_count,
        "nearestCrossing": crossings_json.first().cloned().unwrap_or(Value::Null),
        "crossings": crossings_json.into_iter().take(10).collect::<Vec<_>>(),
        "railCorridors": rail_corridors,
        "transitRoutes": transit_routes,
        "btsTileLayer": bts_noise_tile_meta(state, "rail"),
        "referenceUrl": BTS_TRANSIT_MAP_REF_URL,
        "noiseMapUrl": BTS_NOISE_MAP_REF_URL,
        "mapLayers": map_layers,
    })))
}

pub struct RailTransitNoiseAdapter;

impl DataExtensionAdapter for RailTransitNoiseAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_manifest_from_embedded(
            RAIL_MANIFEST_YAML,
            RAIL_TRANSIT_EXT_ID,
            "FRA / FTA Rail Schedules, Quiet Zones & Wayside Horn Noise",
            "Keyless USDOT BTS / FRA Grade Crossing Inventory train schedules and 24-hour Quiet Zone horn bans, North American Rail Network lines, and National Transit Map routes.",
            236,
            604_800,
            ShareScope::Geohash6,
            86_400,
            BTS_TRANSIT_MAP_REF_URL,
            BTS_NOISE_MAP_REF_URL,
            RAIL_TRANSIT_SOURCE_NAME,
            BTS_TRANSIT_MAP_REF_URL,
            BTS_TRANSIT_MAP_REF_URL,
        )
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
            let Some((lat, lon)) = valid_us_coords(query.lat, query.lon, query) else {
                return Ok(Vec::new());
            };
            let started = Instant::now();
            let eps = RailNoiseEndpoints::default();
            let Some(payload_val) =
                fetch_rail_transit_noise_with_endpoints(http, &eps, lat, lon, &query.state).await?
            else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = payload_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            Ok(vec![ExtensionEnvelopeProto {
                extension_id: RAIL_TRANSIT_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainEnvironmental as i32,
                priority: 236,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: RAIL_TRANSIT_EXT_ID.to_string(),
                    source_name: RAIL_TRANSIT_SOURCE_NAME.to_string(),
                    source_url: BTS_TRANSIT_MAP_REF_URL.to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&payload_val)?)),
            }])
        })
    }
}

// ============================================================================
// 4. Military Airspace SUA Schedules, MTR Corridors & DoD Airfields
// ============================================================================

#[derive(Debug, Clone)]
pub struct MilitaryNoiseEndpoints {
    pub sua_layer: String,
    pub mtr_layer: String,
    pub dod_airspace_layer: String,
    pub military_bases_layer: String,
    pub airports_layer: String,
    pub runway_ends_layer: String,
}

impl Default for MilitaryNoiseEndpoints {
    fn default() -> Self {
        Self {
            sua_layer: format!("{FAA_AIS_SERVICES}/Special_Use_Airspace/FeatureServer/0"),
            mtr_layer: format!("{FAA_AIS_SERVICES}/MTRSegment/FeatureServer/0"),
            dod_airspace_layer: format!("{FAA_AIS_SERVICES}/DoD_Mar_13/FeatureServer/0"),
            military_bases_layer: format!(
                "{BTS_NTAD_SERVICES}/NTAD_Military_Bases/FeatureServer/0"
            ),
            airports_layer: format!("{FAA_AIS_SERVICES}/US_Airport/FeatureServer/0"),
            runway_ends_layer: format!("{BTS_NTAD_SERVICES}/Runway_Ends_Table/FeatureServer/1"),
        }
    }
}

/// DoD MR_NMAP / AICUZ screening for a low-level Military Training Route (`IR` / `VR`) segment.
/// Returns `(in_corridor, est_ldnmr_dba, peak_jet_lmax_dba)`.
pub fn model_mtr_corridor_noise(
    dist_nm: f64,
    half_width_nm: f64,
    floor_agl_ft: f64,
) -> (bool, Option<f64>, Option<f64>) {
    if !dist_nm.is_finite() || dist_nm > 15.0 {
        return (false, None, None);
    }
    let width = half_width_nm.clamp(1.0, 12.0);
    let in_corridor = dist_nm <= width;
    let h_ft = floor_agl_ft.clamp(500.0, 18_000.0);
    let lateral_excess_ft = ((dist_nm - width).max(0.0)) * (NM_M / FOOT_M);
    let slant_ft = (h_ft * h_ft + lateral_excess_ft * lateral_excess_ft)
        .sqrt()
        .max(500.0);
    // Onset-rate adjusted fast-jet screening (+5 dB startle penalty when floor < 3,000 ft AGL inside corridor)
    let startle = if in_corridor && h_ft < 3_000.0 {
        5.0
    } else {
        0.0
    };
    let lmax = (95.0 - 20.0 * (slant_ft / 1000.0).log10() + startle).clamp(32.0, 108.0);
    if lmax < 42.0 {
        return (in_corridor, None, None);
    }
    let ldnmr = (lmax + 6.0 + 10.0 * (2.5_f64).log10() - 49.4).clamp(24.0, 78.0);
    (in_corridor, Some(round_n(ldnmr, 1)), Some(round_n(lmax, 1)))
}

fn is_federal_or_military_airport(name: &str, mil_code: Option<&str>) -> bool {
    if let Some(mc) = mil_code {
        let up = mc.trim().to_ascii_uppercase();
        if !up.is_empty() && up != "CIVIL" && up != "NONE" && up != "N" {
            return true;
        }
    }
    let u = name.to_ascii_uppercase();
    [
        "AFB",
        "AIR FORCE",
        "NAVAL",
        "NAS ",
        "MCAS",
        "ARMY",
        "AAF",
        "ANGB",
        "FEDERAL AIRFIELD",
        "MOFFETT",
        "JOINT BASE",
        "COAST GUARD",
    ]
    .iter()
    .any(|kw| u.contains(kw))
}

pub async fn fetch_military_airspace_noise_with_endpoints(
    http: &HttpClient,
    eps: &MilitaryNoiseEndpoints,
    lat: f64,
    lon: f64,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }

    let sua_extra = [("maxAllowableOffset", "0.0005".to_string())];
    let mtr_extra = [("maxAllowableOffset", "0.0003".to_string())];
    let dod_extra = [("maxAllowableOffset", "0.0003".to_string())];
    let bases_extra = [("maxAllowableOffset", "0.0004".to_string())];

    let sua_fut = http.arcgis_radius_query(
        &eps.sua_layer,
        lat,
        lon,
        80_000.0,
        "NAME,TYPE_CODE,TIMESOFUSE,UPPER_VAL,UPPER_UOM,UPPER_CODE,LOWER_VAL,LOWER_UOM,LOWER_CODE,CONT_AGENT,CITY,STATE",
        &sua_extra,
        REQ_TIMEOUT_MS,
    );
    let mtr_fut = http.arcgis_radius_query(
        &eps.mtr_layer,
        lat,
        lon,
        60_000.0,
        "IDENT,MTR_TYPE,ROUTETYPE,ALT_DESC,LOWER_VAL,UPPER_VAL,WIDTHLEFT,WIDTHRIGHT",
        &mtr_extra,
        REQ_TIMEOUT_MS,
    );
    let dod_fut = http.arcgis_radius_query(
        &eps.dod_airspace_layer,
        lat,
        lon,
        45_000.0,
        "Proponent,Branch,Base,Facility,Airspace,Floor,Ceiling",
        &dod_extra,
        REQ_TIMEOUT_MS,
    );
    let bases_fut = http.arcgis_radius_query(
        &eps.military_bases_layer,
        lat,
        lon,
        40_000.0,
        "SITE_NAME,OPER_STAT,COMPONENT,JOINT_BASE,STATE_TERR",
        &bases_extra,
        REQ_TIMEOUT_MS,
    );
    let airports_fut = http.arcgis_radius_query(
        &eps.airports_layer,
        lat,
        lon,
        30_000.0,
        "IDENT,ICAO_ID,NAME,TYPE_CODE,MIL_CODE",
        &[],
        REQ_TIMEOUT_MS,
    );

    let (sua_res, mtr_res, dod_res, bases_res, airports_res) =
        tokio::join!(sua_fut, mtr_fut, dod_fut, bases_fut, airports_fut);

    if sua_res.is_err()
        && mtr_res.is_err()
        && dod_res.is_err()
        && bases_res.is_err()
        && airports_res.is_err()
    {
        return Ok(None);
    }

    let mut map_features = Vec::new();
    let mut ldn_components = Vec::new();
    let mut peak_lmax: Option<f64> = None;

    // 1. Special Use Airspace (SUA) + TIMESOFUSE schedules
    let mut sua_list = Vec::new();
    if let Ok(feats) = sua_res {
        let mut seen = HashSet::new();
        for f in &feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let name = clean_str(get_ci(a, "NAME")).unwrap_or_default();
            if name.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            let Some(geom) = f.get("geometry") else { continue };
            let Some((dist_m, inside)) = polygon_distance_and_inside(lat, lon, geom) else {
                continue;
            };
            let dist_mi = round_n(dist_m / MILE_M, 2);
            let type_code = clean_str(get_ci(a, "TYPE_CODE")).unwrap_or_else(|| "SUA".to_string());
            let times_of_use = clean_str(get_ci(a, "TIMESOFUSE"));
            let floor = clean_str(get_ci(a, "LOWER_VAL")).unwrap_or_else(|| "SFC".to_string());
            let ceiling = clean_str(get_ci(a, "UPPER_VAL")).unwrap_or_default();
            let agent = clean_str(get_ci(a, "CONT_AGENT"));

            if inside || dist_mi <= 5.0 {
                let sua_ldn = if inside { 44.0 } else { 38.0 };
                let sua_lmax = if inside { 72.0 } else { 62.0 };
                ldn_components.push(sua_ldn);
                peak_lmax = Some(peak_lmax.map_or(sua_lmax, |p| p.max(sua_lmax)));
            }

            if dist_mi <= 35.0 {
                if let Some(gj) = simplified_arcgis_polygon_geojson(geom, 90) {
                    map_features.push(json!({
                        "type": "Feature",
                        "geometry": gj,
                        "properties": {
                            "featureKind": "sua",
                            "name": name,
                            "typeCode": type_code,
                            "timesOfUse": times_of_use,
                            "floor": floor,
                            "ceiling": ceiling,
                            "inside": inside,
                            "distanceMi": dist_mi,
                            "color": "#9333ea",
                            "dash": "5 4",
                            "popup": format!(
                                "SUA {type_code} · {} · Alt {floor}–{ceiling} · Schedule: {}",
                                if inside { "Overhead".to_string() } else { fmt_mi(dist_mi) },
                                times_of_use.as_deref().unwrap_or("See NOTAM")
                            ),
                        }
                    }));
                }
            }

            sua_list.push(json!({
                "name": name,
                "typeCode": type_code,
                "timesOfUse": times_of_use,
                "floor": floor,
                "ceiling": ceiling,
                "controllingAgency": agent,
                "inside": inside,
                "distanceMi": dist_mi,
            }));
        }
    }
    sua_list.sort_by(|a, b| {
        let da = val_f64(a.get("distanceMi")).unwrap_or(f64::INFINITY);
        let db = val_f64(b.get("distanceMi")).unwrap_or(f64::INFINITY);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    // 2. Military Training Routes (MTR)
    let mut mtr_list = Vec::new();
    if let Ok(feats) = mtr_res {
        let mut seen = HashSet::new();
        for f in &feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let ident = clean_str(get_ci(a, "IDENT")).unwrap_or_default();
            let mtr_type = clean_str(get_ci(a, "MTR_TYPE"))
                .or_else(|| clean_str(get_ci(a, "ROUTETYPE")))
                .unwrap_or_else(|| "MTR".to_string());
            let Some(geom) = f.get("geometry") else { continue };
            let Some(dist_m) = polyline_distance_m(lat, lon, geom) else { continue };
            let dist_nm = dist_m / NM_M;
            let wl = val_f64(get_ci(a, "WIDTHLEFT")).unwrap_or(2.0);
            let wr = val_f64(get_ci(a, "WIDTHRIGHT")).unwrap_or(2.0);
            let half_w = wl.max(wr).max(1.0);
            let floor_ft = val_f64(get_ci(a, "LOWER_VAL")).unwrap_or(1_500.0);
            let ceil_ft = val_f64(get_ci(a, "UPPER_VAL"));

            let route_label = format!("{mtr_type}-{ident}");
            if !seen.insert(route_label.clone()) {
                continue;
            }
            let (in_corridor, ldnmr, jet_lmax) = model_mtr_corridor_noise(dist_nm, half_w, floor_ft);
            if let Some(l) = ldnmr {
                ldn_components.push(l);
            }
            if let Some(m) = jet_lmax {
                peak_lmax = Some(peak_lmax.map_or(m, |p| p.max(m)));
            }

            if dist_nm <= 25.0 {
                if let Some(gj) = arcgis_to_geojson_geometry(geom) {
                    map_features.push(json!({
                        "type": "Feature",
                        "geometry": gj,
                        "properties": {
                            "featureKind": "mtr",
                            "name": route_label,
                            "inCorridor": in_corridor,
                            "halfWidthNm": round_n(half_w, 1),
                            "floorFt": floor_ft.round() as i64,
                            "ceilingFt": ceil_ft.map(|c| c.round() as i64),
                            "distanceNm": round_n(dist_nm, 2),
                            "estLmaxDba": jet_lmax,
                            "color": "#c026d3",
                            "popup": format!(
                                "Military Training Route · {:.1} NM away (±{:.1} NM width) · Floor {} ft",
                                dist_nm,
                                half_w,
                                floor_ft.round() as i64
                            ),
                        }
                    }));
                }
            }

            mtr_list.push(json!({
                "routeId": route_label,
                "mtrType": mtr_type,
                "inCorridor": in_corridor,
                "corridorHalfWidthNm": round_n(half_w, 1),
                "floorFt": floor_ft.round() as i64,
                "ceilingFt": ceil_ft.map(|c| c.round() as i64),
                "altDescription": clean_str(get_ci(a, "ALT_DESC")),
                "distanceNm": round_n(dist_nm, 2),
                "distanceMi": round_n(dist_m / MILE_M, 2),
                "estLdnmrDba": ldnmr,
                "peakJetLmaxDba": jet_lmax,
            }));
        }
    }
    mtr_list.sort_by(|a, b| {
        let da = val_f64(a.get("distanceMi")).unwrap_or(f64::INFINITY);
        let db = val_f64(b.get("distanceMi")).unwrap_or(f64::INFINITY);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    // 3. DoD Security Airspace & Installations
    let mut installations = Vec::new();
    let mut seen_inst = HashSet::new();
    if let Ok(feats) = dod_res {
        for f in &feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let base = clean_str(get_ci(a, "Base"))
                .or_else(|| clean_str(get_ci(a, "Facility")))
                .unwrap_or_default();
            if base.is_empty() || !seen_inst.insert(base.to_ascii_uppercase()) {
                continue;
            }
            let Some(geom) = f.get("geometry") else { continue };
            let Some((dist_m, inside)) = polygon_distance_and_inside(lat, lon, geom) else {
                continue;
            };
            let dist_mi = round_n(dist_m / MILE_M, 2);
            let branch = clean_str(get_ci(a, "Branch"))
                .or_else(|| clean_str(get_ci(a, "Proponent")));
            let floor = clean_str(get_ci(a, "Floor"));
            let ceiling = clean_str(get_ci(a, "Ceiling"));
            if dist_mi <= 20.0 {
                if let Some(gj) = simplified_arcgis_polygon_geojson(geom, 80) {
                    map_features.push(json!({
                        "type": "Feature",
                        "geometry": gj,
                        "properties": {
                            "featureKind": "dod_airspace",
                            "name": base,
                            "branch": branch,
                            "inside": inside,
                            "distanceMi": dist_mi,
                            "color": "#be123c",
                            "popup": format!(
                                "{} · {}{}",
                                branch.as_deref().unwrap_or("DoD / Federal"),
                                if inside { "Inside boundary".to_string() } else { fmt_mi(dist_mi) },
                                ceiling.as_deref().map(|c| format!(" · Ceiling {c}")).unwrap_or_default()
                            ),
                        }
                    }));
                }
            }
            installations.push(json!({
                "name": base,
                "branch": branch,
                "floor": floor,
                "ceiling": ceiling,
                "inside": inside,
                "distanceMi": dist_mi,
            }));
        }
    }
    if let Ok(feats) = bases_res {
        for f in &feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let site = clean_str(get_ci(a, "SITE_NAME")).unwrap_or_default();
            if site.is_empty() || !seen_inst.insert(site.to_ascii_uppercase()) {
                continue;
            }
            let dist_mi = f
                .get("geometry")
                .and_then(|g| polygon_distance_and_inside(lat, lon, g).map(|(dm, _)| round_n(dm / MILE_M, 2)))
                .or_else(|| {
                    f.get("geometry")
                        .and_then(arcgis_representative_point)
                        .map(|(la, lo)| round_n(haversine_mi(lat, lon, la, lo), 2))
                });
            let Some(d_mi) = dist_mi else { continue };
            installations.push(json!({
                "name": site,
                "branch": clean_str(get_ci(a, "COMPONENT")),
                "status": clean_str(get_ci(a, "OPER_STAT")),
                "inside": d_mi == 0.0,
                "distanceMi": d_mi,
            }));
        }
    }
    installations.sort_by(|a, b| {
        let da = val_f64(a.get("distanceMi")).unwrap_or(f64::INFINITY);
        let db = val_f64(b.get("distanceMi")).unwrap_or(f64::INFINITY);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    // 4. Federal / Military Airfield Runway Approach Corridors (e.g. KNUQ Moffett Federal Airfield)
    let mut mil_airfields = Vec::new();
    if let Ok(ap_feats) = airports_res {
        let mut mil_ids = Vec::new();
        let mut mil_names = HashMap::new();
        for f in &ap_feats {
            let a = f.get("attributes").unwrap_or(&Value::Null);
            let Some(ident) = clean_str(get_ci(a, "IDENT")).and_then(|s| sanitize_alnum_code(&s))
            else {
                continue;
            };
            let name = clean_str(get_ci(a, "NAME")).unwrap_or_default();
            let mil_code = clean_str(get_ci(a, "MIL_CODE"));
            if is_federal_or_military_airport(&name, mil_code.as_deref()) && mil_ids.len() < 6 {
                mil_names.insert(ident.clone(), name);
                mil_ids.push(ident);
            }
        }
        if !mil_ids.is_empty() {
            let in_clause = mil_ids
                .iter()
                .map(|id| format!("'{id}'"))
                .collect::<Vec<_>>()
                .join(",");
            let params = vec![
                ("where", format!("ARPT_ID IN ({in_clause})")),
                (
                    "outFields",
                    "ARPT_ID,RWY_END_ID,TRUE_ALIGNMENT,VISUAL_GLIDE_PATH_ANGLE,THR_CROSSING_HGT,LAT_DECIMAL,LONG_DECIMAL".to_string(),
                ),
                ("returnGeometry", "false".to_string()),
            ];
            if let Ok(rj) = http
                .arcgis_query(&eps.runway_ends_layer, &params, REQ_TIMEOUT_MS)
                .await
            {
                if let Some(r_feats) = rj.get("features").and_then(Value::as_array) {
                    for rf in r_feats {
                        let a = rf.get("attributes").unwrap_or(&Value::Null);
                        let Some(arpt) =
                            clean_str(get_ci(a, "ARPT_ID")).and_then(|s| sanitize_alnum_code(&s))
                        else {
                            continue;
                        };
                        let Some(rwy_end) = clean_str(get_ci(a, "RWY_END_ID")) else {
                            continue;
                        };
                        let (Some(r_lat), Some(r_lon), Some(hdg)) = (
                            val_f64(get_ci(a, "LAT_DECIMAL")),
                            val_f64(get_ci(a, "LONG_DECIMAL")),
                            val_f64(get_ci(a, "TRUE_ALIGNMENT")),
                        ) else {
                            continue;
                        };
                        let out_brg = (hdg + 180.0).rem_euclid(360.0);
                        let (along_m, cross_m) =
                            ray_along_and_cross_m(r_lat, r_lon, out_brg, lat, lon);
                        let along_mi = along_m / MILE_M;
                        let cross_mi = cross_m / MILE_M;
                        let gpa = val_f64(get_ci(a, "VISUAL_GLIDE_PATH_ANGLE")).unwrap_or(3.0);
                        let tch = val_f64(get_ci(a, "THR_CROSSING_HGT")).unwrap_or(50.0);
                        let ap_name = mil_names.get(&arpt).cloned().unwrap_or_else(|| arpt.clone());
                        let is_fighter = ap_name.to_ascii_uppercase().contains("AFB")
                            || ap_name.to_ascii_uppercase().contains("NAS")
                            || ap_name.to_ascii_uppercase().contains("MCAS");
                        let sel_1000 = if is_fighter { 104.0 } else { 96.0 };

                        if let Some((ldn, lmax, alt_ft, _)) = model_faa_aedt_runway_corridor(
                            along_mi, cross_mi, gpa, tch, sel_1000, 8.0, 1.0,
                        ) {
                            ldn_components.push(ldn);
                            peak_lmax = Some(peak_lmax.map_or(lmax, |p| p.max(lmax)));
                            let end_12mi = dest_point_latlon(r_lat, r_lon, out_brg, 12.0 * MILE_M);
                            map_features.push(json!({
                                "type": "Feature",
                                "geometry": {
                                    "type": "LineString",
                                    "coordinates": [[r_lon, r_lat], [end_12mi.1, end_12mi.0]]
                                },
                                "properties": {
                                    "featureKind": "military_runway_corridor",
                                    "name": format!("{ap_name} ({arpt}) Rwy {rwy_end}"),
                                    "crossTrackMi": round_n(cross_mi, 2),
                                    "estGlidepathAltAglFt": alt_ft,
                                    "estLdnDba": ldn,
                                    "peakLmaxDba": lmax,
                                    "color": "#e11d48",
                                    "dash": "7 4",
                                    "popup": format!(
                                        "Federal/DoD airfield approach · {:.2} mi cross-track · ~{} ft AGL · Peak ~{:.0} dBA",
                                        cross_mi, alt_ft, lmax
                                    ),
                                }
                            }));
                            mil_airfields.push(json!({
                                "airportId": arpt,
                                "airportName": ap_name,
                                "runwayEndId": rwy_end,
                                "alongTrackMi": round_n(along_mi, 2),
                                "crossTrackMi": round_n(cross_mi, 2),
                                "estGlidepathAltAglFt": alt_ft,
                                "estLdnDba": ldn,
                                "peakLmaxDba": lmax,
                            }));
                        }
                    }
                }
            }
        }
    }

    let est_ldn = energetic_sum_dba(&ldn_components);
    let mut map_layers = Vec::new();
    if !map_features.is_empty() {
        map_layers.push(json!({
            "id": "noise-military-airspace",
            "title": "Special Use Airspace (SUA), MTR jet corridors & DoD airfields",
            "kind": "mixed",
            "color": "#9333ea",
            "features": map_features,
        }));
    }

    Ok(Some(json!({
        "extensionId": MILITARY_AIRSPACE_EXT_ID,
        "fetchedAt": now_iso(),
        "model": "DoD AICUZ / MR_NMAP Screening & FAA SUA Schedule Windows",
        "estLdnDba": est_ldn,
        "peakJetLmaxDba": peak_lmax,
        "specialUseAirspace": sua_list.into_iter().take(8).collect::<Vec<_>>(),
        "militaryTrainingRoutes": mtr_list.into_iter().take(8).collect::<Vec<_>>(),
        "installations": installations.into_iter().take(8).collect::<Vec<_>>(),
        "federalAirfieldCorridors": mil_airfields,
        "referenceUrl": FAA_AERO_DATA_REF_URL,
        "mapLayers": map_layers,
    })))
}

pub struct MilitaryAirspaceNoiseAdapter;

impl DataExtensionAdapter for MilitaryAirspaceNoiseAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        build_manifest_from_embedded(
            MILITARY_MANIFEST_YAML,
            MILITARY_AIRSPACE_EXT_ID,
            "FAA Special Use Airspace Schedules, MTR Corridors & DoD Airfields",
            "Keyless FAA Special Use Airspace (SUA) polygons with TIMESOFUSE schedules, Military Training Routes (MTR), DoD airspace/bases, and AICUZ screening.",
            234,
            604_800,
            ShareScope::Geohash5,
            86_400,
            FAA_AERO_DATA_REF_URL,
            "https://sua.faa.gov/",
            MILITARY_AIRSPACE_SOURCE_NAME,
            FAA_AERO_DATA_REF_URL,
            FAA_AERO_DATA_REF_URL,
        )
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
            let Some((lat, lon)) = valid_us_coords(query.lat, query.lon, query) else {
                return Ok(Vec::new());
            };
            let started = Instant::now();
            let eps = MilitaryNoiseEndpoints::default();
            let Some(payload_val) =
                fetch_military_airspace_noise_with_endpoints(http, &eps, lat, lon).await?
            else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = payload_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            Ok(vec![ExtensionEnvelopeProto {
                extension_id: MILITARY_AIRSPACE_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainEnvironmental as i32,
                priority: 234,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: MILITARY_AIRSPACE_EXT_ID.to_string(),
                    source_name: MILITARY_AIRSPACE_SOURCE_NAME.to_string(),
                    source_url: FAA_AERO_DATA_REF_URL.to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&payload_val)?)),
            }])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use serde_json::Map;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn offline_and_non_us_queries_return_empty_without_fabricating_noise() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let us_query = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            state: "CA".to_string(),
            ..Default::default()
        };
        for adapter in all_noise_feed_adapters() {
            let out = adapter.execute_async(&us_query, &http, &env).await.unwrap();
            assert!(
                out.is_empty(),
                "{} must return empty offline without fabricating metrics",
                adapter.manifest().extension_id
            );
        }

        let live_http = HttpClient::new(None);
        let mut extra_params = HashMap::new();
        extra_params.insert("country".to_string(), "CA".to_string());
        let non_us_query = ExtensionQueryProto {
            lat: Some(43.6532),
            lon: Some(-79.3832),
            extra_params,
            ..Default::default()
        };
        for adapter in all_noise_feed_adapters() {
            let out = adapter
                .execute_async(&non_us_query, &live_http, &env)
                .await
                .unwrap();
            assert!(out.is_empty());
        }
    }

    #[test]
    fn acoustic_models_match_campbell_ca_reference_benchmarks() {
        // 1. FAA AEDT glidepath at Campbell under NUQ Rwy 32R (9.58 mi along, 0.76 mi cross, 3.0 deg GPA)
        let (av_ldn, av_lmax, alt_agl, slant_ft) =
            model_faa_aedt_runway_corridor(9.58, 0.76, 3.0, 55.0, 94.0, 35.0, 3.0)
                .expect("NUQ 32R glidepath");
        assert!(
            (2650..=2760).contains(&alt_agl),
            "expected ~2706 ft AGL, got {alt_agl}"
        );
        assert!(slant_ft > 4500 && slant_ft < 5200);
        assert!(
            (40.0..=47.0).contains(&av_ldn),
            "expected ~43 dBA Ldn, got {av_ldn}"
        );
        assert!(
            (60.0..=67.0).contains(&av_lmax),
            "expected ~63 dBA peak flyover Lmax, got {av_lmax}"
        );

        // 2. FHWA TNM 2.5 Campbell corridors: San Tomas Expy (24,378 AADT, 165 m), Virginia Ave (4,274, 62 m), SR-17 (132,000, 1520 m)
        let (st_ldn, st_pk, _) =
            model_fhwa_tnm_corridor(24_378.0, 1_967.0, 714.0, 45.0, 165.0, None).unwrap();
        let (va_ldn, _, _) =
            model_fhwa_tnm_corridor(4_274.0, 120.0, 20.0, 30.0, 62.0, None).unwrap();
        let (sr17_ldn, _, _) =
            model_fhwa_tnm_corridor(132_000.0, 9_771.0, 7_749.0, 65.0, 1_520.0, Some(0.084))
                .unwrap();
        let comb_road = energetic_sum_dba(&[st_ldn, va_ldn, sr17_ldn]).unwrap();
        assert!(
            (54.0..=61.0).contains(&comb_road),
            "expected ~57.9 dBA composite road Ldn, got {comb_road} (st={st_ldn}, va={va_ldn}, sr17={sr17_ldn})"
        );
        assert!(st_pk >= st_ldn - 2.0);

        // 3. FTA wayside + 49 CFR 222 Quiet Zone horn suppression vs active horn
        let wayside = model_fta_wayside_rail_ldn("light_rail", 55.0, 78.0, 22.0, 1_450.0).unwrap();
        assert!(wayside < 45.0, "expected <45 dBA LRT wayside at 1450m, got {wayside}");

        let (qz, qz_lmax, qz_ldn) =
            model_crossing_horn_noise("light_rail", Some("24 hr"), true, 78.0, 22.0, 350.0);
        assert!(qz, "24 hr WhistleBan must be flagged as Quiet Zone");
        assert!(qz_lmax.is_none() && qz_ldn.is_none(), "Quiet Zone must suppress horn impulse");

        let (no_qz, active_lmax, active_ldn) =
            model_crossing_horn_noise("freight", Some("No"), true, 14.0, 6.0, 250.0);
        assert!(!no_qz);
        assert!(active_lmax.unwrap() > 85.0, "active freight horn at 250m should exceed 85 dBA Lmax");
        assert!(active_ldn.unwrap() > 55.0);

        // 4. Military MTR low-level corridor screening
        let (in_corr, ldnmr, jet_lmax) = model_mtr_corridor_noise(1.2, 3.0, 1_000.0);
        assert!(in_corr);
        assert!(jet_lmax.unwrap() >= 95.0);
        assert!(ldnmr.is_some());
    }

    #[tokio::test]
    async fn mock_endpoints_populate_all_four_noise_feeds_and_geojson_layers() {
        let app = Router::new()
            .route(
                "/airports/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "IDENT": "NUQ",
                                "ICAO_ID": "KNUQ",
                                "NAME": "MOFFETT FEDERAL AIRFIELD",
                                "TYPE_CODE": "AD",
                                "ELEVATION": 32.0,
                                "IAPEXISTS": 1,
                                "MIL_CODE": "MIL"
                            },
                            "geometry": { "x": -122.049, "y": 37.416 }
                        }]
                    }))
                }),
            )
            .route(
                "/runways/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "ARPT_ID": "NUQ",
                                "RWY_ID": "14L/32R",
                                "RWY_END_ID": "32R",
                                "TRUE_ALIGNMENT": 337.0,
                                "ILS_TYPE": "ILS/DME",
                                "VISUAL_GLIDE_PATH_ANGLE": 3.0,
                                "THR_CROSSING_HGT": 55.0,
                                "LAT_DECIMAL": 37.406,
                                "LONG_DECIMAL": -122.044
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/airspace/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "IDENT": "SJC",
                                "NAME": "SAN JOSE CLASS C",
                                "CLASS": "C",
                                "LOWER_VAL": "2500",
                                "UPPER_VAL": "4000"
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/adsb/v2/point/:lat/:lon/:rad",
                get(|| async {
                    Json(json!({
                        "ac": [{
                            "hex": "a12345",
                            "flight": "QXE2432 ",
                            "t": "E75L",
                            "alt_baro": 4250,
                            "gs": 182.0,
                            "track": 318.0,
                            "baro_rate": -960,
                            "lat": 37.301,
                            "lon": -121.992,
                            "dst": 2.1
                        }]
                    }))
                }),
            )
            .route(
                "/opensky/api/states/all",
                get(|| async {
                    Json(json!({
                        "states": [[
                            "abc999",
                            "UAL28   ",
                            "United States",
                            1700000000,
                            1700000000,
                            -121.970,
                            37.295,
                            1295.0,
                            false,
                            92.0,
                            318.0,
                            -4.5
                        ]]
                    }))
                }),
            )
            .route(
                "/hpms_CA/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "route_id": "SCL_CMB_SAN TOMAS EXWY",
                                "f_system": 3,
                                "through_lanes": 8,
                                "aadt": 24378,
                                "aadt_single_unit": 1967,
                                "aadt_combination": 714,
                                "speed_limit": 45
                            },
                            "geometry": {
                                "paths": [[[-121.9635, 37.2810], [-121.9635, 37.2840]]]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/caltrans/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "RTE": "017",
                                "DESCRIPTION": "CAMDEN AVE",
                                "AHEAD_AADT": 131000,
                                "AHEAD_PEAK_HOUR": 11100
                            },
                            "geometry": { "x": -121.95, "y": 37.28 }
                        }]
                    }))
                }),
            )
            .route(
                "/xings/query",
                get(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "CrossingID": "925811F",
                                    "STREET": "CIVIC CENTER DR",
                                    "RailroadCode": "VTA",
                                    "ReportingAgencyName": "Santa Clara VTA",
                                    "CrossingPosition": "At Grade",
                                    "WhistleBan": "24 hr",
                                    "TotalDaylightThruTrains": 78,
                                    "TotalNighttimeThruTrains": 22,
                                    "MaximumTimetableSpeed": 55,
                                    "NumberOfTransitTracks": 2,
                                    "URL": "https://safetydata.fra.dot.gov/OfficeofSafety/PublicSite/Crossing/Crossing.aspx"
                                },
                                "geometry": { "x": -121.944, "y": 37.287 }
                            },
                            {
                                "attributes": {
                                    "CrossingID": "111222A",
                                    "STREET": "INDUSTRIAL WAY",
                                    "RailroadCode": "UP",
                                    "CrossingPosition": "At Grade",
                                    "WhistleBan": "No",
                                    "TotalDaylightThruTrains": 8,
                                    "TotalNighttimeThruTrains": 4,
                                    "MaximumTimetableSpeed": 40
                                },
                                "geometry": { "x": -121.952, "y": 37.279 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/raillines/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "RROWNER1": "UP",
                                "SUBDIV": "VASONA IND LD",
                                "TRACKS": 1
                            },
                            "geometry": {
                                "paths": [[[-121.945, 37.280], [-121.944, 37.290]]]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/transit/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "agency_name": "VTA",
                                "route_short_name": "Green Line",
                                "route_type": "0"
                            },
                            "geometry": {
                                "paths": [[[-121.945, 37.280], [-121.944, 37.290]]]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/sua/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "NAME": "A-682",
                                "TYPE_CODE": "A",
                                "TIMESOFUSE": "0800 - 2100, MON - FRI",
                                "LOWER_VAL": "SFC",
                                "UPPER_VAL": "6000"
                            },
                            "geometry": {
                                "rings": [[
                                    [-122.05, 37.20],
                                    [-121.85, 37.20],
                                    [-121.85, 37.35],
                                    [-122.05, 37.35],
                                    [-122.05, 37.20]
                                ]]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/mtr/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "IDENT": "203",
                                "MTR_TYPE": "IR",
                                "LOWER_VAL": 1500,
                                "UPPER_VAL": 5000,
                                "WIDTHLEFT": 3,
                                "WIDTHRIGHT": 3
                            },
                            "geometry": {
                                "paths": [[[-122.00, 37.26], [-121.92, 37.30]]]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/dod/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "Branch": "USAF",
                                "Base": "Moffett ANGB",
                                "Floor": "SFC",
                                "Ceiling": "2500"
                            },
                            "geometry": {
                                "rings": [[
                                    [-122.06, 37.40],
                                    [-122.03, 37.40],
                                    [-122.03, 37.43],
                                    [-122.06, 37.43],
                                    [-122.06, 37.40]
                                ]]
                            }
                        }]
                    }))
                }),
            )
            .route("/bases/query", get(|| async { Json(json!({ "features": [] })) }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let http = HttpClient::new(None);
        let (lat, lon) = (37.282147, -121.961797);

        let flight = fetch_faa_flight_noise_with_endpoints(
            &http,
            &FlightNoiseEndpoints {
                airports_layer: format!("{base}/airports"),
                runway_ends_layer: format!("{base}/runways"),
                class_airspace_layer: format!("{base}/airspace"),
                adsb_lol_base: format!("{base}/adsb"),
                opensky_base: format!("{base}/opensky"),
            },
            lat,
            lon,
            "CA",
        )
        .await
        .unwrap()
        .expect("flight noise payload");
        assert!(flight["estLdnDba"].as_f64().unwrap() > 35.0);
        assert_eq!(flight["liveAdsb"]["airborneCount"], json!(1));
        assert_eq!(flight["liveAdsb"]["provider"], json!("adsb.lol"));
        assert_eq!(flight["mapLayers"][0]["id"], json!("noise-flight-corridors"));

        let flight_fb = fetch_faa_flight_noise_with_endpoints(
            &http,
            &FlightNoiseEndpoints {
                airports_layer: format!("{base}/airports"),
                runway_ends_layer: format!("{base}/runways"),
                class_airspace_layer: format!("{base}/airspace"),
                adsb_lol_base: format!("{base}/adsb_unavailable_404"),
                opensky_base: format!("{base}/opensky"),
            },
            lat,
            lon,
            "CA",
        )
        .await
        .unwrap()
        .expect("flight fallback payload");
        assert_eq!(flight_fb["liveAdsb"]["provider"], json!("opensky-network.org"));
        assert_eq!(flight_fb["liveAdsb"]["airborneCount"], json!(1));

        let hw = fetch_highway_traffic_noise_with_endpoints(
            &http,
            &HighwayNoiseEndpoints {
                state_hpms_template: format!("{base}/hpms_{{STATE}}"),
                national_hpms_fallback_layer: format!("{base}/hpms_CA"),
                caltrans_aadt_layer: format!("{base}/caltrans"),
            },
            lat,
            lon,
            "CA",
        )
        .await
        .unwrap()
        .expect("highway noise payload");
        assert!(hw["estLdnDba"].as_f64().unwrap() > 50.0);
        assert_eq!(hw["mapLayers"][0]["id"], json!("noise-highway-aadt"));
        assert_eq!(hw["caltransPeakHours"].as_array().unwrap().len(), 1);

        let rail = fetch_rail_transit_noise_with_endpoints(
            &http,
            &RailNoiseEndpoints {
                grade_crossings_layer: format!("{base}/xings"),
                rail_lines_layer: format!("{base}/raillines"),
                transit_routes_layer: format!("{base}/transit"),
            },
            lat,
            lon,
            "CA",
        )
        .await
        .unwrap()
        .expect("rail noise payload");
        assert_eq!(rail["quietZoneCrossingCount"], json!(1));
        assert_eq!(rail["activeHornCrossingCount"], json!(1));
        assert!(rail["peakHornLmaxDba"].as_f64().is_some());
        let layer_ids: Vec<&str> = rail["mapLayers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|l| l["id"].as_str())
            .collect();
        assert!(layer_ids.contains(&"noise-rail-lines"));
        assert!(layer_ids.contains(&"noise-rail-crossings"));

        let mil = fetch_military_airspace_noise_with_endpoints(
            &http,
            &MilitaryNoiseEndpoints {
                sua_layer: format!("{base}/sua"),
                mtr_layer: format!("{base}/mtr"),
                dod_airspace_layer: format!("{base}/dod"),
                military_bases_layer: format!("{base}/bases"),
                airports_layer: format!("{base}/airports"),
                runway_ends_layer: format!("{base}/runways"),
            },
            lat,
            lon,
        )
        .await
        .unwrap()
        .expect("military airspace payload");
        assert!(mil["estLdnDba"].as_f64().is_some());
        assert_eq!(
            mil["specialUseAirspace"][0]["timesOfUse"],
            json!("0800 - 2100, MON - FRI")
        );
        assert_eq!(mil["mapLayers"][0]["id"], json!("noise-military-airspace"));

        // Verify fusion into environment.data.noise and ExtensionHub registration
        let snap = crate::extensions::ExtensionHub::global().snapshot();
        for ext_id in [
            FAA_FLIGHT_EXT_ID,
            HIGHWAY_TRAFFIC_EXT_ID,
            RAIL_TRANSIT_EXT_ID,
            MILITARY_AIRSPACE_EXT_ID,
        ] {
            assert!(
                snap.is_extension_enabled(ext_id),
                "{ext_id} must be registered and enabled by default in ExtensionHub"
            );
        }

        let mut base_noise = json!({
            "checked": true,
            "estLdnDb": 41.0,
            "roadDb": 41.0,
            "aviationDb": 41.0,
            "railDb": 41.0,
            "atProperty": {
                "all": { "label": "<45 dB (below mapped threshold)", "lowDb": null, "highDb": 45 },
                "road": { "label": "<45 dB (below mapped threshold)", "lowDb": null, "highDb": 45 },
                "aviation": { "label": "<45 dB (below mapped threshold)", "lowDb": null, "highDb": 45 },
                "rail": { "label": "<45 dB (below mapped threshold)", "lowDb": null, "highDb": 45 }
            }
        });
        let mut ext_map = Map::new();
        ext_map.insert("faaFlight".into(), flight);
        ext_map.insert("highwayTraffic".into(), hw);
        ext_map.insert("railTransit".into(), rail);
        ext_map.insert("militaryAirspace".into(), mil);
        crate::modules::environment::fuse_noise_extensions_into_noise(
            &mut base_noise,
            "CA",
            &ext_map,
        );
        assert!(base_noise["estLdnDb"].as_f64().unwrap() > 52.0);
        assert!(base_noise["peakLmaxDb"].as_f64().unwrap() > 60.0);
        assert!(base_noise["btsBaseline"]["estLdnDb"].as_f64().is_some());
        assert!(base_noise["btsTiles"]["aviation"]["tileUrl"].as_str().is_some());
    }
}
