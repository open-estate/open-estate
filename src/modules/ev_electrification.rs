//! Public EV DC Fast Chargers & Home Electrification Economics (`ev_fast_chargers_electrification`).
//!
//! Queries keyless zero-bottleneck USDOT BTS / DOE AFDC `Alternative_Fueling_Stations` ArcGIS
//! FeatureServer (`fuel_type_code='ELEC' AND status_code='E' AND access_code='public'`), Esri
//! hosted OSM North America `OSM_NA_Amenities` (`amenity='charging_station'`), and optional BYOK
//! `NREL_API_KEY` on `developer.nlr.gov` (strictly rejecting `DEMO_KEY`).
//!
//! Combines 5-year `HDD65`/`CDD65` daily-mean climate normals, local electric (`$/kWh` blended &
//! off-peak TOU) and natural-gas (`$/therm`) tariffs, and `permit_mep_extractor` (`MepIntelProto`)
//! 200A panel / UL load-splitter readiness into annual heating/cooling kWh & therms, Home Level-2
//! EV `$/1,000 mi`, DCFC NACS/CCS/CHAdeMO station summaries, and the `ev-chargers` GeoJSON layer.

use anyhow::{anyhow, Result};
use chrono::{Datelike, Utc};
use futures::future::BoxFuture;
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use url::Url;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind, ExecutionMode,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    GeoScopeProto, LicenseClass, ManifestSourceAttributionProto, MeteringSpecProto, MepIntelProto,
    PropertyFactsProto, ProvenanceTier, RateLimitSpecProto, ShareScope, SharingSpecProto,
    SourceAttributionProto,
};
use crate::modules::live_tariffs::{
    fetch_live_climate_degree_days, resolve_live_utility_and_climate_bundle, LiveUtilityQuery,
};
use crate::property::permit_pipeline::extract_mep_intel;
use crate::registry::utilities::{resolve_electric_utility, resolve_gas_utility, UtilityLookupInput};
use crate::shared::{
    bearing_deg, compass8, haversine_m, meters_to_miles, now_iso, round_n, EnvConfig, HttpClient,
};

pub const EV_ELECTRIFICATION_EXT_ID: &str = "ev_fast_chargers_electrification";
pub const BTS_AFDC_FEATURE_SERVER_URL: &str =
    "https://services.arcgis.com/xOi1kZaI0eWDREZv/arcgis/rest/services/Alternative_Fueling_Stations/FeatureServer/0";
pub const ESRI_OSM_AMENITIES_URL: &str =
    "https://services6.arcgis.com/Do88DoK2xjTUCXd1/arcgis/rest/services/OSM_NA_Amenities/FeatureServer/0";
pub const NLR_ALT_FUEL_NEAREST_URL: &str =
    "https://developer.nlr.gov/api/alt-fuel-stations/v1/nearest.json";
pub const OPEN_METEO_ARCHIVE_URL: &str = "https://archive-api.open-meteo.com/v1/archive";

pub const AFDC_SOURCE_NAME: &str =
    "U.S. DOT Bureau of Transportation Statistics / U.S. DOE Alternative Fuels Data Center (AFDC)";
pub const AFDC_HOMEPAGE_URL: &str = "https://afdc.energy.gov/stations";
pub const NLR_DOCS_URL: &str =
    "https://developer.nlr.gov/docs/transportation/alt-fuel-stations-v1/";

pub const EV_CACHE_TTL_SECS: u64 = 1_209_600; // 14 days
const EV_REQ_TIMEOUT_MS: u64 = 8_000;
const AFDC_SEARCH_RADIUS_M: f64 = 8_047.0; // 5.0 miles
const OSM_CHARGER_RADIUS_M: f64 = 4_828.0; // 3.0 miles
const DEDUP_DISTANCE_M: f64 = 65.0;
const DEDUP_SAME_NAME_M: f64 = 125.0;
const MAX_REPORTED_STATIONS: usize = 25;
const EV_KWH_PER_1000_MI: f64 = 303.0; // 3.30 mi/kWh wall-to-wheels incl. Level-2 EVSE efficiency

static CLIMATE_5YR_CACHE: Lazy<Cache<String, FiveYearClimateNormals>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(2_048)
        .time_to_live(Duration::from_secs(30 * 86_400))
        .build()
});

#[derive(Debug, Clone)]
pub struct EvElectrificationEndpoints {
    pub afdc_layer_url: String,
    pub osm_amenities_layer_url: String,
    pub nlr_nearest_url: String,
    pub open_meteo_archive_url: String,
}

impl Default for EvElectrificationEndpoints {
    fn default() -> Self {
        Self {
            afdc_layer_url: BTS_AFDC_FEATURE_SERVER_URL.to_string(),
            osm_amenities_layer_url: ESRI_OSM_AMENITIES_URL.to_string(),
            nlr_nearest_url: NLR_ALT_FUEL_NEAREST_URL.to_string(),
            open_meteo_archive_url: OPEN_METEO_ARCHIVE_URL.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FiveYearClimateNormals {
    pub hdd65: i64,
    pub cdd65: i64,
    pub normal_period: String,
    pub years_sampled: usize,
    pub source_name: String,
    pub source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvStationItem {
    pub name: String,
    pub network: String,
    pub charger_tier: String, // "dcfc" | "level2" | "public_evse"
    pub is_dc_fast: bool,
    pub dc_fast_ports: Option<i64>,
    pub level2_ports: Option<i64>,
    pub connectors: Vec<String>,
    pub street_address: Option<String>,
    pub city: Option<String>,
    pub pricing: Option<String>,
    pub access_hours: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub distance_m: f64,
    pub distance_mi: f64,
    pub direction: String,
    pub source: String, // "doe_afdc_bts" | "nlr_afdc_api" | "osm_na_amenities"
    #[serde(skip)]
    pub priority_rank: u8,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedTariffInputs {
    pub electric_carrier: Option<String>,
    pub electric_schedule: Option<String>,
    pub electric_rate_kwh: Option<f64>,
    pub off_peak_kwh: Option<f64>,
    pub peak_kwh: Option<f64>,
    pub electric_source: Option<String>,
    pub gas_carrier: Option<String>,
    pub gas_schedule: Option<String>,
    pub gas_rate_therm: Option<f64>,
    pub gas_source: Option<String>,
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

fn afdc_reference_url(lat: f64, lon: f64) -> String {
    format!("https://afdc.energy.gov/stations#/find/nearest?location={lat:.5},{lon:.5}&fuel=ELEC")
}

/// Validates an optional BYOK `NREL_API_KEY` for `developer.nlr.gov`.
/// Strictly rejects empty strings and the shared rate-limited placeholder key (`10 req/hr/IP`).
pub fn sanitize_nrel_api_key(raw: Option<&str>) -> Option<String> {
    let clean = raw?.trim();
    if clean.is_empty() || clean.eq_ignore_ascii_case("demo_key") {
        return None;
    }
    Some(clean.to_string())
}

fn get_attr_ci<'a>(attrs: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let obj = attrs.as_object()?;
    for want in keys {
        if let Some(v) = obj.get(*want) {
            if !v.is_null() {
                return Some(v);
            }
        }
    }
    for want in keys {
        for (k, v) in obj {
            if k.eq_ignore_ascii_case(want) && !v.is_null() {
                return Some(v);
            }
        }
    }
    None
}

fn attr_str(attrs: &Value, keys: &[&str]) -> Option<String> {
    let v = get_attr_ci(attrs, keys)?;
    match v {
        Value::String(s) => {
            let clean = s.trim();
            if clean.is_empty()
                || clean.eq_ignore_ascii_case("null")
                || clean.eq_ignore_ascii_case("none")
                || clean.eq_ignore_ascii_case("n/a")
                || clean.eq_ignore_ascii_case("not available")
            {
                None
            } else {
                Some(clean.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn attr_i64_nonneg(attrs: &Value, keys: &[&str]) -> Option<i64> {
    let v = get_attr_ci(attrs, keys)?;
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|x| x.is_finite()).map(|x| x.round() as i64))
            .filter(|&x| x >= 0 && x < 10_000),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .map(|x| x.round() as i64)
            .filter(|&x| x >= 0 && x < 10_000),
        _ => None,
    }
}

fn attr_f64(attrs: &Value, keys: &[&str]) -> Option<f64> {
    let v = get_attr_ci(attrs, keys)?;
    match v {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

/// Normalizes AFDC / OSM EV connector strings into canonical connector families
/// (`"NACS"`, `"CCS"`, `"CHAdeMO"`, `"J1772"`).
pub fn normalize_ev_connectors(raw_connectors: Option<&str>, network_hint: &str) -> Vec<String> {
    let mut set = BTreeSet::new();
    if let Some(raw) = raw_connectors {
        for tok in raw.split(|c: char| {
            c.is_whitespace() || matches!(c, ',' | ';' | '|' | '/' | '[' | ']' | '"' | '\'')
        }) {
            let up = tok.trim().to_ascii_uppercase();
            if up.is_empty() {
                continue;
            }
            match up.as_str() {
                "NACS" | "TESLA" | "J3400" | "SAE_J3400" => {
                    set.insert("NACS".to_string());
                }
                "J1772COMBO" | "CCS" | "CCS1" | "COMBO" | "TYPE1_COMBO" => {
                    set.insert("CCS".to_string());
                }
                "CHADEMO" => {
                    set.insert("CHAdeMO".to_string());
                }
                "J1772" | "TYPE1" | "NEMA1450" | "NEMA515" | "NEMA520" => {
                    set.insert("J1772".to_string());
                }
                _ => {}
            }
        }
    }
    if set.is_empty() {
        let nl = network_hint.to_ascii_lowercase();
        if nl.contains("supercharger") || (nl.contains("tesla") && !nl.contains("destination")) {
            set.insert("NACS".to_string());
        }
    }
    let mut ordered = Vec::new();
    for canonical in ["NACS", "CCS", "CHAdeMO", "J1772"] {
        if set.contains(canonical) {
            ordered.push(canonical.to_string());
        }
    }
    ordered
}

pub fn normalize_ev_network(raw: Option<&str>) -> String {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return "Independent / Non-Networked".to_string();
    };
    let low = s.to_ascii_lowercase();
    if low == "tesla" || low.contains("supercharger") {
        "Tesla Supercharger".to_string()
    } else if low.contains("tesla destination") {
        "Tesla Destination".to_string()
    } else if low.contains("chargepoint") {
        "ChargePoint".to_string()
    } else if low.contains("electrify america") {
        "Electrify America".to_string()
    } else if low.contains("evgo") {
        "EVgo".to_string()
    } else if low.contains("blink") {
        "Blink".to_string()
    } else if low.contains("shell") || low.contains("greenlots") {
        "Shell Recharge".to_string()
    } else if low.contains("flo") && low.len() <= 12 {
        "FLO".to_string()
    } else if low.contains("rivian") {
        "Rivian Adventure Network".to_string()
    } else if low.contains("non-networked") {
        "Independent / Non-Networked".to_string()
    } else {
        s.to_string()
    }
}

fn parse_afdc_station_record(
    attrs: &Value,
    geom: Option<&Value>,
    subj_lat: f64,
    subj_lon: f64,
    source_tag: &str,
    priority_rank: u8,
) -> Option<EvStationItem> {
    let (st_lat, st_lon) = geom
        .and_then(|g| {
            let x = g.get("x").and_then(Value::as_f64)?;
            let y = g.get("y").and_then(Value::as_f64)?;
            (x.is_finite() && y.is_finite()).then_some((y, x))
        })
        .or_else(|| {
            let la = attr_f64(attrs, &["latitude", "lat", "y"])?;
            let lo = attr_f64(attrs, &["longitude", "lon", "lng", "x"])?;
            Some((la, lo))
        })?;

    if !(-90.0..=90.0).contains(&st_lat) || !(-180.0..=180.0).contains(&st_lon) {
        return None;
    }

    let dist_m = haversine_m(subj_lat, subj_lon, st_lat, st_lon);
    if !dist_m.is_finite() || dist_m > AFDC_SEARCH_RADIUS_M * 1.15 {
        return None;
    }

    let raw_net = attr_str(attrs, &["ev_network", "network", "operator"]);
    let network = normalize_ev_network(raw_net.as_deref());
    let name = attr_str(attrs, &["station_name", "name"])
        .unwrap_or_else(|| format!("{network} Charging Station"));

    let dcfc_ports = attr_i64_nonneg(attrs, &["ev_dc_fast_num", "ev_dc_fast_count"]);
    let l2_ports = attr_i64_nonneg(attrs, &["ev_level2_evse_num", "ev_level2_count"]);

    let raw_conn = if let Some(arr) =
        get_attr_ci(attrs, &["ev_connector_types"]).and_then(Value::as_array)
    {
        let joined: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        (!joined.is_empty()).then(|| joined.join(" "))
    } else {
        attr_str(attrs, &["ev_connector_types", "connector_types"])
    };
    let connectors = normalize_ev_connectors(raw_conn.as_deref(), &network);

    let is_dc_fast = dcfc_ports.unwrap_or(0) > 0
        || (dcfc_ports.is_none()
            && connectors
                .iter()
                .any(|c| matches!(c.as_str(), "CCS" | "CHAdeMO")));
    let charger_tier = if is_dc_fast {
        "dcfc"
    } else if l2_ports.unwrap_or(0) > 0 || connectors.iter().any(|c| c == "J1772") {
        "level2"
    } else {
        "public_evse"
    }
    .to_string();

    let dir = compass8(bearing_deg(subj_lat, subj_lon, st_lat, st_lon)).to_string();

    Some(EvStationItem {
        name,
        network,
        charger_tier,
        is_dc_fast,
        dc_fast_ports: dcfc_ports,
        level2_ports: l2_ports,
        connectors,
        street_address: attr_str(attrs, &["street_address", "address"]),
        city: attr_str(attrs, &["city"]),
        pricing: attr_str(attrs, &["ev_pricing", "ev_pricing_french"]),
        access_hours: attr_str(attrs, &["access_days_time", "opening_hours"]),
        lat: round_n(st_lat, 6),
        lon: round_n(st_lon, 6),
        distance_m: round_n(dist_m, 1),
        distance_mi: round_n(meters_to_miles(dist_m), 2),
        direction: dir,
        source: source_tag.to_string(),
        priority_rank,
    })
}

fn parse_osm_amenity_station(
    feat: &Value,
    subj_lat: f64,
    subj_lon: f64,
) -> Option<EvStationItem> {
    let attrs = feat.get("attributes")?;
    let geom = feat.get("geometry")?;
    let lon = geom.get("x").and_then(Value::as_f64)?;
    let lat = geom.get("y").and_then(Value::as_f64)?;
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    let dist_m = haversine_m(subj_lat, subj_lon, lat, lon);
    if !dist_m.is_finite() || dist_m > AFDC_SEARCH_RADIUS_M {
        return None;
    }

    let raw_net = attr_str(attrs, &["network", "brand", "operator"]);
    let network = normalize_ev_network(raw_net.as_deref());
    let name = attr_str(attrs, &["name", "brand", "operator"])
        .unwrap_or_else(|| format!("{network} EV Charger"));

    let mut conn_tokens = Vec::new();
    for (keys, tag) in [
        (
            &["socket_nacs", "socket_tesla_supercharger", "socket_tesla_standard"][..],
            "NACS",
        ),
        (
            &["socket_type1_combo", "socket_ccs", "socket_type2_combo"][..],
            "CCS",
        ),
        (&["socket_chademo"][..], "CHADEMO"),
        (&["socket_type1", "socket_j1772"][..], "J1772"),
    ] {
        if let Some(s) = attr_str(attrs, keys) {
            if s != "no" && s != "0" {
                conn_tokens.push(tag);
            }
        }
    }
    let joined_conn = (!conn_tokens.is_empty()).then(|| conn_tokens.join(" "));
    let connectors = normalize_ev_connectors(joined_conn.as_deref(), &network);

    let is_dc_fast = connectors
        .iter()
        .any(|c| matches!(c.as_str(), "CCS" | "CHAdeMO"))
        || network == "Tesla Supercharger"
        || network == "Electrify America"
        || network == "EVgo";

    // Strict data honesty (`AGENTS.md`): OSM `capacity` rarely separates DCFC vs Level-2 ports,
    // so leave `dc_fast_ports` / `level2_ports` as `None` unless verified by DOE AFDC.
    let charger_tier = if is_dc_fast {
        "dcfc"
    } else if connectors.iter().any(|c| c == "J1772") {
        "level2"
    } else {
        "public_evse"
    }
    .to_string();

    let dir = compass8(bearing_deg(subj_lat, subj_lon, lat, lon)).to_string();

    Some(EvStationItem {
        name,
        network,
        charger_tier,
        is_dc_fast,
        dc_fast_ports: None,
        level2_ports: None,
        connectors,
        street_address: attr_str(attrs, &["addr_street", "street"]),
        city: attr_str(attrs, &["addr_city", "city"]),
        pricing: attr_str(attrs, &["charge", "fee"]),
        access_hours: attr_str(attrs, &["opening_hours"]),
        lat: round_n(lat, 6),
        lon: round_n(lon, 6),
        distance_m: round_n(dist_m, 1),
        distance_mi: round_n(meters_to_miles(dist_m), 2),
        direction: dir,
        source: "osm_na_amenities".to_string(),
        priority_rank: 1,
    })
}

fn norm_station_label(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

pub fn merge_and_dedupe_ev_stations(mut candidates: Vec<EvStationItem>) -> Vec<EvStationItem> {
    // Process higher-authority records (DOE AFDC / NLR API `priority_rank` >= 2) first, then closer distance.
    candidates.sort_by(|a, b| {
        b.priority_rank
            .cmp(&a.priority_rank)
            .then_with(|| {
                a.distance_m
                    .partial_cmp(&b.distance_m)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let mut deduped: Vec<EvStationItem> = Vec::new();
    for cand in candidates {
        let cand_lbl = norm_station_label(&cand.name);
        if let Some(existing) = deduped.iter_mut().find(|ex| {
            let d = haversine_m(ex.lat, ex.lon, cand.lat, cand.lon);
            if d <= DEDUP_DISTANCE_M {
                return true;
            }
            d <= DEDUP_SAME_NAME_M
                && !cand_lbl.is_empty()
                && (norm_station_label(&ex.name) == cand_lbl || ex.network == cand.network)
        }) {
            if existing.dc_fast_ports.is_none() && cand.dc_fast_ports.is_some() {
                existing.dc_fast_ports = cand.dc_fast_ports;
            }
            if existing.level2_ports.is_none() && cand.level2_ports.is_some() {
                existing.level2_ports = cand.level2_ports;
            }
            if !existing.is_dc_fast && cand.is_dc_fast {
                existing.is_dc_fast = true;
                existing.charger_tier = "dcfc".to_string();
            }
            for c in cand.connectors {
                if !existing.connectors.contains(&c) {
                    existing.connectors.push(c);
                }
            }
            if existing.pricing.is_none() {
                existing.pricing = cand.pricing;
            }
            if existing.access_hours.is_none() {
                existing.access_hours = cand.access_hours;
            }
            if existing.street_address.is_none() {
                existing.street_address = cand.street_address;
            }
        } else {
            deduped.push(cand);
        }
    }

    deduped.sort_by(|a, b| {
        a.distance_m
            .partial_cmp(&b.distance_m)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    deduped
}

async fn query_nlr_nearest_byok(
    http: &HttpClient,
    nlr_url: &str,
    api_key: &str,
    lat: f64,
    lon: f64,
) -> Result<Vec<EvStationItem>> {
    let mut parsed = Url::parse(nlr_url)?;
    {
        let mut qp = parsed.query_pairs_mut();
        qp.append_pair("api_key", api_key);
        qp.append_pair("latitude", &format!("{lat:.6}"));
        qp.append_pair("longitude", &format!("{lon:.6}"));
        qp.append_pair("fuel_type", "ELEC");
        qp.append_pair("status", "E");
        qp.append_pair("access", "public");
        qp.append_pair("radius", "5.0");
        qp.append_pair("limit", "25");
    }
    let j = http
        .get_json_opts(parsed.as_str(), EV_REQ_TIMEOUT_MS, 0, EV_CACHE_TTL_SECS as i64, None)
        .await
        .map_err(|e| anyhow!("developer.nlr.gov request failed: {}", e.to_string().replace(api_key, "[REDACTED]")))?;
    let arr = j
        .get("fuel_stations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|st| parse_afdc_station_record(st, None, lat, lon, "nlr_afdc_api", 3))
        .collect())
}

/// Fetches 5-year annualized `HDD65` / `CDD65` climate normals from Open-Meteo Historical Weather
/// (`year-5 .. year-1`), falling back to prior-year degree-days if the 5-year window is unavailable.
pub async fn fetch_5yr_climate_normals_with_url(
    http: &HttpClient,
    archive_url: &str,
    lat: f64,
    lon: f64,
) -> Option<FiveYearClimateNormals> {
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    let grid_lat = (lat * 10.0).round() / 10.0;
    let grid_lon = (lon * 10.0).round() / 10.0;
    let end_year = Utc::now().year() - 1;
    let start_year = end_year - 4;
    let cache_key = format!("clim5:{archive_url}:{grid_lat:.1}:{grid_lon:.1}:{start_year}-{end_year}");
    if let Some(hit) = CLIMATE_5YR_CACHE.get(&cache_key).await {
        return Some(hit);
    }

    let mut parsed = Url::parse(archive_url).ok()?;
    {
        let mut qp = parsed.query_pairs_mut();
        qp.append_pair("latitude", &format!("{grid_lat:.2}"));
        qp.append_pair("longitude", &format!("{grid_lon:.2}"));
        qp.append_pair("start_date", &format!("{start_year}-01-01"));
        qp.append_pair("end_date", &format!("{end_year}-12-31"));
        qp.append_pair("daily", "temperature_2m_mean");
        qp.append_pair("temperature_unit", "fahrenheit");
        qp.append_pair("timezone", "auto");
    }
    let req_url = parsed.to_string();
    if let Ok(j) = http
        .get_json_opts(&req_url, EV_REQ_TIMEOUT_MS, 0, 30 * 86_400, None)
        .await
    {
        if let Some(arr) = j.pointer("/daily/temperature_2m_mean").and_then(Value::as_array) {
            let mut hdd_sum = 0.0_f64;
            let mut cdd_sum = 0.0_f64;
            let mut valid_days = 0_usize;
            for tv in arr {
                if let Some(t) = tv.as_f64().filter(|x| x.is_finite()) {
                    hdd_sum += (65.0 - t).max(0.0);
                    cdd_sum += (t - 65.0).max(0.0);
                    valid_days += 1;
                }
            }
            if valid_days >= 300 {
                let annual_scale = 365.25 / (valid_days as f64);
                let years_sampled = ((valid_days as f64) / 365.25).round().max(1.0) as usize;
                let period_str = if years_sampled >= 2 {
                    format!("{start_year}–{end_year}")
                } else {
                    format!("{end_year}")
                };
                let normals = FiveYearClimateNormals {
                    hdd65: (hdd_sum * annual_scale).round() as i64,
                    cdd65: (cdd_sum * annual_scale).round() as i64,
                    normal_period: period_str.clone(),
                    years_sampled,
                    source_name: format!(
                        "Open-Meteo Historical Weather ({period_str} Daily-Mean HDD65/CDD65 Normals)"
                    ),
                    source_url: req_url,
                };
                CLIMATE_5YR_CACHE.insert(cache_key, normals.clone()).await;
                return Some(normals);
            }
        }
    }

    if archive_url == OPEN_METEO_ARCHIVE_URL {
        if let Some(one_yr) = fetch_live_climate_degree_days(http, lat, lon).await {
            return Some(FiveYearClimateNormals {
                hdd65: one_yr.hdd_65,
                cdd65: one_yr.cdd_65,
                normal_period: one_yr.year.to_string(),
                years_sampled: 1,
                source_name: one_yr.source_name,
                source_url: one_yr.source_url,
            });
        }
    }
    None
}

fn extract_mep_from_query(query: &ExtensionQueryProto) -> Option<MepIntelProto> {
    for env in &query.upstream_envelopes {
        if let Some(Payload::MepIntel(ref mep)) = env.payload {
            return Some(mep.clone());
        }
    }
    for env in &query.upstream_envelopes {
        if let Some(Payload::PermitsHistory(ref hist)) = env.payload {
            let permit_vals: Vec<Value> = hist
                .permits
                .iter()
                .filter_map(|p| serde_json::to_value(p).ok())
                .collect();
            if !permit_vals.is_empty() {
                return Some(extract_mep_intel(&permit_vals));
            }
        }
    }
    let ep = &query.extra_params;
    let has_any_mep_param = ep.contains_key("has_electrical_upgrade")
        || ep.contains_key("latest_electrical_year")
        || ep.contains_key("has_ev_charger")
        || ep.contains_key("has_heat_pump");
    if has_any_mep_param {
        let parse_bool = |k: &str| {
            ep.get(k)
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(false)
        };
        let parse_yr = |k: &str| ep.get(k).and_then(|v| v.trim().parse::<i32>().ok());
        return Some(MepIntelProto {
            has_hvac_upgrade: parse_bool("has_hvac_upgrade"),
            has_electrical_upgrade: parse_bool("has_electrical_upgrade"),
            has_plumbing_upgrade: parse_bool("has_plumbing_upgrade"),
            has_ev_charger: parse_bool("has_ev_charger"),
            has_heat_pump: parse_bool("has_heat_pump"),
            latest_hvac_year: parse_yr("latest_hvac_year"),
            latest_electrical_year: parse_yr("latest_electrical_year"),
            latest_plumbing_year: parse_yr("latest_plumbing_year"),
            permits: Vec::new(),
            note: "MEP flags supplied via query context.".to_string(),
        });
    }
    None
}

async fn resolve_tariffs_for_query(
    http: &HttpClient,
    env: &EnvConfig,
    query: &ExtensionQueryProto,
    lat: f64,
    lon: f64,
    use_live_utility_lookup: bool,
) -> ResolvedTariffInputs {
    let mut out = ResolvedTariffInputs::default();
    let ep = &query.extra_params;
    let parse_pos_f64 = |k: &str| -> Option<f64> {
        ep.get(k)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|&v| v.is_finite() && v > 0.0)
    };

    out.electric_rate_kwh = parse_pos_f64("electric_rate_kwh");
    out.off_peak_kwh = parse_pos_f64("off_peak_kwh");
    out.peak_kwh = parse_pos_f64("peak_kwh");
    out.electric_carrier = ep
        .get("electric_carrier")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    out.electric_schedule = ep
        .get("electric_schedule")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    out.gas_rate_therm = parse_pos_f64("gas_rate_therm");
    out.gas_carrier = ep
        .get("gas_carrier")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    if use_live_utility_lookup && (out.electric_rate_kwh.is_none() || out.gas_rate_therm.is_none())
    {
        let lq = LiveUtilityQuery {
            state: query.state.trim().to_ascii_uppercase(),
            county_fips: query.county_fips.trim().to_string(),
            city: query.city.trim().to_string(),
            zip: query.zip.trim().to_string(),
            lat: Some(lat),
            lon: Some(lon),
            sdwis_water_name: query.sdwis_water_name.clone(),
            sdwis_pwsid: query.sdwis_pwsid.clone(),
            gov_kwh_benchmark: query.gov_kwh_benchmark,
            is_private_well: query.is_likely_private_well.unwrap_or(false),
            is_private_septic: query.is_likely_private_septic.unwrap_or(false),
            hoa_covers_water_trash: query.hoa_covers_water_trash.unwrap_or(false),
            is_condo_or_attached: query.is_condo_or_attached.unwrap_or(false),
        };
        let bundle = resolve_live_utility_and_climate_bundle(http, env, &lq).await;
        if let Some(el) = bundle.electric {
            if out.electric_rate_kwh.is_none() && el.rate_per_kwh_usd > 0.0 {
                out.electric_rate_kwh = Some(el.rate_per_kwh_usd);
                if el.off_peak_kwh_usd > 0.0 {
                    out.off_peak_kwh = Some(el.off_peak_kwh_usd);
                }
                if el.peak_kwh_usd > 0.0 {
                    out.peak_kwh = Some(el.peak_kwh_usd);
                }
                out.electric_carrier = Some(el.carrier_name);
                out.electric_schedule = Some(el.rate_schedule);
                out.electric_source = Some(el.rate_source);
            }
        }
        if let Some(gs) = bundle.gas {
            if out.gas_rate_therm.is_none() {
                if let Some(rt) = gs.rate_per_therm_usd.filter(|&r| r > 0.0) {
                    out.gas_rate_therm = Some(rt);
                    out.gas_carrier = Some(gs.carrier_name);
                    out.gas_schedule = Some(gs.rate_schedule);
                    out.gas_source = Some(gs.rate_source);
                }
            }
        }
    }

    if query.state.trim().len() == 2
        && (out.electric_rate_kwh.is_none() || out.gas_rate_therm.is_none())
    {
        let st_up = query.state.trim().to_ascii_uppercase();
        let cname = query.county_name.as_deref().unwrap_or("");
        let u_inp = UtilityLookupInput {
            state: &st_up,
            county_fips: query.county_fips.trim(),
            county_name: cname,
            city: query.city.trim(),
            zip: query.zip.trim(),
            sqft: query.sqft.unwrap_or(0),
            year_built: query.year_built,
            is_condo_or_attached: query.is_condo_or_attached.unwrap_or(false),
            hoa_covers_water_trash: query.hoa_covers_water_trash.unwrap_or(false),
            is_likely_private_well: query.is_likely_private_well.unwrap_or(false),
            is_likely_private_septic: query.is_likely_private_septic.unwrap_or(false),
            sdwis_water_name: query.sdwis_water_name.as_deref(),
            sdwis_pwsid: query.sdwis_pwsid.as_deref(),
            gov_kwh_benchmark: query.gov_kwh_benchmark,
            has_solar: query.has_solar.unwrap_or(false),
            acs_broadband_pct: query.acs_broadband_pct,
            is_rural_area: query.is_rural_area.unwrap_or(false),
        };
        if out.electric_rate_kwh.is_none() {
            let eq = resolve_electric_utility(&u_inp);
            if eq.rate_per_kwh_usd > 0.0 {
                out.electric_rate_kwh = Some(eq.rate_per_kwh_usd);
                if out.off_peak_kwh.is_none() && eq.off_peak_kwh_usd > 0.0 {
                    out.off_peak_kwh = Some(eq.off_peak_kwh_usd);
                }
                if out.peak_kwh.is_none() && eq.peak_kwh_usd > 0.0 {
                    out.peak_kwh = Some(eq.peak_kwh_usd);
                }
                out.electric_carrier = Some(eq.carrier_name);
                out.electric_schedule = Some(eq.rate_schedule);
                out.electric_source = Some(eq.rate_source);
            }
        }
        if out.gas_rate_therm.is_none() {
            let gq = resolve_gas_utility(&u_inp);
            if let Some(rt) = gq.rate_per_therm_usd.filter(|&r| r > 0.0) {
                out.gas_rate_therm = Some(rt);
                out.gas_carrier = Some(gq.carrier_name);
                out.gas_schedule = Some(gq.rate_schedule);
                out.gas_source = Some(gq.rate_source);
            }
        }
    }

    out
}

/// Evaluates 200A electrical service panel readiness vs. UL-listed NEC 625.42 load-management
/// splitter options from building vintage and `permit_mep_extractor` (`MepIntelProto`).
pub fn evaluate_panel_readiness(
    year_built: Option<i64>,
    mep: Option<&MepIntelProto>,
) -> Value {
    let has_elec_upg = mep.map(|m| m.has_electrical_upgrade).unwrap_or(false);
    let latest_elec_yr = mep.and_then(|m| m.latest_electrical_year);
    let has_ev = mep.map(|m| m.has_ev_charger).unwrap_or(false);
    let has_hp = mep.map(|m| m.has_heat_pump).unwrap_or(false);

    let (status, est_class, upgrade_needed, splitter_applicable, rec) = if has_elec_upg {
        let yr_txt = latest_elec_yr
            .map(|y| format!(" ({y})"))
            .unwrap_or_default();
        (
            "verified_electrical_upgrade_permit",
            Some("200A_upgraded_likely"),
            Some(false),
            false,
            format!(
                "Permitted electrical upgrade on file{yr_txt}{}: verify main breaker rating (>=200A) and open 240V spaces for dedicated 48A EVSE and heat-pump circuits.",
                if has_ev {
                    " including permitted EV charger circuit"
                } else {
                    ""
                }
            ),
        )
    } else if let Some(yb) = year_built.filter(|&y| (1700..=2035).contains(&y)) {
        if yb < 1980 {
            (
                "pre_1980_unupgraded_panel_risk",
                Some("100A_to_125A_vintage_likely"),
                Some(true),
                true,
                format!(
                    "Built in {yb} with no electrical panel upgrade permit on file: legacy 100A–125A main panels typically require either a 200A service upgrade (eligible for IRA §25C up to $600 tax credit) OR a UL-listed NEC 625.42 automatic EV load-management splitter ($400–$1,100) before adding a 40A–48A Level-2 EV charger + heat-pump HVAC."
                ),
            )
        } else if yb < 2005 {
            (
                "verify_125a_or_200a_main_breaker",
                Some("125A_to_200A_mixed_era"),
                Some(false),
                true,
                format!(
                    "Built in {yb}: inspect main service breaker (125A vs. 200A) and physical busbar space; if rated 125A–150A, a UL-listed NEC 625.42 EVEMS load-splitter avoids utility trenching/panel replacement."
                ),
            )
        } else {
            (
                "modern_200a_standard_likely",
                Some("200A_modern_standard"),
                Some(false),
                false,
                format!(
                    "Built in {yb}: modern 200A service panel standard; verify garage 240V 50A pre-wire/conduit for plug-and-play Level-2 EV charging."
                ),
            )
        }
    } else {
        (
            "unverified_inspect_main_breaker",
            None,
            None,
            true,
            "Year built and electrical permit history are unverified; inspect main service amperage (>=200A preferred for full electrification, or use a UL-listed NEC 625.42 load-splitter on 100A–125A panels).".to_string(),
        )
    };

    json!({
        "status": status,
        "estimatedPanelClass": est_class,
        "panelUpgradeLikelyNeeded": upgrade_needed,
        "loadSplitterAlternativeApplicable": splitter_applicable,
        "hasPermittedElectricalUpgrade": has_elec_upg,
        "latestElectricalPermitYear": latest_elec_yr,
        "hasPermittedEvCharger": has_ev,
        "hasPermittedHeatPump": has_hp,
        "recommendation": rec,
    })
}

/// Synthesizes annual heating/cooling thermal & electric demand from `HDD65`/`CDD65` normals,
/// local `$/kWh` & `$/therm` tariffs, and Home Level-2 EV `$/1,000 mi`.
/// Strictly omits whole-home totals when `sqft` is missing (`AGENTS.md` data honesty).
pub fn synthesize_electrification_economics(
    sqft_opt: Option<i64>,
    year_built_opt: Option<i64>,
    climate: Option<&FiveYearClimateNormals>,
    tariffs: &ResolvedTariffInputs,
) -> (Value, Value) {
    let valid_sqft = sqft_opt.filter(|&s| s > 0).map(|s| s as f64);
    let valid_yb = year_built_opt.filter(|&y| (1700..=2035).contains(&y));

    let btu_per_hdd_sqft = match valid_yb {
        Some(y) if y < 1960 => 8.5,
        Some(y) if y < 1980 => 7.2,
        Some(y) if y < 2000 => 5.8,
        Some(y) if y < 2015 => 4.6,
        Some(_) => 3.8,
        None => 5.8,
    };
    let (btu_per_cdd_sqft, assumed_seer) = match valid_yb {
        Some(y) if y < 1980 => (7.5, 13.0),
        Some(y) if y < 2010 => (6.2, 14.5),
        Some(_) => (5.0, 16.5),
        None => (6.2, 14.5),
    };
    let assumed_afue = if valid_yb.map(|y| y >= 2010).unwrap_or(false) {
        0.90
    } else {
        0.82
    };

    let climate_json = if let Some(clim) = climate {
        let hdd = clim.hdd65.max(0) as f64;
        let cdd = clim.cdd65.max(0) as f64;
        let seasonal_cop = if hdd >= 5_500.0 {
            2.65
        } else if hdd >= 3_500.0 {
            2.95
        } else {
            3.30
        };

        // Per-1,000 sq ft normalized intensities (always valid when HDD65/CDD65 are verified)
        let heat_btu_per_1k = 1_000.0 * btu_per_hdd_sqft * hdd;
        let therms_per_1k = round_n(heat_btu_per_1k / (100_000.0 * assumed_afue), 1);
        let hp_kwh_per_1k = ((heat_btu_per_1k / (3_412.14 * seasonal_cop)).round()) as i64;
        let cool_kwh_per_1k =
            (((1_000.0 * btu_per_cdd_sqft * cdd) / (assumed_seer * 1_000.0)).round()) as i64;

        let (
            ann_therms,
            ann_hp_kwh,
            ann_cool_kwh,
            ann_gas_usd,
            ann_hp_usd,
            ann_cool_usd,
            ann_hp_vs_gas_delta_usd,
        ) = if let Some(sq) = valid_sqft {
            let scale = sq / 1_000.0;
            let th = round_n((heat_btu_per_1k * scale) / (100_000.0 * assumed_afue), 1);
            let hpk = ((heat_btu_per_1k * scale) / (3_412.14 * seasonal_cop)).round() as i64;
            let ck = ((1_000.0 * btu_per_cdd_sqft * cdd * scale) / (assumed_seer * 1_000.0))
                .round() as i64;

            let g_usd = tariffs
                .gas_rate_therm
                .map(|gt| (th * gt).round() as i64);
            let hp_usd = tariffs
                .electric_rate_kwh
                .map(|ek| ((hpk as f64) * ek).round() as i64);
            let c_usd = tariffs
                .electric_rate_kwh
                .map(|ek| ((ck as f64) * ek).round() as i64);
            let delta = match (g_usd, hp_usd) {
                (Some(g), Some(h)) => Some(g - h),
                _ => None,
            };
            (
                Some(th),
                Some(hpk),
                Some(ck),
                g_usd,
                hp_usd,
                c_usd,
                delta,
            )
        } else {
            (None, None, None, None, None, None, None)
        };

        json!({
            "available": true,
            "hdd65": clim.hdd65,
            "cdd65": clim.cdd65,
            "normalPeriod": clim.normal_period,
            "yearsSampled": clim.years_sampled,
            "sourceName": clim.source_name,
            "sourceUrl": clim.source_url,
            "livingSqftUsed": sqft_opt.filter(|&s| s > 0),
            "assumedGasFurnaceAfue": assumed_afue,
            "assumedHeatPumpSeasonalCop": seasonal_cop,
            "assumedCoolingSeer": assumed_seer,
            "per1000SqftIntensity": {
                "heatingThermsGas": therms_per_1k,
                "heatingKwhHeatPump": hp_kwh_per_1k,
                "coolingKwh": cool_kwh_per_1k,
            },
            "annualHeatingThermsGas": ann_therms,
            "annualHeatingKwhHeatPump": ann_hp_kwh,
            "annualCoolingKwh": ann_cool_kwh,
            "annualGasHeatingUsd": ann_gas_usd,
            "annualHeatPumpHeatingUsd": ann_hp_usd,
            "annualCoolingUsd": ann_cool_usd,
            "annualGasMinusHeatPumpSavingsUsd": ann_hp_vs_gas_delta_usd,
        })
    } else {
        json!({
            "available": false,
            "hdd65": Value::Null,
            "cdd65": Value::Null,
            "normalPeriod": Value::Null,
            "livingSqftUsed": sqft_opt.filter(|&s| s > 0),
            "annualHeatingThermsGas": Value::Null,
            "annualHeatingKwhHeatPump": Value::Null,
            "annualCoolingKwh": Value::Null,
            "annualGasHeatingUsd": Value::Null,
            "annualHeatPumpHeatingUsd": Value::Null,
            "annualCoolingUsd": Value::Null,
            "annualGasMinusHeatPumpSavingsUsd": Value::Null,
        })
    };

    let eff_ev_rate = tariffs.off_peak_kwh.or(tariffs.electric_rate_kwh);
    let cost_1000_mi = eff_ev_rate.map(|r| (r * EV_KWH_PER_1000_MI).round() as i64);
    let blended_1000_mi = tariffs
        .electric_rate_kwh
        .map(|r| (r * EV_KWH_PER_1000_MI).round() as i64);
    let annual_12k_usd = cost_1000_mi.map(|c| c * 12);

    let home_ev_json = json!({
        "available": eff_ev_rate.is_some(),
        "kwhPer1000Mi": EV_KWH_PER_1000_MI,
        "chargingRatePerKwhUsd": eff_ev_rate.map(|r| round_n(r, 4)),
        "blendedRatePerKwhUsd": tariffs.electric_rate_kwh.map(|r| round_n(r, 4)),
        "offPeakRatePerKwhUsd": tariffs.off_peak_kwh.map(|r| round_n(r, 4)),
        "peakRatePerKwhUsd": tariffs.peak_kwh.map(|r| round_n(r, 4)),
        "rateBasis": if tariffs.off_peak_kwh.is_some() {
            "off_peak_tou_tariff"
        } else if tariffs.electric_rate_kwh.is_some() {
            "blended_residential_tariff"
        } else {
            "missing_electric_tariff"
        },
        "costPer1000MiUsd": cost_1000_mi,
        "blendedCostPer1000MiUsd": blended_1000_mi,
        "annualHomeCharging12kMiUsd": annual_12k_usd,
    });

    (climate_json, home_ev_json)
}

pub fn build_ev_chargers_map_layer(stations: &[EvStationItem]) -> Option<Value> {
    if stations.is_empty() {
        return None;
    }
    let features: Vec<Value> = stations
        .iter()
        .take(MAX_REPORTED_STATIONS)
        .map(|s| {
            let col = if s.is_dc_fast { "#10b981" } else { "#0284c7" };
            let conn_str = if s.connectors.is_empty() {
                String::new()
            } else {
                format!(" · {}", s.connectors.join("/"))
            };
            let port_str = match (s.dc_fast_ports, s.level2_ports) {
                (Some(dc), Some(l2)) if dc > 0 && l2 > 0 => format!(" · {dc} DCFC + {l2} L2"),
                (Some(dc), _) if dc > 0 => format!(" · {dc} DCFC ports"),
                (_, Some(l2)) if l2 > 0 => format!(" · {l2} L2 ports"),
                _ => {
                    if s.is_dc_fast {
                        " · DC Fast".to_string()
                    } else {
                        " · Public EVSE".to_string()
                    }
                }
            };
            let popup = format!(
                "{} ({}){}{} · {:.2} mi {}",
                s.name, s.network, port_str, conn_str, s.distance_mi, s.direction
            );
            json!({
                "type": "Feature",
                "geometry": {
                    "type": "Point",
                    "coordinates": [s.lon, s.lat],
                },
                "properties": {
                    "name": s.name,
                    "network": s.network,
                    "chargerTier": s.charger_tier,
                    "isDcFast": s.is_dc_fast,
                    "dcFastPorts": s.dc_fast_ports,
                    "level2Ports": s.level2_ports,
                    "connectors": s.connectors,
                    "distanceM": s.distance_m,
                    "distanceMi": s.distance_mi,
                    "direction": s.direction,
                    "pricing": s.pricing,
                    "accessHours": s.access_hours,
                    "source": s.source,
                    "color": col,
                    "popup": popup,
                    "url": afdc_reference_url(s.lat, s.lon),
                }
            })
        })
        .collect();

    Some(json!({
        "id": "ev-chargers",
        "title": "Public EV DC Fast & Level-2 Chargers",
        "kind": "points",
        "color": "#10b981",
        "defaultOn": true,
        "type": "FeatureCollection",
        "features": features,
    }))
}

pub async fn fetch_ev_electrification(
    http: &HttpClient,
    env: &EnvConfig,
    query: &ExtensionQueryProto,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    fetch_ev_electrification_with_endpoints(
        http,
        env,
        query,
        &EvElectrificationEndpoints::default(),
        true,
    )
    .await
}

pub async fn fetch_ev_electrification_with_endpoints(
    http: &HttpClient,
    env: &EnvConfig,
    query: &ExtensionQueryProto,
    endpoints: &EvElectrificationEndpoints,
    use_live_utility_lookup: bool,
) -> Result<Option<Value>> {
    let country = query
        .extra_params
        .get("country")
        .map(String::as_str)
        .unwrap_or("");
    if !is_us_country_opt(country) {
        return Ok(None);
    }
    let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
        return Ok(None);
    };
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Ok(None);
    }

    let afdc_where = [(
        "where",
        "fuel_type_code='ELEC' AND status_code='E' AND access_code='public'".to_string(),
    )];
    let osm_where = [("where", "amenity='charging_station'".to_string())];
    let nrel_key_opt = sanitize_nrel_api_key(env.get("NREL_API_KEY").as_deref());

    let afdc_fut = http.arcgis_radius_query(
        &endpoints.afdc_layer_url,
        lat,
        lon,
        AFDC_SEARCH_RADIUS_M,
        "*",
        &afdc_where,
        EV_REQ_TIMEOUT_MS,
    );
    let osm_fut = http.arcgis_radius_query(
        &endpoints.osm_amenities_layer_url,
        lat,
        lon,
        OSM_CHARGER_RADIUS_M,
        "*",
        &osm_where,
        EV_REQ_TIMEOUT_MS,
    );
    let nlr_fut = async {
        match nrel_key_opt.as_deref() {
            Some(key) => Some(query_nlr_nearest_byok(http, &endpoints.nlr_nearest_url, key, lat, lon).await),
            None => None,
        }
    };
    let climate_fut =
        fetch_5yr_climate_normals_with_url(http, &endpoints.open_meteo_archive_url, lat, lon);
    let tariff_fut =
        resolve_tariffs_for_query(http, env, query, lat, lon, use_live_utility_lookup);

    let (afdc_res, osm_res, nlr_res_opt, climate_opt, tariffs) =
        tokio::join!(afdc_fut, osm_fut, nlr_fut, climate_fut, tariff_fut);

    let afdc_ok = afdc_res.is_ok();
    let osm_ok = osm_res.is_ok();
    let nlr_ok = nlr_res_opt.as_ref().map(|r| r.is_ok()).unwrap_or(false);

    if !afdc_ok && !osm_ok && !nlr_ok && climate_opt.is_none() {
        return Ok(None);
    }

    let mut raw_candidates: Vec<EvStationItem> = Vec::new();
    if let Ok(feats) = afdc_res {
        for f in &feats {
            if let Some(attrs) = f.get("attributes") {
                if let Some(item) =
                    parse_afdc_station_record(attrs, f.get("geometry"), lat, lon, "doe_afdc_bts", 2)
                {
                    raw_candidates.push(item);
                }
            }
        }
    }
    if let Some(Ok(nlr_items)) = nlr_res_opt {
        raw_candidates.extend(nlr_items);
    }
    if let Ok(osm_feats) = osm_res {
        for f in &osm_feats {
            if let Some(item) = parse_osm_amenity_station(f, lat, lon) {
                raw_candidates.push(item);
            }
        }
    }

    let stations = merge_and_dedupe_ev_stations(raw_candidates);

    let dcfc_within_3mi: Vec<&EvStationItem> = stations
        .iter()
        .filter(|s| s.is_dc_fast && s.distance_mi <= 3.0)
        .collect();
    let dcfc_within_5mi: Vec<&EvStationItem> =
        stations.iter().filter(|s| s.is_dc_fast).collect();

    let total_dcfc_ports_3mi: i64 = dcfc_within_3mi
        .iter()
        .filter_map(|s| s.dc_fast_ports)
        .sum();
    let total_l2_ports_3mi: i64 = stations
        .iter()
        .filter(|s| s.distance_mi <= 3.0)
        .filter_map(|s| s.level2_ports)
        .sum();

    let count_conn = |c_name: &str| -> usize {
        stations
            .iter()
            .filter(|s| s.connectors.iter().any(|c| c == c_name))
            .count()
    };
    let mut net_counts: BTreeMap<String, usize> = BTreeMap::new();
    for s in &stations {
        *net_counts.entry(s.network.clone()).or_insert(0) += 1;
    }

    let nearest_station = stations.first().cloned();
    let nearest_dcfc = dcfc_within_5mi.first().copied().cloned();
    let nearest_nacs_dcfc = dcfc_within_5mi
        .iter()
        .find(|s| s.connectors.iter().any(|c| c == "NACS"))
        .copied()
        .cloned();
    let nearest_ccs_dcfc = dcfc_within_5mi
        .iter()
        .find(|s| s.connectors.iter().any(|c| c == "CCS"))
        .copied()
        .cloned();

    let mep_opt = extract_mep_from_query(query);
    let panel_readiness = evaluate_panel_readiness(query.year_built, mep_opt.as_ref());
    let (climate_thermal, home_ev) =
        synthesize_electrification_economics(query.sqft, query.year_built, climate_opt.as_ref(), &tariffs);

    let mut map_layers = Vec::new();
    if let Some(ev_layer) = build_ev_chargers_map_layer(&stations) {
        map_layers.push(ev_layer);
    }

    let capped_stations: Vec<&EvStationItem> = stations.iter().take(MAX_REPORTED_STATIONS).collect();
    let fetched_at = now_iso();

    Ok(Some(json!({
        "extensionId": EV_ELECTRIFICATION_EXT_ID,
        "fetchedAt": fetched_at,
        "referenceUrl": afdc_reference_url(lat, lon),
        "sourcesQueried": {
            "btsDoeAfdcFeatureServer": afdc_ok,
            "esriOsmNaAmenities": osm_ok,
            "nlrByokApiUsed": nlr_ok,
            "openMeteo5YrNormals": climate_opt.is_some(),
        },
        "utilityTariffsUsed": {
            "electricCarrier": tariffs.electric_carrier,
            "electricSchedule": tariffs.electric_schedule,
            "electricRatePerKwhUsd": tariffs.electric_rate_kwh.map(|r| round_n(r, 4)),
            "offPeakRatePerKwhUsd": tariffs.off_peak_kwh.map(|r| round_n(r, 4)),
            "peakRatePerKwhUsd": tariffs.peak_kwh.map(|r| round_n(r, 4)),
            "electricSource": tariffs.electric_source,
            "gasCarrier": tariffs.gas_carrier,
            "gasSchedule": tariffs.gas_schedule,
            "gasRatePerThermUsd": tariffs.gas_rate_therm.map(|r| round_n(r, 3)),
            "gasSource": tariffs.gas_source,
        },
        "climateAndThermalLoad": climate_thermal,
        "homeEvCharging": home_ev,
        "panelReadiness": panel_readiness,
        "publicCharging": {
            "searchRadiusMi": 5.0,
            "totalStationsWithin5Mi": stations.len(),
            "dcfcStationsWithin3Mi": dcfc_within_3mi.len(),
            "dcfcStationsWithin5Mi": dcfc_within_5mi.len(),
            "verifiedDcfcPortsWithin3Mi": total_dcfc_ports_3mi,
            "verifiedLevel2PortsWithin3Mi": total_l2_ports_3mi,
            "connectorStationCounts": {
                "nacs": count_conn("NACS"),
                "ccs": count_conn("CCS"),
                "chademo": count_conn("CHAdeMO"),
                "j1772": count_conn("J1772"),
            },
            "networkStationCounts": net_counts,
            "nearestStation": nearest_station,
            "nearestDcfcStation": nearest_dcfc,
            "nearestNacsDcfcStation": nearest_nacs_dcfc,
            "nearestCcsDcfcStation": nearest_ccs_dcfc,
            "stations": capped_stations,
        },
        "mapLayers": map_layers,
    })))
}

/// Native `DataExtensionAdapter` for `ev_fast_chargers_electrification`.
pub struct EvFastChargersElectrificationAdapter;

impl DataExtensionAdapter for EvFastChargersElectrificationAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: EV_ELECTRIFICATION_EXT_ID.to_string(),
            name: "Public EV DC Fast Chargers & Home Electrification Economics".to_string(),
            version: "1.0.0".to_string(),
            description: "Zero-bottleneck keyless USDOT BTS / DOE AFDC public EV charging stations (NACS, CCS, CHAdeMO, J1772) enriched with Esri OSM charging amenities and optional BYOK developer.nlr.gov API, fused with 5-year HDD65/CDD65 climate normals, local electric/gas tariffs, and 200A panel / load-splitter readiness.".to_string(),
            domain: CapabilityDomain::DomainLivability as i32,
            additional_domains: vec![
                CapabilityDomain::DomainElectricUtility as i32,
                CapabilityDomain::DomainPropertyFacts as i32,
            ],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                states: Vec::new(),
                counties: Vec::new(),
                zip_prefixes: Vec::new(),
                zip_codes: Vec::new(),
                cities: Vec::new(),
                nationwide: true,
                specificity_override: None,
                country: "US".to_string(),
            }),
            auth: Some(AuthSpecProto {
                auth_type: "api_key_query".to_string(),
                env_var: "NREL_API_KEY".to_string(),
                header_name: String::new(),
                query_param: "api_key".to_string(),
                header_prefix: None,
                optional_key: true,
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 5,
                burst: 10,
                timeout_ms: EV_REQ_TIMEOUT_MS as u32,
                max_retries: 1,
            }),
            cache_ttl_secs: EV_CACHE_TTL_SECS,
            priority: 245,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.LivabilitySummaryProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some(AFDC_HOMEPAGE_URL.to_string()),
            documentation_url: Some(NLR_DOCS_URL.to_string()),
            codeowners: vec!["@openestate/livability".to_string()],
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
                ttl_secs: EV_CACHE_TTL_SECS,
                stale_while_revalidate_secs: 86_400,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: AFDC_SOURCE_NAME.to_string(),
                url: AFDC_HOMEPAGE_URL.to_string(),
                reference_url_template:
                    "https://afdc.energy.gov/stations#/find/nearest?location={{lat}},{{lon}}&fuel=ELEC"
                        .to_string(),
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
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if http.is_offline_external_blocked() {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
                return Ok(Vec::new());
            };
            let started = Instant::now();
            let Some(payload_val) = fetch_ev_electrification(http, env, query).await? else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = payload_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            let ref_url = afdc_reference_url(lat, lon);

            let attr = SourceAttributionProto {
                extension_id: EV_ELECTRIFICATION_EXT_ID.to_string(),
                source_name: AFDC_SOURCE_NAME.to_string(),
                source_url: ref_url,
                provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                fetched_at_rfc3339: fetched_at,
                schedule_or_version: Some("1.0.0".to_string()),
                notes: None,
            };

            let mut envs = vec![ExtensionEnvelopeProto {
                extension_id: EV_ELECTRIFICATION_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainLivability as i32,
                priority: 245,
                geo_specificity: 500,
                attribution: Some(attr.clone()),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&payload_val)?)),
            }];

            if let Some(mep) = extract_mep_from_query(query) {
                if mep.has_ev_charger || mep.has_heat_pump {
                    envs.push(ExtensionEnvelopeProto {
                        extension_id: EV_ELECTRIFICATION_EXT_ID.to_string(),
                        domain: CapabilityDomain::DomainPropertyFacts as i32,
                        priority: 245,
                        geo_specificity: 500,
                        attribution: Some(attr),
                        latency_ms,
                        cached: false,
                        payload: Some(Payload::PropertyFacts(PropertyFactsProto {
                            has_ev_charger: mep.has_ev_charger.then_some(true),
                            has_heat_pump: mep.has_heat_pump.then_some(true),
                            ..Default::default()
                        })),
                    });
                }
            }

            Ok(envs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::manifest::{
        parse_and_validate_package_with_origin, ManifestValidationOrigin,
    };
    use axum::{extract::Query, routing::get, Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[test]
    fn manifest_yaml_matches_native_adapter_and_strict_oss_schema() {
        let yaml = include_str!("../../extensions/ev_fast_chargers_electrification/manifest.yaml");
        let pkg = parse_and_validate_package_with_origin(
            yaml,
            None,
            Some(true),
            ManifestValidationOrigin::PublicOss,
        )
        .expect("manifest.yaml must pass strict PublicOss validation");
        let adapter = EvFastChargersElectrificationAdapter;
        assert_eq!(pkg.extension_id, EV_ELECTRIFICATION_EXT_ID);
        assert_eq!(pkg.manifest.domain, adapter.manifest().domain);
        assert_eq!(
            pkg.manifest.proto_message_type,
            adapter.manifest().proto_message_type
        );
        assert_eq!(pkg.manifest.sharing, adapter.manifest().sharing);
    }

    #[test]
    fn nrel_api_key_sanitizer_rejects_empty_and_shared_placeholder_keys() {
        assert_eq!(sanitize_nrel_api_key(None), None);
        assert_eq!(sanitize_nrel_api_key(Some("")), None);
        assert_eq!(sanitize_nrel_api_key(Some("   ")), None);
        let placeholder = concat!("DEMO", "_KEY");
        assert_eq!(sanitize_nrel_api_key(Some(placeholder)), None);
        assert_eq!(
            sanitize_nrel_api_key(Some(&placeholder.to_ascii_lowercase())),
            None
        );
        assert_eq!(
            sanitize_nrel_api_key(Some(" user-byok-secret-123 ")),
            Some("user-byok-secret-123".to_string())
        );
    }

    #[tokio::test]
    async fn offline_and_non_us_queries_never_fabricate_data() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let adapter = EvFastChargersElectrificationAdapter;

        let q = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            state: "CA".to_string(),
            sqft: Some(2040),
            year_built: Some(1974),
            ..Default::default()
        };
        let envs = adapter.execute_async(&q, &http, &env).await.unwrap();
        assert!(envs.is_empty(), "offline must return empty vec without inventing data");

        let mut non_us = q.clone();
        non_us
            .extra_params
            .insert("country".to_string(), "CA".to_string());
        let res = fetch_ev_electrification(&HttpClient::new(None), &env, &non_us)
            .await
            .unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn panel_readiness_distinguishes_permitted_upgrade_vs_pre_1980_vintage_and_missing_sqft() {
        // Pre-1980 unupgraded panel -> flags 200A upgrade or UL load-splitter
        let pre80 = evaluate_panel_readiness(Some(1972), None);
        assert_eq!(pre80["status"], "pre_1980_unupgraded_panel_risk");
        assert_eq!(pre80["panelUpgradeLikelyNeeded"], true);
        assert_eq!(pre80["loadSplitterAlternativeApplicable"], true);

        // Pre-1980 WITH verified electrical permit from permit_mep_extractor
        let mep = MepIntelProto {
            has_electrical_upgrade: true,
            latest_electrical_year: Some(2022),
            has_ev_charger: true,
            ..Default::default()
        };
        let upgraded = evaluate_panel_readiness(Some(1972), Some(&mep));
        assert_eq!(upgraded["status"], "verified_electrical_upgrade_permit");
        assert_eq!(upgraded["panelUpgradeLikelyNeeded"], false);
        assert_eq!(upgraded["latestElectricalPermitYear"], 2022);
        assert_eq!(upgraded["hasPermittedEvCharger"], true);

        // Missing sqft omits whole-home kWh/therms rather than inventing house size
        let clim = FiveYearClimateNormals {
            hdd65: 2150,
            cdd65: 480,
            normal_period: "2021–2025".to_string(),
            years_sampled: 5,
            source_name: "Open-Meteo".to_string(),
            source_url: "https://archive-api.open-meteo.com/v1/archive".to_string(),
        };
        let tariffs = ResolvedTariffInputs {
            electric_rate_kwh: Some(0.38),
            off_peak_kwh: Some(0.28),
            gas_rate_therm: Some(2.45),
            ..Default::default()
        };
        let (thermal_no_sqft, ev_ok) =
            synthesize_electrification_economics(None, Some(1974), Some(&clim), &tariffs);
        assert!(thermal_no_sqft["annualHeatingThermsGas"].is_null());
        assert!(thermal_no_sqft["annualHeatingKwhHeatPump"].is_null());
        assert!(thermal_no_sqft["annualCoolingKwh"].is_null());
        assert!(thermal_no_sqft["per1000SqftIntensity"]["heatingKwhHeatPump"].as_i64().unwrap() > 500);
        // Off-peak $0.28/kWh * 303 kWh = $85 / 1,000 mi
        assert_eq!(ev_ok["costPer1000MiUsd"], 85);
        assert_eq!(ev_ok["blendedCostPer1000MiUsd"], 115);
    }

    #[tokio::test]
    async fn fetches_bts_afdc_osm_and_byok_nlr_stations_climate_and_map_layer() {
        let nlr_hits = Arc::new(AtomicUsize::new(0));
        let nlr_hits_srv = Arc::clone(&nlr_hits);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let app = Router::new()
            .route(
                "/afdc/0/query",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let w = q.get("where").cloned().unwrap_or_default();
                    assert!(w.contains("fuel_type_code='ELEC'"));
                    assert!(w.contains("status_code='E'"));
                    assert!(w.contains("access_code='public'"));
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "station_name": "CITYOF CAMPBELL 2NDST GARAGE UL",
                                    "ev_network": "ChargePoint Network",
                                    "ev_dc_fast_num": 0,
                                    "ev_level2_evse_num": 6,
                                    "ev_connector_types": "J1772",
                                    "street_address": "50 N 2nd St",
                                    "city": "Campbell",
                                    "access_days_time": "24 hours daily"
                                },
                                "geometry": { "x": -121.9450, "y": 37.2875 }
                            },
                            {
                                "attributes": {
                                    "station_name": "Campbell Pruneyard Supercharger",
                                    "ev_network": "Tesla",
                                    "ev_dc_fast_num": 16,
                                    "ev_level2_evse_num": 0,
                                    "ev_connector_types": "TESLA J1772COMBO",
                                    "ev_pricing": "$0.42/kWh",
                                    "street_address": "1875 Synth Bascom Ave",
                                    "city": "Campbell",
                                    "access_days_time": "24 hours daily"
                                },
                                "geometry": { "x": -121.9325, "y": 37.2892 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/osm/0/query",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    assert_eq!(
                        q.get("where").map(String::as_str),
                        Some("amenity='charging_station'")
                    );
                    Json(json!({
                        "features": [
                            // Duplicate within 30m of Pruneyard Supercharger -> should merge, not double-count
                            {
                                "attributes": {
                                    "name": "Tesla Supercharger",
                                    "network": "Tesla Supercharger",
                                    "socket_nacs": "16"
                                },
                                "geometry": { "x": -121.9326, "y": 37.2892 }
                            },
                            // Distinct Electrify America CCS/CHAdeMO station
                            {
                                "attributes": {
                                    "name": "Westgate Center DC Fast",
                                    "network": "Electrify America",
                                    "socket_ccs": "6",
                                    "socket_chademo": "1"
                                },
                                "geometry": { "x": -121.9930, "y": 37.2925 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/nlr/nearest.json",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let counter = Arc::clone(&nlr_hits_srv);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(q.get("api_key").map(String::as_str), Some("valid-byok-key"));
                        Json(json!({
                            "fuel_stations": [
                                {
                                    "station_name": "Winchester EVgo Fast Charge",
                                    "ev_network": "eVgo Network",
                                    "ev_dc_fast_num": 4,
                                    "ev_level2_evse_num": 0,
                                    "ev_connector_types": ["J1772COMBO", "CHADEMO"],
                                    "latitude": 37.2801,
                                    "longitude": -121.9502
                                }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/meteo/archive",
                get(|| async {
                    // 365 days of realistic mild Bay Area daily means (52F winter, 72F summer)
                    let mut temps = Vec::with_capacity(365);
                    for d in 0..365 {
                        let t = if d < 120 || d > 300 {
                            53.0
                        } else if d > 180 && d < 250 {
                            71.0
                        } else {
                            63.0
                        };
                        temps.push(t);
                    }
                    Json(json!({
                        "daily": { "temperature_2m_mean": temps }
                    }))
                }),
            );

        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoints = EvElectrificationEndpoints {
            afdc_layer_url: format!("{base}/afdc/0"),
            osm_amenities_layer_url: format!("{base}/osm/0"),
            nlr_nearest_url: format!("{base}/nlr/nearest.json"),
            open_meteo_archive_url: format!("{base}/meteo/archive"),
        };

        let http = HttpClient::offline_loopback_only(None);

        // 1. Verify placeholder key is NEVER sent to developer.nlr.gov
        let mut env_demo_map = HashMap::new();
        env_demo_map.insert("NREL_API_KEY".to_string(), concat!("DEMO", "_KEY").to_string());
        let env_demo = EnvConfig::from_map(env_demo_map);

        let mut extra = HashMap::new();
        extra.insert("electric_rate_kwh".to_string(), "0.39".to_string());
        extra.insert("off_peak_kwh".to_string(), "0.29".to_string());
        extra.insert("gas_rate_therm".to_string(), "2.40".to_string());

        let q = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            city: "Campbell".to_string(),
            state: "CA".to_string(),
            zip: "95008".to_string(),
            sqft: Some(1_950),
            year_built: Some(1968),
            extra_params: extra,
            ..Default::default()
        };

        let res_no_byok = fetch_ev_electrification_with_endpoints(
            &http,
            &env_demo,
            &q,
            &endpoints,
            false,
        )
        .await
        .unwrap()
        .expect("keyless BTS AFDC + OSM succeeds");

        assert_eq!(
            nlr_hits.load(Ordering::SeqCst),
            0,
            "placeholder key must never call developer.nlr.gov"
        );
        assert_eq!(res_no_byok["publicCharging"]["totalStationsWithin5Mi"], 3);
        assert_eq!(res_no_byok["publicCharging"]["dcfcStationsWithin3Mi"], 2);
        assert_eq!(res_no_byok["publicCharging"]["connectorStationCounts"]["nacs"], 1);
        assert_eq!(res_no_byok["publicCharging"]["connectorStationCounts"]["ccs"], 2);
        assert_eq!(res_no_byok["publicCharging"]["connectorStationCounts"]["chademo"], 1);
        assert_eq!(res_no_byok["mapLayers"][0]["id"], "ev-chargers");
        assert_eq!(
            res_no_byok["mapLayers"][0]["features"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(res_no_byok["climateAndThermalLoad"]["hdd65"].as_i64().unwrap() > 1_500);
        assert!(res_no_byok["climateAndThermalLoad"]["cdd65"].as_i64().unwrap() > 300);
        assert!(
            res_no_byok["climateAndThermalLoad"]["annualHeatingKwhHeatPump"]
                .as_i64()
                .unwrap()
                > 1_000
        );
        assert_eq!(res_no_byok["homeEvCharging"]["costPer1000MiUsd"], 88);
        assert_eq!(
            res_no_byok["panelReadiness"]["status"],
            "pre_1980_unupgraded_panel_risk"
        );

        // 2. Verify real BYOK NREL_API_KEY calls developer.nlr.gov and merges 4th station
        let mut env_byok_map = HashMap::new();
        env_byok_map.insert("NREL_API_KEY".to_string(), "valid-byok-key".to_string());
        let env_byok = EnvConfig::from_map(env_byok_map);

        let res_byok = fetch_ev_electrification_with_endpoints(
            &http,
            &env_byok,
            &q,
            &endpoints,
            false,
        )
        .await
        .unwrap()
        .expect("BYOK enriched result");

        srv.abort();

        assert_eq!(nlr_hits.load(Ordering::SeqCst), 1);
        assert_eq!(res_byok["sourcesQueried"]["nlrByokApiUsed"], true);
        assert_eq!(res_byok["publicCharging"]["totalStationsWithin5Mi"], 4);
        assert_eq!(
            res_byok["publicCharging"]["nearestDcfcStation"]["name"],
            "Winchester EVgo Fast Charge"
        );
    }
}
