//! Commute and routing on self-hosted OSRM servers (`OSRM_BASE_URL` and/or regional
//! `OSRM_REGIONS_JSON`).
//!
//! Design: implementation plan v3, section 5.9 + Phase 7 multi-region OSRM routing.
//!
//! - **Router.** OSRM `table` service, `driving` profile, `annotations=duration,distance`. The
//!   public demo server (`router.project-osrm.org`) is never called, even when configured.
//! - **Multi-region.** When `OSRM_REGIONS_JSON` is set (an array of `{name, bbox: [minLon, minLat,
//!   maxLon, maxLat], baseUrl}` entries), each `(origin, destination)` pair selects the most specific
//!   configured region whose bounding box contains both points, falling back to `OSRM_BASE_URL` if
//!   set or deterministically to Haversine `estimated: true` (`no_osrm_region_configured`) otherwise.
//! - **Batching.** Each request carries at most [`MAX_SOURCES_PER_REQUEST`] sources plus as many
//!   destinations as fit in `OSRM_MAX_TABLE_LOCATIONS`, which must match the server's
//!   `--max-table-size`. Batches run concurrently, bounded by [`MAX_CONCURRENT_REQUESTS`] here and
//!   by the router's own semaphore in [`HttpClient`].
//! - **Cache.** In-process, per cell, keyed by router URL, profile, dataset version
//!   (`data_version` from the responses) and both coordinates rounded to 5 decimals (~1 m).
//! - **Estimates.** When no router/region matches ([`EstimateReason::RouterUnset`] /
//!   [`FallbackReason::NoOsrmRegionConfigured`]), the router fails ([`EstimateReason::RouterDown`] /
//!   [`FallbackReason::OsrmUnavailable`]), or a cell is `null` or snaps farther than
//!   [`DEFAULT_MAX_SNAP_M`] ([`EstimateReason::OutsideCoverage`]), the cell falls back to the
//!   road-distance estimate used by `access`: [`ROAD_DISTANCE_FACTOR`] × straight-line miles at
//!   [`EST_MINUTES_PER_MILE`] minutes per mile, at least [`EST_MIN_MINUTES`] minutes, flagged
//!   `estimated`.
//! - **Traffic.** OSRM has none. Free-flow minutes are multiplied by `OSRM_TRAFFIC_BUFFER`
//!   (default 1.3) into `buffered_min`, labelled as such.
//!
//! The scoring engine calls [`commute_times`] (or [`RoutingClient::commute_times`]); the report's
//! "Getting around" block (`access`) calls [`RoutingClient::drive_matrix`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::stream::{self, StreamExt};
use moka::sync::Cache;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::db::shared_cache_store::GeoIndexColumns;
use crate::db::DbStore;
use crate::extensions::shared_cache::{
    dataset_ids, decode_geohash_bbox, encode_geohash, get_shared_dataset_proto,
    put_shared_dataset_proto,
    shared_v1::{DriveTimeEntryProto, DriveTimeMatrixProto},
    ShareScope, SharedExtensionCache,
};
use crate::extensions::ExtensionHub;
use crate::shared::{
    haversine_m, sanitize_user_warning, url_host_label, EnvConfig, HttpClient, SourceItem, MILE_M,
};
pub use super::routing_providers::{
    build_eligible_cloud_pool, build_freeroute_matrix_request, build_locationiq_matrix_url,
    build_mapbox_matrix_url, build_openrouteservice_matrix_request, build_routing_fallback_telemetry,
    clear_cloud_provider_cooldown, cloud_cooldown_key, execute_cloud_matrix_request,
    is_cloud_provider_in_cooldown, is_routing_extension_configured, parse_cloud_matrix_response,
    preview_routing_extension_http, probe_routing_extension, record_routing_failover,
    record_routing_failure, record_routing_success, redact_routing_error, redact_url_secrets,
    round_robin_counter_for_signature, CloudFallbackPoolConfig, CloudFetchFailure,
    CloudProviderEndpoint, ParsedCloudMatrix, RoutingExtensionTelemetryItem,
    RoutingFallbackStrategy, RoutingProviderKind, AUTH_OR_RATE_LIMIT_COOLDOWN_SECS,
    CLOUD_OSM_TRAFFIC_BUFFER, DEFAULT_EXTERNAL_COOLDOWN_SECS, DEFAULT_EXTERNAL_TIMEOUT_MS,
    DEFAULT_FREEROUTE_BASE_URL, DEFAULT_LOCATIONIQ_BASE_URL, DEFAULT_MAPBOX_BASE_URL,
    DEFAULT_OPENROUTESERVICE_BASE_URL, DEFAULT_ORS_TIMEOUT_MS, ENV_FREEROUTE_API_KEY,
    ENV_FREEROUTE_BASE_URL, ENV_LOCATIONIQ_ACCESS_TOKEN, ENV_LOCATIONIQ_API_KEY,
    ENV_LOCATIONIQ_BASE_URL, ENV_MAPBOX_ACCESS_TOKEN, ENV_MAPBOX_API_KEY, ENV_MAPBOX_BASE_URL,
    ENV_OPENROUTESERVICE_API_KEY, ENV_OPENROUTESERVICE_BASE_URL, ENV_ORS_API_KEY,
    ENV_ROUTING_EXTERNAL_COOLDOWN_SECS, ENV_ROUTING_EXTERNAL_TIMEOUT_MS,
    ENV_ROUTING_FALLBACK_ORDER, ENV_ROUTING_FALLBACK_STRATEGY, MAPBOX_TRAFFIC_BUFFER,
    MAX_CLOUD_MATRIX_LOCATIONS,
};

/// Default / fallback router base URL, e.g. `http://osrm:5000`.
pub const ENV_OSRM_BASE_URL: &str = "OSRM_BASE_URL";
/// Optional self-hosted OSRM walking/foot router base URL (`foot` profile; public demo hosts banned).
pub const ENV_OSRM_FOOT_BASE_URL: &str = "OSRM_FOOT_BASE_URL";
/// Optional self-hosted OSRM cycling/bike router base URL (`bike` profile; public demo hosts banned).
pub const ENV_OSRM_BIKE_BASE_URL: &str = "OSRM_BIKE_BASE_URL";
/// Keyless Esri OpenStreetMap North America Trails / Sidewalks / Bikeways FeatureServer.
pub const OSM_NA_TRAILS_LAYER: &str =
    "https://services6.arcgis.com/Do88DoK2xjTUCXd1/arcgis/rest/services/OSM_NA_Trails/FeatureServer/0";
/// Regional OSRM endpoints JSON (`[{"name":"ca","bbox":[-124.5,32.5,-114.1,42.0],"baseUrl":"http://osrm-ca:5000"}]`).
pub const ENV_OSRM_REGIONS_JSON: &str = "OSRM_REGIONS_JSON";
/// Peak-traffic multiplier applied to free-flow minutes (default 1.3, clamped to 1.0–3.0).
pub const ENV_OSRM_TRAFFIC_BUFFER: &str = "OSRM_TRAFFIC_BUFFER";
/// Sources plus destinations per `table` request; must match the server's `--max-table-size`.
pub const ENV_OSRM_MAX_TABLE_LOCATIONS: &str = "OSRM_MAX_TABLE_LOCATIONS";
/// Timeout per router request in milliseconds (default 8000).
pub const ENV_OSRM_TIMEOUT_MS: &str = "OSRM_TIMEOUT_MS";
/// Per-router failure cooldown in seconds after transport/timeout/5xx error (default 20).
pub const ENV_OSRM_FAILURE_COOLDOWN_SECS: &str = "OSRM_FAILURE_COOLDOWN_SECS";

pub const DEFAULT_TRAFFIC_BUFFER: f64 = 1.3;
pub const DEFAULT_MAX_TABLE_LOCATIONS: usize = 1000;
pub const DEFAULT_TIMEOUT_MS: u64 = 8000;
pub const DEFAULT_FAILURE_COOLDOWN_SECS: u64 = 20;
const MAX_FAILURE_COOLDOWN_SECS: u64 = 300;
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 3_000;
/// A coordinate farther than this from the road OSRM snaps it to lies outside the extract (OSRM
/// otherwise snaps any point, however far, to the nearest road it has).
pub const DEFAULT_MAX_SNAP_M: f64 = 3_000.0;

const MIN_TRAFFIC_BUFFER: f64 = 1.0;
/// Operator env-override traffic-buffer ceiling (`OSRM_TRAFFIC_BUFFER` clamped to `1.0..=3.0`);
/// buyer-profile UI input is capped separately at `scoring::profile::MAX_TRAFFIC_BUFFER` (`2.0`).
const MAX_TRAFFIC_BUFFER: f64 = 3.0;
const MIN_TABLE_LOCATIONS: usize = 2;
const MIN_TIMEOUT_MS: u64 = 500;
const MAX_TIMEOUT_MS: u64 = 60_000;

/// Sources per `table` request: up to five of the buyer's places (plan section 5.9).
pub const MAX_SOURCES_PER_REQUEST: usize = 5;
/// Table requests in flight per call.
pub const MAX_CONCURRENT_REQUESTS: usize = 4;
/// OSRM profile segment in request URLs and cache keys.
pub const PROFILE: &str = "driving";

/// Road distance ≈ this × straight-line distance (same factor as the original `access` fallback).
pub const ROAD_DISTANCE_FACTOR: f64 = 1.28;
/// Estimated driving minutes per road mile.
pub const EST_MINUTES_PER_MILE: f64 = 2.1;
/// Floor for an estimated drive, in minutes.
pub const EST_MIN_MINUTES: f64 = 2.0;

/// Source attribution when the router answered.
pub const OSRM_ATTRIBUTION: &str =
    "OSRM routing (self-hosted) on OpenStreetMap data, © OpenStreetMap contributors, ODbL";
/// Where the OpenStreetMap attribution and ODbL licence are explained.
pub const OSM_COPYRIGHT_URL: &str = "https://www.openstreetmap.org/copyright";

const CACHE_CAPACITY: u64 = 250_000;
const CACHE_TTL: Duration = Duration::from_secs(7 * 86_400);
const MAX_WARNINGS: usize = 3;

// ------------------------------------------------------------------------------------------
// Configuration
// ------------------------------------------------------------------------------------------

/// One regional self-hosted OSRM instance covering a `[min_lon, min_lat, max_lon, max_lat]` box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsrmRegion {
    pub name: String,
    /// `[min_lon, min_lat, max_lon, max_lat]` in decimal degrees.
    pub bbox: [f64; 4],
    #[serde(alias = "base_url", alias = "url")]
    pub base_url: String,
}

impl OsrmRegion {
    /// True when `pt` is valid and lies inside (inclusive) this region's bounding box.
    pub fn contains(&self, pt: LatLon) -> bool {
        pt.is_valid()
            && pt.lon >= self.bbox[0]
            && pt.lon <= self.bbox[2]
            && pt.lat >= self.bbox[1]
            && pt.lat <= self.bbox[3]
    }

    /// Center of this region's bounding box (`[min_lon, min_lat, max_lon, max_lat]`).
    pub fn center(&self) -> LatLon {
        LatLon::new(
            (self.bbox[1] + self.bbox[3]) * 0.5,
            (self.bbox[0] + self.bbox[2]) * 0.5,
        )
    }

    fn bbox_area(&self) -> f64 {
        (self.bbox[2] - self.bbox[0]).abs() * (self.bbox[3] - self.bbox[1]).abs()
    }
}

/// Routing settings, normally read from `OSRM_*` environment variables.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingConfig {
    /// Fallback / single-region router base URL without a trailing slash. `None`: rely on `regions` or estimates.
    pub base_url: Option<String>,
    /// Optional self-hosted OSRM `foot` profile base URL (`OSRM_FOOT_BASE_URL`).
    pub foot_base_url: Option<String>,
    /// Optional self-hosted OSRM `bike` profile base URL (`OSRM_BIKE_BASE_URL`).
    pub bike_base_url: Option<String>,
    /// Optional regional OSRM endpoints parsed from `OSRM_REGIONS_JSON`.
    pub regions: Vec<OsrmRegion>,
    /// Multiplier from free-flow to buffered minutes.
    pub traffic_buffer: f64,
    /// True when `traffic_buffer` was explicitly set via `OSRM_TRAFFIC_BUFFER` or `with_traffic_buffer`.
    pub traffic_buffer_overridden: bool,
    /// Maximum sources plus destinations per `table` request.
    pub max_table_locations: usize,
    /// Timeout per request, in milliseconds.
    pub timeout_ms: u64,
    /// Snap distance beyond which a coordinate counts as outside coverage, in meters.
    pub max_snap_m: f64,
    /// Per-router cooldown after transport/timeout/5xx failure, in seconds (`0` disables cooldown).
    pub failure_cooldown_secs: u64,
    /// Tier-2 external cloud matrix fallback pool configuration & load-balancing strategy.
    pub cloud: CloudFallbackPoolConfig,
    /// Problems found while reading the settings; reported as warnings by every matrix.
    pub issues: Vec<String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            foot_base_url: None,
            bike_base_url: None,
            regions: Vec::new(),
            traffic_buffer: DEFAULT_TRAFFIC_BUFFER,
            traffic_buffer_overridden: false,
            max_table_locations: DEFAULT_MAX_TABLE_LOCATIONS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_snap_m: DEFAULT_MAX_SNAP_M,
            failure_cooldown_secs: DEFAULT_FAILURE_COOLDOWN_SECS,
            cloud: CloudFallbackPoolConfig::default(),
            issues: Vec::new(),
        }
    }
}

impl RoutingConfig {
    /// Reads `OSRM_BASE_URL`, `OSRM_FOOT_BASE_URL`, `OSRM_BIKE_BASE_URL`, `OSRM_REGIONS_JSON`,
    /// `OSRM_TRAFFIC_BUFFER`, `OSRM_MAX_TABLE_LOCATIONS`, `OSRM_TIMEOUT_MS`,
    /// `OSRM_FAILURE_COOLDOWN_SECS`, and all Tier-2 cloud matrix fallback environment variables.
    /// Invalid values fall back to their defaults and are listed in `issues`; an invalid or
    /// public-demo base URL disables that router.
    pub fn from_env(env: &EnvConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(raw) = env.get(ENV_OSRM_BASE_URL) {
            match normalize_base_url_for(raw, ENV_OSRM_BASE_URL) {
                Ok(url) => cfg.base_url = Some(url),
                Err(issue) => cfg.issues.push(issue),
            }
        }
        if let Some(raw) = env.get(ENV_OSRM_FOOT_BASE_URL) {
            match normalize_base_url_for(raw, ENV_OSRM_FOOT_BASE_URL) {
                Ok(url) => cfg.foot_base_url = Some(url),
                Err(issue) => cfg.issues.push(issue),
            }
        }
        if let Some(raw) = env.get(ENV_OSRM_BIKE_BASE_URL) {
            match normalize_base_url_for(raw, ENV_OSRM_BIKE_BASE_URL) {
                Ok(url) => cfg.bike_base_url = Some(url),
                Err(issue) => cfg.issues.push(issue),
            }
        }
        if let Some(raw) = env.get(ENV_OSRM_REGIONS_JSON) {
            parse_osrm_regions_json(raw, &mut cfg.regions, &mut cfg.issues);
        }
        if let Some(raw) = env.get(ENV_OSRM_TRAFFIC_BUFFER) {
            match raw.trim().parse::<f64>() {
                Ok(v) if v.is_finite() => {
                    cfg.traffic_buffer = v.clamp(MIN_TRAFFIC_BUFFER, MAX_TRAFFIC_BUFFER);
                    cfg.traffic_buffer_overridden = true;
                    if cfg.traffic_buffer != v {
                        cfg.issues.push(format!(
                            "{ENV_OSRM_TRAFFIC_BUFFER}={v} is outside \
                             {MIN_TRAFFIC_BUFFER}–{MAX_TRAFFIC_BUFFER}; using {}",
                            cfg.traffic_buffer
                        ));
                    }
                }
                _ => cfg.issues.push(format!(
                    "{ENV_OSRM_TRAFFIC_BUFFER} is not a number; using {DEFAULT_TRAFFIC_BUFFER}"
                )),
            }
        }
        if let Some(raw) = env.get(ENV_OSRM_MAX_TABLE_LOCATIONS) {
            match raw.trim().parse::<usize>() {
                Ok(v) if v >= MIN_TABLE_LOCATIONS => cfg.max_table_locations = v,
                _ => cfg.issues.push(format!(
                    "{ENV_OSRM_MAX_TABLE_LOCATIONS} must be an integer of at least \
                     {MIN_TABLE_LOCATIONS}; using {DEFAULT_MAX_TABLE_LOCATIONS}"
                )),
            }
        }
        if let Some(raw) = env.get(ENV_OSRM_TIMEOUT_MS) {
            match raw.trim().parse::<u64>() {
                Ok(v) => cfg.timeout_ms = v.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
                Err(_) => cfg.issues.push(format!(
                    "{ENV_OSRM_TIMEOUT_MS} is not an integer; using {DEFAULT_TIMEOUT_MS}"
                )),
            }
        }
        if let Some(raw) = env.get(ENV_OSRM_FAILURE_COOLDOWN_SECS) {
            match raw.trim().parse::<u64>() {
                Ok(v) => cfg.failure_cooldown_secs = v.min(MAX_FAILURE_COOLDOWN_SECS),
                Err(_) => cfg.issues.push(format!(
                    "{ENV_OSRM_FAILURE_COOLDOWN_SECS} is not an integer; using {DEFAULT_FAILURE_COOLDOWN_SECS}"
                )),
            }
        }
        cfg.cloud = CloudFallbackPoolConfig::from_env(env, &mut cfg.issues);
        cfg
    }

    /// Whether at least one self-hosted Tier-1 OSRM router (regional or global) is configured.
    pub fn is_enabled(&self) -> bool {
        self.base_url.is_some() || !self.regions.is_empty()
    }

    /// Whether either Tier-1 self-hosted OSRM or at least one Tier-2 cloud fallback is configured.
    pub fn has_any_router(&self) -> bool {
        self.is_enabled() || self.cloud.has_any_configured()
    }

    /// Picks the most specific configured region whose bounding box contains both `from` and `to`.
    pub fn select_region(&self, from: LatLon, to: LatLon) -> Option<&OsrmRegion> {
        self.regions
            .iter()
            .filter(|r| r.contains(from) && r.contains(to))
            .min_by(|a, b| a.bbox_area().total_cmp(&b.bbox_area()))
    }

    /// Resolves the primary OSRM base URL to use for routing between `from` and `to`: most specific
    /// matching region first, then `base_url` fallback. Returns `None` when no configured Tier-1
    /// OSRM router covers both points.
    pub fn resolve_endpoint(&self, from: LatLon, to: LatLon) -> Option<&str> {
        self.select_region(from, to)
            .map(|r| r.base_url.as_str())
            .or(self.base_url.as_deref())
    }

    /// Ordered cascade of all matching Tier-1 OSRM base URLs for `(from, to)`:
    /// all matching regions sorted by ascending bbox area (most specific first, e.g. `california`
    /// then containing `us-west`), followed by `base_url` if set, deduplicated in order.
    pub fn resolve_endpoints(&self, from: LatLon, to: LatLon) -> Vec<String> {
        let mut matched: Vec<&OsrmRegion> = self
            .regions
            .iter()
            .filter(|r| r.contains(from) && r.contains(to))
            .collect();
        matched.sort_by(|a, b| a.bbox_area().total_cmp(&b.bbox_area()));
        let mut out: Vec<String> = Vec::with_capacity(matched.len() + 1);
        for r in matched {
            if !out.contains(&r.base_url) {
                out.push(r.base_url.clone());
            }
        }
        if let Some(ref b) = self.base_url {
            if !out.contains(b) {
                out.push(b.clone());
            }
        }
        out
    }
}

/// True for the public OSRM demo server, which Open Estate must never call.
pub fn is_public_demo_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "project-osrm.org"
        || h.ends_with(".project-osrm.org")
        || h == "routing.openstreetmap.de"
        || h.ends_with(".routing.openstreetmap.de")
}

fn normalize_base_url_for(raw: &str, env_name: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = Url::parse(trimmed)
        .map_err(|e| format!("{env_name} is not a valid URL ({e}); routing disabled"))?;
    if !matches!(url.scheme(), "http" | "https") || url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "{env_name} must be an http(s) base URL without a query or fragment; routing disabled"
        ));
    }
    let host = url.host_str().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("{env_name} has no host; routing disabled"));
    }
    if is_public_demo_host(host) {
        return Err(format!(
            "{env_name} points at the public OSRM demo server, which Open Estate does not \
             call; routing disabled"
        ));
    }
    Ok(trimmed.to_string())
}

fn parse_bbox(v: &Value) -> Option<[f64; 4]> {
    let arr = v.as_array()?;
    if arr.len() != 4 {
        return None;
    }
    let min_lon = arr[0].as_f64()?;
    let min_lat = arr[1].as_f64()?;
    let max_lon = arr[2].as_f64()?;
    let max_lat = arr[3].as_f64()?;
    if !(min_lon.is_finite()
        && min_lat.is_finite()
        && max_lon.is_finite()
        && max_lat.is_finite()
        && (-180.0..=180.0).contains(&min_lon)
        && (-180.0..=180.0).contains(&max_lon)
        && (-90.0..=90.0).contains(&min_lat)
        && (-90.0..=90.0).contains(&max_lat)
        && min_lon <= max_lon
        && min_lat <= max_lat)
    {
        return None;
    }
    Some([min_lon, min_lat, max_lon, max_lat])
}

fn parse_osrm_regions_json(raw: &str, out: &mut Vec<OsrmRegion>, issues: &mut Vec<String>) {
    let val: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            issues.push(format!("{ENV_OSRM_REGIONS_JSON} is invalid JSON ({e}); regional routing ignored"));
            return;
        }
    };
    let mut entries: Vec<(String, &Value)> = Vec::new();
    if let Some(arr) = val.as_array() {
        for (idx, item) in arr.iter().enumerate() {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("region-{idx}"));
            entries.push((name, item));
        }
    } else if let Some(obj) = val.as_object() {
        for (k, item) in obj {
            entries.push((k.clone(), item));
        }
    } else {
        issues.push(format!("{ENV_OSRM_REGIONS_JSON} must be a JSON array or object"));
        return;
    }

    for (name, item) in entries {
        let Some(bbox) = item.get("bbox").and_then(parse_bbox) else {
            issues.push(format!("{ENV_OSRM_REGIONS_JSON} region {name:?} has invalid bbox [minLon,minLat,maxLon,maxLat]"));
            continue;
        };
        let raw_url = item
            .get("baseUrl")
            .or_else(|| item.get("base_url"))
            .or_else(|| item.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match normalize_base_url_for(raw_url, &format!("{ENV_OSRM_REGIONS_JSON}[{name}]")) {
            Ok(base_url) => out.push(OsrmRegion {
                name,
                bbox,
                base_url,
            }),
            Err(issue) => issues.push(issue),
        }
    }
}

// ------------------------------------------------------------------------------------------
// Public types
// ------------------------------------------------------------------------------------------

/// A WGS84 point in decimal degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

impl LatLon {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }

    /// Finite, in range, and not the (0, 0) placeholder.
    pub fn is_valid(&self) -> bool {
        self.lat.is_finite()
            && self.lon.is_finite()
            && self.lat.abs() <= 90.0
            && self.lon.abs() <= 180.0
            && (self.lat != 0.0 || self.lon != 0.0)
    }

    /// Latitude and longitude in units of 1e-5 degrees: the 5-decimal cache key and request
    /// coordinate.
    fn key(&self) -> (i32, i32) {
        (
            (self.lat * 1e5).round() as i32,
            (self.lon * 1e5).round() as i32,
        )
    }
}

/// Why a drive time is an estimate rather than a routed result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateReason {
    /// `OSRM_BASE_URL` is not set (or not usable), so no router was called.
    RouterUnset,
    /// The router did not answer, timed out or returned an error.
    RouterDown,
    /// The router has no route: the place or home lies outside the routing extract.
    OutsideCoverage,
}

impl EstimateReason {
    /// The wire value: `router_unset`, `router_down` or `outside_coverage`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RouterUnset => "router_unset",
            Self::RouterDown => "router_down",
            Self::OutsideCoverage => "outside_coverage",
        }
    }

    /// Short English label for the UI.
    pub fn label_en(self) -> &'static str {
        match self {
            Self::RouterUnset => "Estimated: no routing server is configured",
            Self::RouterDown => "Estimated: the routing server did not respond",
            Self::OutsideCoverage => "Estimated: outside the routing map coverage",
        }
    }

    /// Short Simplified Chinese label for the UI.
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::RouterUnset => "估算值：未配置路线服务器",
            Self::RouterDown => "估算值：路线服务器无响应",
            Self::OutsideCoverage => "估算值：超出路线地图覆盖范围",
        }
    }
}

/// Granular fallback reason used for multi-region and availability diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// Neither `OSRM_BASE_URL` nor a matching `OSRM_REGIONS_JSON` bbox covers this pair.
    NoOsrmRegionConfigured,
    /// The selected regional/default OSRM server failed, timed out, or was unreachable.
    OsrmUnavailable,
    /// Tier-2 cloud fallback routing failed when no self-hosted OSRM endpoint covered the pair.
    CloudFallbackUnavailable,
    /// The OSRM server answered, but the point snapped too far or no route existed (`null`).
    OutsideCoverage,
}

impl FallbackReason {
    /// Wire value: `no_osrm_region_configured`, `osrm_unavailable`, `cloud_fallback_unavailable`, or `outside_coverage`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoOsrmRegionConfigured => "no_osrm_region_configured",
            Self::OsrmUnavailable => "osrm_unavailable",
            Self::CloudFallbackUnavailable => "cloud_fallback_unavailable",
            Self::OutsideCoverage => "outside_coverage",
        }
    }
}

/// One drive between two points. Values are unrounded; round for display.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveTime {
    /// Free-flow (no traffic) driving minutes.
    pub free_flow_min: f64,
    /// `free_flow_min` × traffic buffer.
    pub buffered_min: f64,
    /// Road distance in miles.
    pub distance_mi: f64,
    /// True when this is the road-distance estimate rather than a routed result.
    pub estimated: bool,
    /// Set exactly when `estimated` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<EstimateReason>,
    /// Detailed regional/availability fallback code when `estimated` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<FallbackReason>,
}

impl DriveTime {
    fn routed(seconds: f64, meters: f64, traffic_buffer: f64) -> Self {
        let free_flow_min = seconds / 60.0;
        Self {
            free_flow_min,
            buffered_min: free_flow_min * traffic_buffer,
            distance_mi: meters / MILE_M,
            estimated: false,
            reason: None,
            fallback_reason: None,
        }
    }

    fn estimate(
        from: LatLon,
        to: LatLon,
        traffic_buffer: f64,
        reason: EstimateReason,
        fallback_reason: FallbackReason,
    ) -> Self {
        let (distance_mi, free_flow_min) = estimate_drive(from, to);
        Self {
            free_flow_min,
            buffered_min: free_flow_min * traffic_buffer,
            distance_mi,
            estimated: true,
            reason: Some(reason),
            fallback_reason: Some(fallback_reason),
        }
    }
}

/// The road-distance estimate `(miles, free-flow minutes)`: [`ROAD_DISTANCE_FACTOR`] ×
/// straight-line miles, at [`EST_MINUTES_PER_MILE`] minutes per mile, at least
/// [`EST_MIN_MINUTES`] minutes.
pub fn estimate_drive(from: LatLon, to: LatLon) -> (f64, f64) {
    let miles = haversine_m(from.lat, from.lon, to.lat, to.lon) / MILE_M * ROAD_DISTANCE_FACTOR;
    (miles, (miles * EST_MINUTES_PER_MILE).max(EST_MIN_MINUTES))
}

/// Drive times between every source and destination.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveMatrix {
    /// `cells[i][j]`: source `i` to destination `j`, in input order; `None` when either
    /// coordinate is invalid (see [`LatLon::is_valid`]).
    pub cells: Vec<Vec<Option<DriveTime>>>,
    pub traffic_buffer: f64,
    /// Dataset version reported by the router (`osrm-extract --data_version`), if any.
    pub data_version: Option<String>,
    /// Cells answered by the router, including cache hits.
    pub routed: usize,
    /// Cells filled with the road-distance estimate.
    pub estimated: usize,
    /// `table` requests sent for this matrix (0 when everything came from the cache).
    pub requests: usize,
    /// Router and configuration problems, in English, for module warnings.
    pub warnings: Vec<String>,
    /// Honest source attribution(s) for the routing service(s) that answered cells in this matrix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributions: Vec<SourceItem>,
}

impl DriveMatrix {
    /// Citations for the routing service(s) that actually answered routed cells in this matrix.
    pub fn attributions(&self) -> &[SourceItem] {
        &self.attributions
    }

    /// True when every routed cell in this matrix was answered by self-hosted OSRM.
    pub fn used_only_self_hosted_osrm(&self) -> bool {
        !self.attributions.is_empty()
            && self
                .attributions
                .iter()
                .all(|s| s.name == OSRM_ATTRIBUTION)
    }
}

/// Commute times for the scoring engine: every home against every place.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommuteMatrix {
    /// `times[h][p]`: home `h` and place `p`, in input order; `None` when either coordinate is
    /// invalid. OSRM measures the drive from the place to the home (places are the table's
    /// sources, plan section 5.9); in free flow the two directions usually differ only slightly.
    pub times: Vec<Vec<Option<DriveTime>>>,
    pub traffic_buffer: f64,
    pub data_version: Option<String>,
    pub routed: usize,
    pub estimated: usize,
    pub requests: usize,
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributions: Vec<SourceItem>,
}

impl CommuteMatrix {
    pub fn attributions(&self) -> &[SourceItem] {
        &self.attributions
    }

    /// Transposes a places × homes [`DriveMatrix`] into homes × places.
    fn from_places_by_homes(m: DriveMatrix, n_homes: usize) -> Self {
        let times = (0..n_homes)
            .map(|h| m.cells.iter().map(|row| row[h]).collect())
            .collect();
        Self {
            times,
            traffic_buffer: m.traffic_buffer,
            data_version: m.data_version,
            routed: m.routed,
            estimated: m.estimated,
            requests: m.requests,
            warnings: m.warnings,
            attributions: m.attributions,
        }
    }
}

// ------------------------------------------------------------------------------------------
// Batching
// ------------------------------------------------------------------------------------------

fn source_chunk_len(max_locations: usize) -> usize {
    MAX_SOURCES_PER_REQUEST.min(max_locations.max(MIN_TABLE_LOCATIONS) - 1)
}

fn destination_chunk_len(max_locations: usize, sources_in_chunk: usize) -> usize {
    (max_locations.max(MIN_TABLE_LOCATIONS) - sources_in_chunk).max(1)
}

/// Splits an `n_sources × n_destinations` table into requests whose sources plus destinations
/// never exceed `max_locations`: sources go in chunks of up to [`MAX_SOURCES_PER_REQUEST`], and
/// each chunk takes as many destinations as still fit. Returns `(sources, destinations)` index
/// ranges in request order.
pub fn plan_batches(
    n_sources: usize,
    n_destinations: usize,
    max_locations: usize,
) -> Vec<(Range<usize>, Range<usize>)> {
    let mut out = Vec::new();
    if n_sources == 0 || n_destinations == 0 {
        return out;
    }
    let src_len = source_chunk_len(max_locations);
    for s0 in (0..n_sources).step_by(src_len) {
        let s1 = (s0 + src_len).min(n_sources);
        let dst_len = destination_chunk_len(max_locations, s1 - s0);
        for d0 in (0..n_destinations).step_by(dst_len) {
            out.push((s0..s1, d0..(d0 + dst_len).min(n_destinations)));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------
// Cache
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CellKey {
    router: Arc<str>,
    profile: &'static str,
    data_version: Arc<str>,
    from: (i32, i32),
    to: (i32, i32),
}

/// A cell resolved for a unique (source, destination) pair.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Resolved {
    Routed {
        seconds: f64,
        meters: f64,
        provider: RoutingProviderKind,
    },
    Estimate(EstimateReason, FallbackReason),
}

/// Routed cells and `outside_coverage` cells depend only on the dataset, so both are cached;
/// `router_down` and `router_unset` never are.
static CELL_CACHE: Lazy<Cache<CellKey, Resolved>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(CACHE_CAPACITY)
        .time_to_live(CACHE_TTL)
        .build()
});

const DRIVE_TIME_MATRIX_TTL_SECS: u64 = 30 * 86_400;

fn drive_matrix_dest_hash(dst_keys: &[(i32, i32)]) -> String {
    let mut sorted = dst_keys.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hasher = Sha256::new();
    hasher.update(b"driving|");
    for (lat5, lon5) in &sorted {
        hasher.update(format!("{lat5},{lon5};").as_bytes());
    }
    let hex_digest = format!("{:x}", hasher.finalize());
    hex_digest[..32].to_string()
}

fn encode_drive_entry_id(provider: RoutingProviderKind, dlat5: i32, dlon5: i32) -> String {
    format!(
        "{}|{:.5},{:.5}",
        provider.extension_id(),
        f64::from(dlat5) / 1e5,
        f64::from(dlon5) / 1e5
    )
}

fn decode_entry_provider(entry_id: &str, matrix_provider_id: &str) -> RoutingProviderKind {
    entry_id
        .split_once('|')
        .and_then(|(prefix, _)| RoutingProviderKind::from_id_or_alias(prefix))
        .or_else(|| RoutingProviderKind::from_id_or_alias(matrix_provider_id))
        .unwrap_or(RoutingProviderKind::Osrm)
}

async fn load_drive_matrix_from_shared_store(
    db: &DbStore,
    gh7: &str,
    dest_hash: &str,
    dst_keys: &[(i32, i32)],
    extension_hub: &ExtensionHub,
    osrm_active: bool,
    cloud_active: bool,
) -> Vec<(usize, Resolved)> {
    let Ok(Some((_row, proto))) = get_shared_dataset_proto::<DriveTimeMatrixProto>(
        Some(SharedExtensionCache::global()),
        db,
        dataset_ids::DRIVE_TIME_MATRIX,
        ShareScope::Geohash7,
        gh7,
        dest_hash,
    )
    .await
    else {
        return Vec::new();
    };

    let dst_lookup: HashMap<(i32, i32), usize> = dst_keys
        .iter()
        .enumerate()
        .map(|(idx, &key)| (key, idx))
        .collect();
    let mut resolved = Vec::with_capacity(proto.entries.len());
    for entry in &proto.entries {
        if !entry.duration_sec.is_finite()
            || !entry.distance_m.is_finite()
            || !entry.destination_lat.is_finite()
            || !entry.destination_lon.is_finite()
        {
            continue;
        }
        let dkey = (
            (entry.destination_lat * 1e5).round() as i32,
            (entry.destination_lon * 1e5).round() as i32,
        );
        let Some(&d_idx) = dst_lookup.get(&dkey) else {
            continue;
        };
        let provider = decode_entry_provider(&entry.destination_id, &proto.provider_id);
        let provider_allowed = match provider {
            RoutingProviderKind::Osrm => osrm_active,
            _ => cloud_active && extension_hub.is_enabled(provider.extension_id()),
        };
        if !provider_allowed {
            continue;
        }
        resolved.push((
            d_idx,
            Resolved::Routed {
                seconds: entry.duration_sec,
                meters: entry.distance_m,
                provider,
            },
        ));
    }
    resolved
}

async fn persist_drive_matrix_to_shared_store(
    db: &DbStore,
    gh7: &str,
    origin: (i32, i32),
    dest_hash: &str,
    row: &[Option<Resolved>],
    dst_keys: &[(i32, i32)],
) {
    let origin_lat = f64::from(origin.0) / 1e5;
    let origin_lon = f64::from(origin.1) / 1e5;
    let mut entries = Vec::new();
    let mut primary_provider: Option<RoutingProviderKind> = None;

    for (d, cell_opt) in row.iter().enumerate() {
        let Some(Resolved::Routed {
            seconds,
            meters,
            provider,
        }) = cell_opt
        else {
            continue;
        };
        if primary_provider.is_none() {
            primary_provider = Some(*provider);
        }
        let (dlat5, dlon5) = dst_keys[d];
        entries.push(DriveTimeEntryProto {
            destination_id: encode_drive_entry_id(*provider, dlat5, dlon5),
            destination_label: format!("{:.5},{:.5}", f64::from(dlat5) / 1e5, f64::from(dlon5) / 1e5),
            destination_lat: f64::from(dlat5) / 1e5,
            destination_lon: f64::from(dlon5) / 1e5,
            duration_sec: *seconds,
            distance_m: *meters,
        });
    }
    if entries.is_empty() {
        return;
    }
    let provider_id = primary_provider
        .unwrap_or(RoutingProviderKind::Osrm)
        .extension_id()
        .to_string();
    let proto = DriveTimeMatrixProto {
        origin_geohash7: gh7.to_string(),
        origin_lat,
        origin_lon,
        profile: "driving".to_string(),
        provider_id: provider_id.clone(),
        entries,
        fetched_at_rfc3339: chrono::Utc::now().to_rfc3339(),
    };
    let bbox = decode_geohash_bbox(gh7);
    let geo_cols = GeoIndexColumns {
        geohash5: encode_geohash(origin_lat, origin_lon, 5),
        geohash6: encode_geohash(origin_lat, origin_lon, 6),
        geohash7: Some(gh7.to_string()),
        bbox_south: bbox.map(|(s, _, _, _)| s),
        bbox_west: bbox.map(|(_, w, _, _)| w),
        bbox_north: bbox.map(|(_, _, n, _)| n),
        bbox_east: bbox.map(|(_, _, _, e)| e),
        ..Default::default()
    };
    let provenance_json = serde_json::json!({
        "dataset": dataset_ids::DRIVE_TIME_MATRIX,
        "scope": "geohash7",
        "originGeohash7": gh7,
        "providerId": provider_id,
    })
    .to_string();
    if let Err(err) = put_shared_dataset_proto(
        Some(SharedExtensionCache::global()),
        db,
        dataset_ids::DRIVE_TIME_MATRIX,
        ShareScope::Geohash7,
        gh7,
        dest_hash,
        geo_cols,
        &proto,
        None,
        Some(provenance_json),
        None,
        DRIVE_TIME_MATRIX_TTL_SECS,
    )
    .await
    {
        tracing::warn!(geohash7 = gh7, error = %err, "commute: failed to persist DRIVE_TIME_MATRIX");
    }
}

/// Last dataset version each router reported (empty string: the router reports none).
static DATA_VERSIONS: Lazy<DashMap<Arc<str>, Arc<str>>> = Lazy::new(DashMap::new);

#[derive(Debug, Clone)]
struct RouterCooldownState {
    until: Instant,
    warning: String,
}

/// Short per-router outage cooldown keyed by normalized router base URL (or `<ext_id>|<base_url>`).
static ROUTER_COOLDOWNS: Lazy<DashMap<Arc<str>, RouterCooldownState>> = Lazy::new(DashMap::new);

pub(crate) fn active_router_cooldown(base: &str) -> Option<String> {
    let key: Arc<str> = Arc::from(base);
    if let Some(entry) = ROUTER_COOLDOWNS.get(&key) {
        if entry.until > Instant::now() {
            return Some(entry.warning.clone());
        }
    }
    None
}

/// Returns `true` when `base` is currently inside a failure cooldown window.
pub fn is_router_in_cooldown(base: &str) -> bool {
    active_router_cooldown(base).is_some()
}

/// Immediately clears any failure cooldown for `base`.
pub fn clear_router_cooldown(base: &str) {
    let key: Arc<str> = Arc::from(base);
    ROUTER_COOLDOWNS.remove(&key);
}

pub(crate) fn record_router_cooldown(base: &str, cooldown_secs: u64, warning: &str) {
    if cooldown_secs == 0 {
        return;
    }
    let now = Instant::now();
    if ROUTER_COOLDOWNS.len() > 1024 {
        ROUTER_COOLDOWNS.retain(|_, st| st.until > now);
    }
    let key: Arc<str> = Arc::from(base);
    ROUTER_COOLDOWNS.insert(
        key,
        RouterCooldownState {
            until: now + Duration::from_secs(cooldown_secs),
            warning: warning.to_string(),
        },
    );
}

fn extract_http_status_from_error(msg: &str) -> Option<u16> {
    let (_, tail) = msg.split_once("returned HTTP ")?;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u16>().ok()
}

fn is_cooldown_worthy_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    if msg.contains("offline test mode") {
        return false;
    }
    if let Some(code) = extract_http_status_from_error(&msg) {
        return (500..=599).contains(&code);
    }
    true
}

/// Health probe outcome for one configured OSRM regional (or default) router.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsrmRegionProbe {
    pub name: String,
    pub base_url: String,
    pub healthy: bool,
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn cell_key(router: &Arc<str>, version: &Arc<str>, from: (i32, i32), to: (i32, i32)) -> CellKey {
    CellKey {
        router: router.clone(),
        profile: PROFILE,
        data_version: version.clone(),
        from,
        to,
    }
}

// ------------------------------------------------------------------------------------------
// OSRM table requests and responses
// ------------------------------------------------------------------------------------------

/// The parts of an OSRM `table` response Open Estate uses.
#[derive(Debug, Clone, PartialEq)]
struct TableResponse {
    durations: Vec<Vec<Option<f64>>>,
    distances: Vec<Vec<Option<f64>>>,
    source_snap_m: Vec<Option<f64>>,
    destination_snap_m: Vec<Option<f64>>,
    data_version: Option<String>,
}

impl TableResponse {
    fn resolve(&self, i: usize, j: usize, max_snap_m: f64) -> Resolved {
        let far = |snap: Option<f64>| snap.is_some_and(|m| m > max_snap_m);
        if far(self.source_snap_m[i]) || far(self.destination_snap_m[j]) {
            return Resolved::Estimate(
                EstimateReason::OutsideCoverage,
                FallbackReason::OutsideCoverage,
            );
        }
        match (self.durations[i][j], self.distances[i][j]) {
            (Some(seconds), Some(meters)) => Resolved::Routed {
                seconds,
                meters,
                provider: RoutingProviderKind::Osrm,
            },
            _ => Resolved::Estimate(
                EstimateReason::OutsideCoverage,
                FallbackReason::OutsideCoverage,
            ),
        }
    }
}

fn resolve_cloud_cell(
    parsed: &ParsedCloudMatrix,
    i: usize,
    j: usize,
    max_snap_m: f64,
    provider: RoutingProviderKind,
) -> Resolved {
    let far = |snap: Option<f64>| snap.is_some_and(|m| m > max_snap_m);
    if far(parsed.source_snap_m.get(i).copied().flatten())
        || far(parsed.destination_snap_m.get(j).copied().flatten())
    {
        return Resolved::Estimate(
            EstimateReason::OutsideCoverage,
            FallbackReason::OutsideCoverage,
        );
    }
    match (parsed.durations[i][j], parsed.distances[i][j]) {
        (Some(seconds), Some(meters)) => Resolved::Routed {
            seconds,
            meters,
            provider,
        },
        _ => Resolved::Estimate(
            EstimateReason::OutsideCoverage,
            FallbackReason::OutsideCoverage,
        ),
    }
}

fn parse_matrix(v: Option<&Value>, rows: usize, cols: usize) -> Option<Vec<Vec<Option<f64>>>> {
    let arr = v?.as_array()?;
    if arr.len() != rows {
        return None;
    }
    arr.iter()
        .map(|row| {
            let row = row.as_array()?;
            if row.len() != cols {
                return None;
            }
            Some(
                row.iter()
                    .map(|c| c.as_f64().filter(|x| x.is_finite() && *x >= 0.0))
                    .collect(),
            )
        })
        .collect()
}

fn parse_snaps(v: Option<&Value>, n: usize) -> Vec<Option<f64>> {
    let arr = v.and_then(Value::as_array);
    (0..n)
        .map(|i| {
            arr.and_then(|a| a.get(i))
                .and_then(|w| w.get("distance"))
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite())
        })
        .collect()
}

fn parse_table(j: &Value, rows: usize, cols: usize) -> Result<TableResponse, String> {
    let code = j.get("code").and_then(Value::as_str).unwrap_or("");
    if code != "Ok" {
        let message: String = j
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        return Err(format!("OSRM table returned code {code:?} {message}")
            .trim()
            .to_string());
    }
    let shape_err = |what: &str| format!("OSRM table response has no {rows}×{cols} {what} matrix");
    Ok(TableResponse {
        durations: parse_matrix(j.get("durations"), rows, cols)
            .ok_or_else(|| shape_err("durations"))?,
        distances: parse_matrix(j.get("distances"), rows, cols)
            .ok_or_else(|| shape_err("distances"))?,
        source_snap_m: parse_snaps(j.get("sources"), rows),
        destination_snap_m: parse_snaps(j.get("destinations"), cols),
        data_version: j
            .get("data_version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// `{base}/table/v1/driving/{lon,lat;...}?sources=..&destinations=..`: sources first, then
/// destinations, each coordinate at the 5-decimal cache precision.
fn table_url(base: &str, sources: &[(i32, i32)], destinations: &[(i32, i32)]) -> String {
    table_url_for_profile(base, PROFILE, sources, destinations)
}

fn table_url_for_profile(
    base: &str,
    profile: &str,
    sources: &[(i32, i32)],
    destinations: &[(i32, i32)],
) -> String {
    use std::fmt::Write as _;
    let n = sources.len() + destinations.len();
    let mut url = String::with_capacity(base.len() + 96 + n * 26);
    let _ = write!(url, "{base}/table/v1/{profile}/");
    for (i, (lat, lon)) in sources.iter().chain(destinations).enumerate() {
        if i > 0 {
            url.push(';');
        }
        let _ = write!(url, "{:.5},{:.5}", f64::from(*lon) / 1e5, f64::from(*lat) / 1e5);
    }
    let join = |r: Range<usize>| r.map(|i| i.to_string()).collect::<Vec<_>>().join(";");
    let _ = write!(
        url,
        "?sources={}&destinations={}&annotations=duration,distance&generate_hints=false",
        join(0..sources.len()),
        join(sources.len()..n)
    );
    url
}

fn describe_failure(err: &anyhow::Error, url: &str) -> String {
    let mut msg = sanitize_user_warning(&err.to_string().replace(url, "table request"));
    if msg.chars().count() > 200 {
        msg = msg.chars().take(200).collect::<String>() + "…";
    }
    let hint = if msg.contains("HTTP 400") {
        " (check that OSRM_MAX_TABLE_LOCATIONS does not exceed the server's --max-table-size)"
    } else {
        ""
    };
    format!("OSRM router unavailable: {msg}{hint}; drive times are labelled estimates")
}

/// Groups unresolved `(src_idx, dst_idx)` pairs into `(sources, destinations)` index chunks such
/// that `sources.len() <= MAX_SOURCES_PER_REQUEST` and `sources.len() + destinations.len() <= max_locations`.
fn plan_pair_chunks(
    unresolved: &[(usize, usize)],
    max_locations: usize,
) -> Vec<(Vec<usize>, Vec<usize>)> {
    if unresolved.is_empty() {
        return Vec::new();
    }
    let max = max_locations.max(MIN_TABLE_LOCATIONS);
    let src_len = source_chunk_len(max);
    let mut dst_by_src: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for &(s, d) in unresolved {
        dst_by_src.entry(s).or_default().insert(d);
    }
    let active_srcs: Vec<usize> = dst_by_src.keys().copied().collect();
    let mut chunks = Vec::new();
    for src_chunk in active_srcs.chunks(src_len) {
        let mut missing = BTreeSet::new();
        for s in src_chunk {
            if let Some(ds) = dst_by_src.get(s) {
                missing.extend(ds.iter().copied());
            }
        }
        let missing_vec: Vec<usize> = missing.into_iter().collect();
        for dst_chunk in missing_vec.chunks(destination_chunk_len(max, src_chunk.len())) {
            chunks.push((src_chunk.to_vec(), dst_chunk.to_vec()));
        }
    }
    chunks
}

/// Live 1×1 OSRM `table` probe helper invoked by `ExtensionHub::test_fetch` for `osrm_routing`.
pub(crate) async fn probe_osrm_table_pair(
    http: &HttpClient,
    base: &str,
    sources: &[(i32, i32)],
    destinations: &[(i32, i32)],
    timeout_ms: u64,
    attempted_requests: &AtomicUsize,
) -> Result<(Option<f64>, Option<f64>), String> {
    let url = table_url(base, sources, destinations);
    let client = http
        .direct_client_for_url(&url)
        .map_err(|e| describe_failure(&e, &url))?;
    let sem = http.host_semaphore(&url);
    let _permit = sem.acquire_owned().await.ok();
    attempted_requests.fetch_add(1, Ordering::Relaxed);

    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)))
        .send()
        .await
        .map_err(|e| {
            let code_lbl = if e.is_timeout() { "TIMEOUT" } else { "TRANSPORT" };
            record_routing_failure(RoutingProviderKind::Osrm, code_lbl);
            describe_failure(&e.into(), &url)
        })?;

    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        record_routing_failure(RoutingProviderKind::Osrm, &format!("HTTP_{status}"));
        let e = anyhow::anyhow!("{} returned HTTP {status}", url_host_label(&url));
        return Err(describe_failure(&e, &url));
    }
    let json: Value = resp.json().await.map_err(|e| {
        record_routing_failure(RoutingProviderKind::Osrm, "INVALID_JSON");
        let err = anyhow::anyhow!("invalid JSON from {}: {e}", url_host_label(&url));
        describe_failure(&err, &url)
    })?;
    let table = parse_table(&json, sources.len(), destinations.len()).map_err(|e| {
        record_routing_failure(RoutingProviderKind::Osrm, "OSRM_TABLE_ERROR");
        format!("OSRM router error: {e}")
    })?;
    clear_router_cooldown(base);
    record_routing_success(RoutingProviderKind::Osrm);
    let dur = table.durations.first().and_then(|r| r.first()).copied().flatten();
    let dist = table.distances.first().and_then(|r| r.first()).copied().flatten();
    Ok((dur, dist))
}

// ------------------------------------------------------------------------------------------
// Client
// ------------------------------------------------------------------------------------------

/// One table request: target router URL and indices into the unique sources and destinations.
struct Job {
    base: String,
    sources: Vec<usize>,
    destinations: Vec<usize>,
}

/// Routing client for Tier-1 self-hosted OSRM servers (`osrm_routing`, with multi-region cascade)
/// and Tier-2 load-balanced cloud fallback matrix services (`mapbox_routing`, `locationiq_routing`,
/// `freeroute_routing`, `openrouteservice_routing`). Cheap to clone and to construct per call;
/// the OSRM cell cache is process-wide.
#[derive(Clone)]
pub struct RoutingClient {
    http: HttpClient,
    config: RoutingConfig,
    extension_hub: Arc<ExtensionHub>,
    db: Option<DbStore>,
}

impl RoutingClient {
    pub fn new(http: HttpClient, config: RoutingConfig) -> Self {
        Self {
            http,
            config,
            extension_hub: Arc::clone(ExtensionHub::global()),
            db: None,
        }
    }

    /// Uses the `OSRM_*` + cloud fallback settings captured by [`HttpClient::from_env`]. Clients
    /// built otherwise carry no settings, so they never call a router.
    pub fn from_http(http: &HttpClient) -> Self {
        Self::new(http.clone(), RoutingConfig::from_env(http.env()))
    }

    /// Binds an isolated [`ExtensionHub`] (e.g. `AppState.extension_hub` or a per-test hub) so
    /// runtime extension enable/disable toggles immediately govern Tier-1 and Tier-2 routing.
    pub fn with_extension_hub(mut self, hub: Arc<ExtensionHub>) -> Self {
        self.extension_hub = hub;
        self
    }

    /// Binds an explicit [`DbStore`] for persistent `DRIVE_TIME_MATRIX` read/write in `shared_data_records`.
    pub fn with_db(mut self, db: DbStore) -> Self {
        self.db = Some(db);
        self
    }

    pub fn extension_hub(&self) -> &Arc<ExtensionHub> {
        &self.extension_hub
    }

    /// Overrides the traffic buffer (e.g. the buyer's own), clamped to 1.0–3.0.
    pub fn with_traffic_buffer(mut self, traffic_buffer: f64) -> Self {
        if traffic_buffer.is_finite() {
            self.config.traffic_buffer =
                traffic_buffer.clamp(MIN_TRAFFIC_BUFFER, MAX_TRAFFIC_BUFFER);
            self.config.traffic_buffer_overridden = true;
        }
        self
    }

    pub fn config(&self) -> &RoutingConfig {
        &self.config
    }

    /// Commute times for every home against every place (up to [`MAX_SOURCES_PER_REQUEST`]
    /// places per request; more are split into further requests).
    pub async fn commute_times(&self, homes: &[LatLon], places: &[LatLon]) -> CommuteMatrix {
        let m = self.drive_matrix(places, homes).await;
        CommuteMatrix::from_places_by_homes(m, homes.len())
    }

    /// Drive times from every source to every destination. Never fails: cells no healthy router
    /// can answer are labelled estimates.
    pub async fn drive_matrix(&self, sources: &[LatLon], destinations: &[LatLon]) -> DriveMatrix {
        let buffer = self.config.traffic_buffer;
        let mut warnings = self.config.issues.clone();
        let (src_keys, src_map) = unique_keys(sources);
        let (dst_keys, dst_map) = unique_keys(destinations);
        let mut grid: Vec<Vec<Option<Resolved>>> =
            vec![vec![None; dst_keys.len()]; src_keys.len()];
        let mut requests = 0usize;
        let mut data_version: Option<String> = None;

        let mod_enabled =
            crate::modules::routing_providers::is_routing_module_enabled(&self.extension_hub);
        let osrm_active = mod_enabled
            && self.extension_hub.is_enabled("osrm_routing")
            && self.config.is_enabled();
        let cloud_active = mod_enabled
            && self
                .config
                .cloud
                .providers
                .iter()
                .any(|ep| self.extension_hub.is_enabled(ep.kind.extension_id()));

        let mut cell_candidates: Vec<Vec<Vec<String>>> =
            vec![vec![Vec::new(); dst_keys.len()]; src_keys.len()];
        let mut tier1_failure_warnings: Vec<String> = Vec::new();
        let mut cloud_failure_reasons: Vec<String> = Vec::new();
        let db_opt = self.db.clone().or_else(|| self.extension_hub.active_db());

        if !src_keys.is_empty() && !dst_keys.is_empty() {
            let dest_hash = drive_matrix_dest_hash(&dst_keys);
            let origin_geohashes: Vec<Option<String>> = src_keys
                .iter()
                .map(|&(slat, slon)| {
                    encode_geohash(f64::from(slat) / 1e5, f64::from(slon) / 1e5, 7)
                })
                .collect();

            if let Some(ref db_store) = db_opt {
                for (s, gh7_opt) in origin_geohashes.iter().enumerate() {
                    let Some(ref gh7) = gh7_opt else {
                        continue;
                    };
                    for (d, cell) in load_drive_matrix_from_shared_store(
                        db_store,
                        gh7,
                        &dest_hash,
                        &dst_keys,
                        &self.extension_hub,
                        osrm_active,
                        cloud_active,
                    )
                    .await
                    {
                        grid[s][d] = Some(cell);
                    }
                }
            }

            let had_cell_before: Vec<Vec<bool>> = grid
                .iter()
                .map(|row| row.iter().map(Option::is_some).collect())
                .collect();

            // ------------------------------------------------------------------
            // Tier 1: Self-hosted OSRM with ascending-bbox-area regional cascade
            // ------------------------------------------------------------------
            if osrm_active {
                for (s, &(slat, slon)) in src_keys.iter().enumerate() {
                    let from = LatLon::new(f64::from(slat) / 1e5, f64::from(slon) / 1e5);
                    for (d, &(dlat, dlon)) in dst_keys.iter().enumerate() {
                        let to = LatLon::new(f64::from(dlat) / 1e5, f64::from(dlon) / 1e5);
                        cell_candidates[s][d] = self.config.resolve_endpoints(from, to);
                    }
                }

                let mut cell_cand_idx: Vec<Vec<usize>> =
                    vec![vec![0usize; dst_keys.len()]; src_keys.len()];

                loop {
                    let mut by_router: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
                    let mut router_versions: HashMap<String, Option<Arc<str>>> = HashMap::new();

                    for s in 0..src_keys.len() {
                        for d in 0..dst_keys.len() {
                            if grid[s][d].is_some() {
                                continue;
                            }
                            let cands = &cell_candidates[s][d];
                            while cell_cand_idx[s][d] < cands.len() {
                                let base = &cands[cell_cand_idx[s][d]];
                                let router: Arc<str> = Arc::from(base.as_str());
                                let known_version =
                                    DATA_VERSIONS.get(&router).map(|v| v.value().clone());
                                if let Some(ref version) = known_version {
                                    if data_version.is_none() && !version.is_empty() {
                                        data_version = Some(version.to_string());
                                    }
                                    if let Some(hit) = CELL_CACHE.get(&cell_key(
                                        &router,
                                        version,
                                        src_keys[s],
                                        dst_keys[d],
                                    )) {
                                        grid[s][d] = Some(hit);
                                        break;
                                    }
                                }
                                if self.config.failure_cooldown_secs > 0 {
                                    if let Some(cached_warn) = active_router_cooldown(base) {
                                        if !tier1_failure_warnings.contains(&cached_warn) {
                                            tier1_failure_warnings.push(cached_warn);
                                        }
                                        cell_cand_idx[s][d] += 1;
                                        continue;
                                    }
                                }
                                router_versions
                                    .entry(base.clone())
                                    .or_insert(known_version);
                                by_router.entry(base.clone()).or_default().push((s, d));
                                break;
                            }
                        }
                    }

                    if by_router.is_empty() {
                        break;
                    }

                    let mut jobs: Vec<Job> = Vec::new();
                    for (base, pairs) in &by_router {
                        jobs.extend(self.plan_jobs_for_router(base, pairs));
                    }

                    let attempted_requests = AtomicUsize::new(0);
                    let attempted_ref = &attempted_requests;
                    let (src_ref, dst_ref) = (&src_keys, &dst_keys);
                    let results: Vec<(Job, Result<TableResponse, String>)> = stream::iter(jobs)
                        .map(|job| async move {
                            let from: Vec<_> = job.sources.iter().map(|&s| src_ref[s]).collect();
                            let to: Vec<_> = job.destinations.iter().map(|&d| dst_ref[d]).collect();
                            let res = self.fetch_table(&job.base, &from, &to, attempted_ref).await;
                            (job, res)
                        })
                        .buffer_unordered(MAX_CONCURRENT_REQUESTS)
                        .collect()
                        .await;
                    requests += attempted_requests.load(Ordering::Relaxed);

                    for (job, res) in results {
                        let router: Arc<str> = Arc::from(job.base.as_str());
                        let known_version = router_versions.get(&job.base).cloned().flatten();
                        match res {
                            Ok(table) => {
                                let version: Arc<str> =
                                    Arc::from(table.data_version.as_deref().unwrap_or(""));
                                if known_version.as_ref() != Some(&version) {
                                    DATA_VERSIONS.insert(router.clone(), version.clone());
                                }
                                if let Some(ref dv) = table.data_version {
                                    data_version = Some(dv.clone());
                                }
                                for (bi, &s) in job.sources.iter().enumerate() {
                                    for (bj, &d) in job.destinations.iter().enumerate() {
                                        if grid[s][d].is_some() {
                                            continue;
                                        }
                                        let active_cand = cell_candidates[s][d]
                                            .get(cell_cand_idx[s][d])
                                            .map(String::as_str);
                                        if active_cand != Some(job.base.as_str()) {
                                            continue;
                                        }
                                        let cell = table.resolve(bi, bj, self.config.max_snap_m);
                                        let key =
                                            cell_key(&router, &version, src_keys[s], dst_keys[d]);
                                        CELL_CACHE.insert(key, cell);
                                        grid[s][d] = Some(cell);
                                    }
                                }
                            }
                            Err(msg) => {
                                if !tier1_failure_warnings.contains(&msg) {
                                    tier1_failure_warnings.push(msg);
                                }
                                for &s in &job.sources {
                                    for &d in &job.destinations {
                                        if grid[s][d].is_some() {
                                            continue;
                                        }
                                        let active_cand = cell_candidates[s][d]
                                            .get(cell_cand_idx[s][d])
                                            .map(String::as_str);
                                        if active_cand == Some(job.base.as_str()) {
                                            cell_cand_idx[s][d] += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ------------------------------------------------------------------
            // Tier 2: Load-balanced Cloud Fallback Pool (invoked ONLY when Tier 1
            // had no matching endpoint OR all matching Tier-1 endpoints failed/cooled)
            // ------------------------------------------------------------------
            if cloud_active {
                let mut unresolved_cloud: Vec<(usize, usize)> = Vec::new();
                for s in 0..src_keys.len() {
                    for d in 0..dst_keys.len() {
                        if grid[s][d].is_none() {
                            unresolved_cloud.push((s, d));
                        }
                    }
                }

                if !unresolved_cloud.is_empty() {
                    let cloud_chunks =
                        plan_pair_chunks(&unresolved_cloud, MAX_CLOUD_MATRIX_LOCATIONS);
                    let rr_counter =
                        round_robin_counter_for_signature(self.config.cloud.pool_signature());
                    let cloud_attempted = AtomicUsize::new(0);

                    for (src_chunk, dst_chunk) in cloud_chunks {
                        let pool =
                            build_eligible_cloud_pool(&self.config.cloud, &self.extension_hub);
                        if pool.is_empty() {
                            break;
                        }
                        let start_idx = match self.config.cloud.strategy {
                            RoutingFallbackStrategy::RoundRobin => {
                                rr_counter.fetch_add(1, Ordering::Relaxed) % pool.len()
                            }
                            RoutingFallbackStrategy::Priority => 0,
                        };
                        let from_pts: Vec<(i32, i32)> =
                            src_chunk.iter().map(|&s| src_keys[s]).collect();
                        let to_pts: Vec<(i32, i32)> =
                            dst_chunk.iter().map(|&d| dst_keys[d]).collect();

                        for offset in 0..pool.len() {
                            let ep = &pool[(start_idx + offset) % pool.len()];
                            if is_cloud_provider_in_cooldown(ep.kind, &ep.base_url)
                                && self.config.cloud.external_cooldown_secs > 0
                            {
                                continue;
                            }
                            match execute_cloud_matrix_request(
                                &self.http,
                                ep,
                                &from_pts,
                                &to_pts,
                                self.config.cloud.external_cooldown_secs,
                                &cloud_attempted,
                            )
                            .await
                            {
                                Ok(parsed) => {
                                    for (bi, &s) in src_chunk.iter().enumerate() {
                                        for (bj, &d) in dst_chunk.iter().enumerate() {
                                            if grid[s][d].is_none() {
                                                grid[s][d] = Some(resolve_cloud_cell(
                                                    &parsed,
                                                    bi,
                                                    bj,
                                                    self.config.max_snap_m,
                                                    ep.kind,
                                                ));
                                            }
                                        }
                                    }
                                    break;
                                }
                                Err(fail) => {
                                    record_routing_failover(ep.kind);
                                    if !cloud_failure_reasons.contains(&fail.sanitized_reason) {
                                        cloud_failure_reasons.push(fail.sanitized_reason);
                                    }
                                }
                            }
                        }
                    }

                    requests += cloud_attempted.load(Ordering::Relaxed);
                }
            }

            if let Some(ref db_store) = db_opt {
                for (s, gh7_opt) in origin_geohashes.iter().enumerate() {
                    let Some(ref gh7) = gh7_opt else {
                        continue;
                    };
                    let any_new_routed = (0..dst_keys.len()).any(|d| {
                        !had_cell_before[s][d] && matches!(grid[s][d], Some(Resolved::Routed { .. }))
                    });
                    if any_new_routed {
                        persist_drive_matrix_to_shared_store(
                            db_store,
                            gh7,
                            src_keys[s],
                            &dest_hash,
                            &grid[s],
                            &dst_keys,
                        )
                        .await;
                    }
                }
            }

            // ------------------------------------------------------------------
            // Finalize any cells still unresolved after Tier 1 + Tier 2
            // ------------------------------------------------------------------
            let mut saw_unmatched_region_unrouted = false;
            let mut saw_osrm_router_down = false;
            let mut saw_cloud_router_down = false;

            for s in 0..src_keys.len() {
                for d in 0..dst_keys.len() {
                    if grid[s][d].is_some() {
                        continue;
                    }
                    let had_osrm_cand = osrm_active && !cell_candidates[s][d].is_empty();
                    if had_osrm_cand {
                        saw_osrm_router_down = true;
                        grid[s][d] = Some(Resolved::Estimate(
                            EstimateReason::RouterDown,
                            FallbackReason::OsrmUnavailable,
                        ));
                    } else if cloud_active {
                        saw_cloud_router_down = true;
                        grid[s][d] = Some(Resolved::Estimate(
                            EstimateReason::RouterDown,
                            FallbackReason::CloudFallbackUnavailable,
                        ));
                    } else if osrm_active {
                        saw_unmatched_region_unrouted = true;
                        let reason = if self.config.regions.is_empty() {
                            EstimateReason::RouterUnset
                        } else {
                            EstimateReason::OutsideCoverage
                        };
                        grid[s][d] = Some(Resolved::Estimate(
                            reason,
                            FallbackReason::NoOsrmRegionConfigured,
                        ));
                    } else {
                        grid[s][d] = Some(Resolved::Estimate(
                            EstimateReason::RouterUnset,
                            FallbackReason::NoOsrmRegionConfigured,
                        ));
                    }
                }
            }

            if saw_unmatched_region_unrouted && !self.config.regions.is_empty() {
                warnings.push(
                    "no_osrm_region_configured: one or more routes fell outside configured OSRM regions; drive times are labelled estimates"
                        .to_string(),
                );
            }

            if saw_osrm_router_down {
                for w in tier1_failure_warnings {
                    if !warnings.contains(&w) {
                        warnings.push(w);
                    }
                }
                if !warnings.iter().any(|w| w.contains("OSRM router unavailable")) {
                    let detail = if cloud_failure_reasons.is_empty() {
                        "all configured routing services are in cooldown or unreachable"
                            .to_string()
                    } else {
                        cloud_failure_reasons.join("; ")
                    };
                    let clean_detail = redact_routing_error(&detail, &[]);
                    warnings.push(format!(
                        "OSRM router unavailable: {clean_detail}; drive times are labelled estimates"
                    ));
                }
            } else if saw_cloud_router_down {
                let detail = if cloud_failure_reasons.is_empty() {
                    "configured fallback routing services are in cooldown or unreachable".to_string()
                } else {
                    cloud_failure_reasons.join("; ")
                };
                let clean_detail = redact_routing_error(&detail, &[]);
                warnings.push(format!(
                    "Cloud routing fallback unavailable: {clean_detail}; drive times are labelled estimates"
                ));
            }
        }

        if warnings.len() > MAX_WARNINGS {
            let more = warnings.len() - MAX_WARNINGS;
            warnings.truncate(MAX_WARNINGS);
            warnings.push(format!("…and {more} more routing warnings"));
        }

        let (fallback_reason, fallback_detail) = if osrm_active {
            (EstimateReason::RouterDown, FallbackReason::OsrmUnavailable)
        } else if cloud_active {
            (
                EstimateReason::RouterDown,
                FallbackReason::CloudFallbackUnavailable,
            )
        } else {
            (
                EstimateReason::RouterUnset,
                FallbackReason::NoOsrmRegionConfigured,
            )
        };

        let (mut routed, mut estimated) = (0, 0);
        let mut used_providers: BTreeSet<RoutingProviderKind> = BTreeSet::new();
        let mut cells = Vec::with_capacity(sources.len());
        for (from, s) in sources.iter().zip(&src_map) {
            let mut row = Vec::with_capacity(destinations.len());
            for (to, d) in destinations.iter().zip(&dst_map) {
                let (Some(s), Some(d)) = (s, d) else {
                    row.push(None);
                    continue;
                };
                let cell = match grid[*s][*d]
                    .unwrap_or(Resolved::Estimate(fallback_reason, fallback_detail))
                {
                    Resolved::Routed {
                        seconds,
                        meters,
                        provider,
                    } => {
                        routed += 1;
                        used_providers.insert(provider);
                        let cell_buf = provider
                            .effective_traffic_buffer(buffer, self.config.traffic_buffer_overridden);
                        DriveTime::routed(seconds, meters, cell_buf)
                    }
                    Resolved::Estimate(reason, detail) => {
                        estimated += 1;
                        DriveTime::estimate(*from, *to, buffer, reason, detail)
                    }
                };
                row.push(Some(cell));
            }
            cells.push(row);
        }

        let matrix_traffic_buffer = if used_providers.len() == 1 {
            let only = *used_providers.iter().next().expect("single provider");
            only.effective_traffic_buffer(buffer, self.config.traffic_buffer_overridden)
        } else {
            buffer
        };

        let attributions: Vec<SourceItem> = used_providers
            .into_iter()
            .map(RoutingProviderKind::report_source_item)
            .collect();

        DriveMatrix {
            cells,
            traffic_buffer: matrix_traffic_buffer,
            data_version,
            routed,
            estimated,
            requests,
            warnings,
            attributions,
        }
    }

    /// Requests for every unresolved cell belonging to `base`: per chunk of up to five unique
    /// sources, the destinations still missing for any of them, in chunks that keep each request
    /// within `max_table_locations`.
    fn plan_jobs_for_router(&self, base: &str, unresolved: &[(usize, usize)]) -> Vec<Job> {
        plan_pair_chunks(unresolved, self.config.max_table_locations)
            .into_iter()
            .map(|(sources, destinations)| Job {
                base: base.to_string(),
                sources,
                destinations,
            })
            .collect()
    }

    async fn fetch_table(
        &self,
        base: &str,
        sources: &[(i32, i32)],
        destinations: &[(i32, i32)],
        attempted_requests: &AtomicUsize,
    ) -> Result<TableResponse, String> {
        if self.config.failure_cooldown_secs > 0 {
            if let Some(cached_warn) = active_router_cooldown(base) {
                return Err(cached_warn);
            }
        }
        let url = table_url(base, sources, destinations);
        let client = match self.http.direct_client_for_url(&url) {
            Ok(c) => c,
            Err(e) => {
                let desc = describe_failure(&e, &url);
                return Err(desc);
            }
        };
        let sem = self.http.host_semaphore(&url);
        let _permit = sem.acquire_owned().await.ok();
        attempted_requests.fetch_add(1, Ordering::Relaxed);

        let send_res = client
            .get(&url)
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(self.config.timeout_ms))
            .send()
            .await;

        let json: Value = match send_res {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if !(200..300).contains(&status) {
                    let host_lbl = url_host_label(&url);
                    let e = anyhow::anyhow!("{host_lbl} returned HTTP {status}");
                    let desc = describe_failure(&e, &url);
                    record_routing_failure(RoutingProviderKind::Osrm, &format!("HTTP_{status}"));
                    if is_cooldown_worthy_error(&e) {
                        record_router_cooldown(base, self.config.failure_cooldown_secs, &desc);
                    }
                    return Err(desc);
                }
                match resp.json::<Value>().await {
                    Ok(j) => {
                        clear_router_cooldown(base);
                        j
                    }
                    Err(je) => {
                        let e =
                            anyhow::anyhow!("invalid JSON from {}: {je}", url_host_label(&url));
                        let desc = describe_failure(&e, &url);
                        record_routing_failure(RoutingProviderKind::Osrm, "INVALID_JSON");
                        return Err(desc);
                    }
                }
            }
            Err(re) => {
                let code_lbl = if re.is_timeout() {
                    "TIMEOUT"
                } else {
                    "TRANSPORT"
                };
                let e: anyhow::Error = re.into();
                let desc = describe_failure(&e, &url);
                record_routing_failure(RoutingProviderKind::Osrm, code_lbl);
                if is_cooldown_worthy_error(&e) {
                    record_router_cooldown(base, self.config.failure_cooldown_secs, &desc);
                }
                return Err(desc);
            }
        };

        match parse_table(&json, sources.len(), destinations.len()) {
            Ok(t) => {
                record_routing_success(RoutingProviderKind::Osrm);
                Ok(t)
            }
            Err(e) => {
                record_routing_failure(RoutingProviderKind::Osrm, "OSRM_TABLE_ERROR");
                Err(format!(
                    "OSRM router error: {e}; drive times are labelled estimates"
                ))
            }
        }
    }

    /// Probes every configured regional OSRM router (at its bbox center) plus `OSRM_BASE_URL`
    /// fallback if distinct, via `GET {base}/nearest/v1/driving/{mid_lon},{mid_lat}?number=1`.
    /// Clears any active failure cooldown immediately when a router answers HTTP 200.
    pub async fn probe_regions(&self) -> Vec<OsrmRegionProbe> {
        let mut targets: Vec<(String, String, LatLon)> = Vec::new();
        for r in &self.config.regions {
            targets.push((r.name.clone(), r.base_url.clone(), r.center()));
        }
        if let Some(ref base) = self.config.base_url {
            if !targets.iter().any(|(_, url, _)| url == base) {
                targets.push((
                    "default".to_string(),
                    base.clone(),
                    LatLon::new(37.33382, -121.88634),
                ));
            }
        }
        if targets.is_empty() {
            return Vec::new();
        }

        let timeout_ms = self
            .config
            .timeout_ms
            .min(DEFAULT_PROBE_TIMEOUT_MS)
            .max(MIN_TIMEOUT_MS);
        let cooldown_secs = self.config.failure_cooldown_secs;
        let futs = targets.into_iter().map(|(name, base_url, pt)| {
            let http = self.http.clone();
            async move {
                let url = format!(
                    "{base_url}/nearest/v1/{PROFILE}/{:.5},{:.5}?number=1",
                    pt.lon, pt.lat
                );
                let t0 = Instant::now();
                let client = match http.direct_client_for_url(&url) {
                    Ok(c) => c,
                    Err(e) => {
                        let clean_err = sanitize_user_warning(&e.to_string());
                        return OsrmRegionProbe {
                            name,
                            base_url,
                            healthy: false,
                            status_code: None,
                            latency_ms: t0.elapsed().as_millis() as u64,
                            error: Some(clean_err),
                        };
                    }
                };
                let send_res = client
                    .get(&url)
                    .header("Accept", "application/json")
                    .timeout(Duration::from_millis(timeout_ms))
                    .send()
                    .await;
                let latency_ms = t0.elapsed().as_millis() as u64;
                match send_res {
                    Ok(resp) if resp.status().as_u16() == 200 => {
                        clear_router_cooldown(&base_url);
                        OsrmRegionProbe {
                            name,
                            base_url,
                            healthy: true,
                            status_code: Some(200),
                            latency_ms,
                            error: None,
                        }
                    }
                    Ok(resp) => {
                        let code = resp.status().as_u16();
                        let msg = format!("HTTP {code}");
                        if code >= 500 {
                            record_router_cooldown(
                                &base_url,
                                cooldown_secs,
                                &format!(
                                    "OSRM router unavailable: {msg}; drive times are labelled estimates"
                                ),
                            );
                        }
                        OsrmRegionProbe {
                            name,
                            base_url,
                            healthy: false,
                            status_code: Some(code),
                            latency_ms,
                            error: Some(msg),
                        }
                    }
                    Err(re) => {
                        let e: anyhow::Error = re.into();
                        let clean_err = sanitize_user_warning(&e.to_string());
                        if is_cooldown_worthy_error(&e) {
                            record_router_cooldown(
                                &base_url,
                                cooldown_secs,
                                &format!(
                                    "OSRM router unavailable: {clean_err}; drive times are labelled estimates"
                                ),
                            );
                        }
                        OsrmRegionProbe {
                            name,
                            base_url,
                            healthy: false,
                            status_code: None,
                            latency_ms,
                            error: Some(clean_err),
                        }
                    }
                }
            }
        });
        futures::future::join_all(futs).await
    }

    /// Computes active-mobility (`foot` or `bike`) travel durations and distances from `origin` to
    /// `destinations`. Calls self-hosted `OSRM_FOOT_BASE_URL` / `OSRM_BIKE_BASE_URL` when configured
    /// (public demo hosts are strictly rejected by `is_public_demo_host`), and otherwise falls back
    /// honestly (`estimated: true`) to routed road-network distance from `drive_cells` (when available)
    /// or a detour-adjusted Haversine distance.
    pub async fn active_mobility_row(
        &self,
        mode: ActiveMobilityMode,
        origin: LatLon,
        destinations: &[LatLon],
        drive_cells: Option<&[Option<DriveTime>]>,
        has_bikeway_corridor: bool,
    ) -> Vec<Option<ActiveMobilityTime>> {
        if !origin.is_valid() || destinations.is_empty() {
            return vec![None; destinations.len()];
        }

        let mod_enabled =
            crate::modules::routing_providers::is_routing_module_enabled(&self.extension_hub);
        let ext_enabled = mod_enabled && self.extension_hub.is_enabled("osrm_routing");
        let base_opt = match mode {
            ActiveMobilityMode::Foot => self.config.foot_base_url.as_deref(),
            ActiveMobilityMode::Bike => self.config.bike_base_url.as_deref(),
        };

        let mut routed_map: HashMap<(i32, i32), (f64, f64)> = HashMap::new();
        let mut fallback_reason = EstimateReason::RouterUnset;

        if ext_enabled {
            if let Some(base) = base_opt {
                let (dst_keys, _) = unique_keys(destinations);
                if !dst_keys.is_empty() {
                    let src_key = [origin.key()];
                    let max_dst = self
                        .config
                        .max_table_locations
                        .max(MIN_TABLE_LOCATIONS)
                        .saturating_sub(1)
                        .max(1);
                    let mut router_failed = false;
                    for chunk in dst_keys.chunks(max_dst) {
                        match self
                            .fetch_profile_table(base, mode.osrm_profile(), &src_key, chunk)
                            .await
                        {
                            Ok(table) => {
                                let src_far = table
                                    .source_snap_m
                                    .first()
                                    .copied()
                                    .flatten()
                                    .is_some_and(|m| m > self.config.max_snap_m);
                                if !src_far {
                                    for (j, &dk) in chunk.iter().enumerate() {
                                        let dst_far = table
                                            .destination_snap_m
                                            .get(j)
                                            .copied()
                                            .flatten()
                                            .is_some_and(|m| m > self.config.max_snap_m);
                                        if dst_far {
                                            continue;
                                        }
                                        if let (Some(sec), Some(met)) = (
                                            table.durations.first().and_then(|r| r.get(j)).copied().flatten(),
                                            table.distances.first().and_then(|r| r.get(j)).copied().flatten(),
                                        ) {
                                            routed_map.insert(dk, (sec, met));
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                router_failed = true;
                                break;
                            }
                        }
                    }
                    fallback_reason = if router_failed {
                        EstimateReason::RouterDown
                    } else {
                        EstimateReason::OutsideCoverage
                    };
                }
            }
        }

        destinations
            .iter()
            .enumerate()
            .map(|(idx, dst)| {
                if !dst.is_valid() {
                    return None;
                }
                let dk = dst.key();
                if let Some(&(seconds, meters)) = routed_map.get(&dk) {
                    return Some(ActiveMobilityTime {
                        duration_min: (seconds / 60.0).max(1.0),
                        distance_mi: meters / MILE_M,
                        estimated: false,
                        reason: None,
                        basis: "self_hosted_osrm",
                        profile: mode.osrm_profile(),
                    });
                }

                let routed_drive_mi = drive_cells
                    .and_then(|row| row.get(idx))
                    .copied()
                    .flatten()
                    .filter(|c| !c.estimated && c.distance_mi.is_finite() && c.distance_mi > 0.0)
                    .map(|c| c.distance_mi);

                let (dist_mi, basis) = if let Some(d_mi) = routed_drive_mi {
                    (d_mi, "drive_network_distance_scaled")
                } else {
                    let straight_m = haversine_m(origin.lat, origin.lon, dst.lat, dst.lon);
                    let detour = match mode {
                        ActiveMobilityMode::Foot => 1.25,
                        ActiveMobilityMode::Bike => ROAD_DISTANCE_FACTOR,
                    };
                    ((straight_m / MILE_M) * detour, "straight_line_detour_estimate")
                };

                let speed_mph = match mode {
                    ActiveMobilityMode::Foot => 3.0,
                    ActiveMobilityMode::Bike => {
                        if has_bikeway_corridor {
                            11.5
                        } else {
                            10.5
                        }
                    }
                };
                let duration_min = ((dist_mi / speed_mph) * 60.0).max(1.0);

                Some(ActiveMobilityTime {
                    duration_min,
                    distance_mi: dist_mi,
                    estimated: true,
                    reason: Some(fallback_reason),
                    basis,
                    profile: mode.osrm_profile(),
                })
            })
            .collect()
    }

    async fn fetch_profile_table(
        &self,
        base: &str,
        profile: &str,
        sources: &[(i32, i32)],
        destinations: &[(i32, i32)],
    ) -> Result<TableResponse, String> {
        let cooldown_key = format!("{profile}|{base}");
        if self.config.failure_cooldown_secs > 0 {
            if let Some(cached_warn) = active_router_cooldown(&cooldown_key) {
                return Err(cached_warn);
            }
        }
        let url = table_url_for_profile(base, profile, sources, destinations);
        let client = self
            .http
            .direct_client_for_url(&url)
            .map_err(|e| describe_failure(&e, &url))?;
        let sem = self.http.host_semaphore(&url);
        let _permit = sem.acquire_owned().await.ok();

        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(self.config.timeout_ms))
            .send()
            .await
            .map_err(|re| {
                let e: anyhow::Error = re.into();
                let desc = describe_failure(&e, &url);
                if is_cooldown_worthy_error(&e) {
                    record_router_cooldown(&cooldown_key, self.config.failure_cooldown_secs, &desc);
                }
                desc
            })?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let e = anyhow::anyhow!("{} returned HTTP {status}", url_host_label(&url));
            let desc = describe_failure(&e, &url);
            if is_cooldown_worthy_error(&e) {
                record_router_cooldown(&cooldown_key, self.config.failure_cooldown_secs, &desc);
            }
            return Err(desc);
        }
        clear_router_cooldown(&cooldown_key);
        let json: Value = resp.json().await.map_err(|je| {
            let e = anyhow::anyhow!("invalid JSON from {}: {je}", url_host_label(&url));
            describe_failure(&e, &url)
        })?;
        parse_table(&json, sources.len(), destinations.len())
    }
}

/// Active-mobility profile (`foot` or `bike`) for walking and cycling matrices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveMobilityMode {
    Foot,
    Bike,
}

impl ActiveMobilityMode {
    pub fn osrm_profile(self) -> &'static str {
        match self {
            Self::Foot => "foot",
            Self::Bike => "bike",
        }
    }
}

/// Walking or cycling duration & distance cell between two points, with honest `estimated` metadata.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveMobilityTime {
    pub duration_min: f64,
    pub distance_mi: f64,
    pub estimated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<EstimateReason>,
    pub basis: &'static str,
    pub profile: &'static str,
}

/// Summary of mapped neighborhood sidewalks, footways, cycleways, and trails within 800 m from
/// Esri's keyless `OSM_NA_Trails` FeatureServer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveMobilityCorridorSummary {
    pub queried: bool,
    pub status: String,
    pub sidewalk_or_footway_segments_800m: usize,
    pub bikeway_or_cycleway_segments_800m: usize,
    pub lit_pathway_segments_800m: usize,
    pub has_sidewalk_corridor: bool,
    pub has_bikeway_corridor: bool,
    pub named_trails: Vec<String>,
}

impl ActiveMobilityCorridorSummary {
    pub fn skipped(status: &str) -> Self {
        Self {
            queried: false,
            status: status.to_string(),
            sidewalk_or_footway_segments_800m: 0,
            bikeway_or_cycleway_segments_800m: 0,
            lit_pathway_segments_800m: 0,
            has_sidewalk_corridor: false,
            has_bikeway_corridor: false,
            named_trails: Vec::new(),
        }
    }
}

/// Queries keyless Esri `OSM_NA_Trails` within 800 m of `(lat, lon)` to check sidewalk/footway
/// and dedicated bikeway/cycleway corridor presence. Respects `http.is_offline_external_blocked()`.
pub async fn fetch_osm_na_trails_corridor(
    http: &HttpClient,
    lat: f64,
    lon: f64,
) -> ActiveMobilityCorridorSummary {
    if http.is_offline_external_blocked() {
        return ActiveMobilityCorridorSummary::skipped("offline_skipped");
    }
    fetch_osm_na_trails_corridor_with_layer(http, OSM_NA_TRAILS_LAYER, lat, lon).await
}

pub async fn fetch_osm_na_trails_corridor_with_layer(
    http: &HttpClient,
    layer_url: &str,
    lat: f64,
    lon: f64,
) -> ActiveMobilityCorridorSummary {
    if !LatLon::new(lat, lon).is_valid() {
        return ActiveMobilityCorridorSummary::skipped("invalid_coordinates");
    }
    let params = vec![
        ("geometry", format!("{lon},{lat}")),
        ("geometryType", "esriGeometryPoint".to_string()),
        ("inSR", "4326".to_string()),
        ("spatialRel", "esriSpatialRelIntersects".to_string()),
        ("distance", "800".to_string()),
        ("units", "esriSRUnit_Meter".to_string()),
        (
            "outFields",
            "osm_id,name,highway,foot,footway,bicycle,bicycle_road,cycleway,sidewalk,surface,lit"
                .to_string(),
        ),
        ("returnGeometry", "false".to_string()),
        ("resultRecordCount", "80".to_string()),
    ];
    let Ok(j) = http.arcgis_query(layer_url, &params, 4_500).await else {
        return ActiveMobilityCorridorSummary::skipped("upstream_unavailable");
    };
    let Some(feats) = j.get("features").and_then(Value::as_array) else {
        return ActiveMobilityCorridorSummary::skipped("upstream_unavailable");
    };

    let mut foot_cnt = 0usize;
    let mut bike_cnt = 0usize;
    let mut lit_cnt = 0usize;
    let mut named_trails: Vec<String> = Vec::new();

    for f in feats {
        let Some(attrs) = f.get("attributes").and_then(Value::as_object) else {
            continue;
        };
        let s = |k: &str| {
            attrs
                .get(k)
                .and_then(Value::as_str)
                .map(|v| v.trim().to_ascii_lowercase())
                .unwrap_or_default()
        };
        let hwy = s("highway");
        let foot = s("foot");
        let footway = s("footway");
        let sidewalk = s("sidewalk");
        let bicycle = s("bicycle");
        let bike_rd = s("bicycle_road");
        let cycleway = s("cycleway");
        let lit = s("lit");

        let is_foot = matches!(
            hwy.as_str(),
            "footway" | "pedestrian" | "path" | "steps" | "living_street"
        ) || matches!(foot.as_str(), "yes" | "designated" | "permissive")
            || !footway.is_empty()
            || (!sidewalk.is_empty() && sidewalk != "no" && sidewalk != "none");

        let is_bike = hwy == "cycleway"
            || matches!(bicycle.as_str(), "designated" | "yes")
            || bike_rd == "yes"
            || (!cycleway.is_empty() && cycleway != "no" && cycleway != "none");

        if is_foot {
            foot_cnt += 1;
        }
        if is_bike {
            bike_cnt += 1;
        }
        if lit == "yes" {
            lit_cnt += 1;
        }
        if let Some(nm) = attrs
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty() && named_trails.len() < 5)
        {
            if !named_trails.iter().any(|x| x.eq_ignore_ascii_case(nm)) {
                named_trails.push(nm.to_string());
            }
        }
    }

    ActiveMobilityCorridorSummary {
        queried: true,
        status: "verified_osm_na_trails".to_string(),
        sidewalk_or_footway_segments_800m: foot_cnt,
        bikeway_or_cycleway_segments_800m: bike_cnt,
        lit_pathway_segments_800m: lit_cnt,
        has_sidewalk_corridor: foot_cnt > 0,
        has_bikeway_corridor: bike_cnt > 0,
        named_trails,
    }
}

/// Commute times for every home against every place, using the routing settings captured by
/// [`HttpClient::from_env`] (no router: labelled estimates).
pub async fn commute_times(
    http: &HttpClient,
    homes: &[LatLon],
    places: &[LatLon],
) -> CommuteMatrix {
    RoutingClient::from_http(http).commute_times(homes, places).await
}

/// Unique valid points (by 5-decimal key) and, per input point, its index among them.
fn unique_keys(points: &[LatLon]) -> (Vec<(i32, i32)>, Vec<Option<usize>>) {
    let mut keys: Vec<(i32, i32)> = Vec::new();
    let mut index: HashMap<(i32, i32), usize> = HashMap::new();
    let map = points
        .iter()
        .map(|p| {
            if !p.is_valid() {
                return None;
            }
            let k = p.key();
            Some(*index.entry(k).or_insert_with(|| {
                keys.push(k);
                keys.len() - 1
            }))
        })
        .collect();
    (keys, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn table_url_lists_sources_then_destinations_at_five_decimals() {
        let url = table_url(
            "http://osrm:5000",
            &[LatLon::new(37.33382, -121.88634).key()],
            &[LatLon::new(37.2538, -121.9285).key(), LatLon::new(37.28, -121.95).key()],
        );
        assert_eq!(
            url,
            "http://osrm:5000/table/v1/driving/-121.88634,37.33382;-121.92850,37.25380;\
             -121.95000,37.28000?sources=0&destinations=1;2&annotations=duration,distance\
             &generate_hints=false"
        );
    }

    #[test]
    fn parse_table_reads_nulls_snaps_and_version() {
        let j = json!({
            "code": "Ok",
            "data_version": "2026-11-11",
            "durations": [[60.0, null]],
            "distances": [[1609.344, null]],
            "sources": [{ "distance": 4.2 }],
            "destinations": [{ "distance": 1.0 }, { "distance": 9000.0 }]
        });
        let t = parse_table(&j, 1, 2).unwrap();
        assert_eq!(t.data_version.as_deref(), Some("2026-11-11"));
        assert_eq!(
            t.resolve(0, 0, DEFAULT_MAX_SNAP_M),
            Resolved::Routed {
                seconds: 60.0,
                meters: 1609.344,
                provider: RoutingProviderKind::Osrm,
            }
        );
        assert_eq!(
            t.resolve(0, 1, DEFAULT_MAX_SNAP_M),
            Resolved::Estimate(EstimateReason::OutsideCoverage, FallbackReason::OutsideCoverage)
        );
        assert!(parse_table(&json!({"code": "TooBig", "message": "Too many"}), 1, 2).is_err());
        assert!(parse_table(&json!({"code": "Ok", "durations": [[1.0]]}), 1, 2).is_err());
    }

    #[test]
    fn snapping_far_from_any_road_counts_as_outside_coverage() {
        let j = json!({
            "code": "Ok",
            "durations": [[600.0]],
            "distances": [[9000.0]],
            "sources": [{ "distance": 85_000.0 }],
            "destinations": [{ "distance": 3.0 }]
        });
        let t = parse_table(&j, 1, 1).unwrap();
        assert_eq!(
            t.resolve(0, 0, DEFAULT_MAX_SNAP_M),
            Resolved::Estimate(EstimateReason::OutsideCoverage, FallbackReason::OutsideCoverage)
        );
    }

    #[test]
    fn failure_cooldown_env_and_region_center() {
        let cfg = RoutingConfig::from_env(&EnvConfig::from_map(HashMap::from([(
            ENV_OSRM_FAILURE_COOLDOWN_SECS.to_string(),
            "25".to_string(),
        )])));
        assert_eq!(cfg.failure_cooldown_secs, 25);

        let r = OsrmRegion {
            name: "ca".to_string(),
            bbox: [-124.0, 32.0, -114.0, 42.0],
            base_url: "http://127.0.0.1:5001".to_string(),
        };
        let c = r.center();
        assert!((c.lat - 37.0).abs() < 1e-9);
        assert!((c.lon - (-119.0)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn cooldown_fast_fails_on_5xx_and_clears_on_probe() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicU8;

        #[derive(Clone)]
        struct MockState {
            // 0 = 200 Ok, 1 = 503 Unavailable, 2 = 400 Bad Request
            mode: Arc<AtomicU8>,
            table_hits: Arc<AtomicUsize>,
            probe_hits: Arc<AtomicUsize>,
        }

        let st = MockState {
            mode: Arc::new(AtomicU8::new(1)),
            table_hits: Arc::new(AtomicUsize::new(0)),
            probe_hits: Arc::new(AtomicUsize::new(0)),
        };

        let app = Router::new()
            .route(
                "/table/v1/driving/:coords",
                get(|State(s): State<MockState>| async move {
                    s.table_hits.fetch_add(1, Ordering::SeqCst);
                    match s.mode.load(Ordering::SeqCst) {
                        0 => (
                            StatusCode::OK,
                            Json(json!({
                                "code": "Ok",
                                "data_version": "mock-v1",
                                "durations": [[120.0]],
                                "distances": [[2400.0]],
                                "sources": [{"distance": 3.0}],
                                "destinations": [{"distance": 4.0}]
                            })),
                        ),
                        2 => (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"code": "TooBig", "message": "too big"})),
                        ),
                        _ => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"code": "Error", "message": "down"})),
                        ),
                    }
                }),
            )
            .route(
                "/nearest/v1/driving/:coord",
                get(|State(s): State<MockState>| async move {
                    s.probe_hits.fetch_add(1, Ordering::SeqCst);
                    if s.mode.load(Ordering::SeqCst) == 0 {
                        (StatusCode::OK, Json(json!({"code": "Ok", "waypoints": []})))
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"code": "Error"})))
                    }
                }),
            )
            .with_state(st.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base = format!("http://{addr}");
        let regions_json = serde_json::to_string(&json!([
            {
                "name": "mock-ca",
                "bbox": [-124.5, 32.5, -114.1, 42.0],
                "baseUrl": base,
            }
        ]))
        .unwrap();
        let env = EnvConfig::from_map(HashMap::from([
            (ENV_OSRM_REGIONS_JSON.to_string(), regions_json),
            (ENV_OSRM_TIMEOUT_MS.to_string(), "3000".to_string()),
            (ENV_OSRM_FAILURE_COOLDOWN_SECS.to_string(), "30".to_string()),
        ]));
        let http = HttpClient::from_env(&env, None);
        let client = RoutingClient::from_http(&http);
        let src = [LatLon::new(37.2567, -121.8968)];
        let dst = [LatLon::new(37.3380, -121.8853)];

        // 1. First request hits OSRM 503 -> falls back to RouterDown and activates cooldown.
        let m1 = client.drive_matrix(&src, &dst).await;
        assert_eq!(m1.requests, 1);
        assert_eq!(st.table_hits.load(Ordering::SeqCst), 1);
        assert_eq!(m1.cells[0][0].unwrap().reason, Some(EstimateReason::RouterDown));
        assert!(is_router_in_cooldown(&base));

        // 2. Second request fast-fails without hitting the HTTP server!
        let m2 = client.drive_matrix(&src, &dst).await;
        assert_eq!(m2.requests, 0);
        assert_eq!(st.table_hits.load(Ordering::SeqCst), 1);
        assert_eq!(m2.cells[0][0].unwrap().reason, Some(EstimateReason::RouterDown));

        // 3. Probe while still down keeps cooldown active.
        let probes_down = client.probe_regions().await;
        assert_eq!(probes_down.len(), 1);
        assert!(!probes_down[0].healthy);
        assert_eq!(probes_down[0].status_code, Some(503));
        assert!(is_router_in_cooldown(&base));

        // 4. Flip mock OSRM to healthy -> probe clears cooldown immediately -> live route succeeds!
        st.mode.store(0, Ordering::SeqCst);
        let probes_up = client.probe_regions().await;
        assert_eq!(probes_up.len(), 1);
        assert!(probes_up[0].healthy);
        assert_eq!(probes_up[0].status_code, Some(200));
        assert!(!is_router_in_cooldown(&base));

        let m3 = client.drive_matrix(&src, &dst).await;
        assert_eq!(m3.requests, 1);
        assert_eq!(st.table_hits.load(Ordering::SeqCst), 2);
        assert_eq!(m3.routed, 1);
        assert!(!m3.cells[0][0].unwrap().estimated);

        // 5. HTTP 400 (query error, not outage) does NOT set cooldown on a distinct base URL.
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let st2 = MockState {
            mode: Arc::new(AtomicU8::new(2)),
            table_hits: Arc::new(AtomicUsize::new(0)),
            probe_hits: Arc::new(AtomicUsize::new(0)),
        };
        let app2 = Router::new()
            .route(
                "/table/v1/driving/:coords",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"code": "TooBig", "message": "too big"})),
                    )
                }),
            )
            .with_state(st2);
        tokio::spawn(async move {
            let _ = axum::serve(listener2, app2).await;
        });
        let base2 = format!("http://{addr2}");
        let env2 = EnvConfig::from_map(HashMap::from([
            (ENV_OSRM_BASE_URL.to_string(), base2.clone()),
            (ENV_OSRM_FAILURE_COOLDOWN_SECS.to_string(), "30".to_string()),
        ]));
        let c2 = RoutingClient::from_http(&HttpClient::from_env(&env2, None));
        let m400 = c2.drive_matrix(&src, &dst).await;
        assert_eq!(m400.cells[0][0].unwrap().reason, Some(EstimateReason::RouterDown));
        assert!(
            !is_router_in_cooldown(&base2),
            "HTTP 400 query errors must not trigger router outage cooldown"
        );
    }

    #[tokio::test]
    async fn unconfigured_osrm_without_cloud_extensions_emits_router_unset() {
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let closed_addr = unused.local_addr().expect("addr");
        drop(unused);

        // Even if commercial env vars are present, Personal Edition has no BYOK cloud routing
        // extensions registered on default ExtensionHub, so it cleanly emits RouterUnset.
        let env = EnvConfig::from_map(std::collections::HashMap::from([
            (
                crate::modules::routing_providers::ENV_FREEROUTE_ALLOW_KEYLESS.to_string(),
                "1".to_string(),
            ),
            (
                crate::modules::routing_providers::ENV_FREEROUTE_BASE_URL.to_string(),
                format!("http://{closed_addr}"),
            ),
            (
                crate::modules::routing_providers::ENV_ROUTING_EXTERNAL_TIMEOUT_MS.to_string(),
                "500".to_string(),
            ),
        ]));
        let client = RoutingClient::from_http(&HttpClient::from_env(&env, None));
        let src = [LatLon::new(37.2839, -121.9317)];
        let dst = [LatLon::new(37.3382, -121.8863)];
        let matrix = client.drive_matrix(&src, &dst).await;
        let cell = matrix.cells[0][0].expect("estimate cell");
        assert_eq!(cell.reason, Some(EstimateReason::RouterUnset));
        assert_eq!(
            cell.fallback_reason,
            Some(FallbackReason::NoOsrmRegionConfigured)
        );
        assert!(
            !matrix
                .warnings
                .iter()
                .any(|w| w.contains("OSRM router unavailable")),
            "must not misreport OSRM unavailable when no OSRM region is configured: {:?}",
            matrix.warnings
        );
    }

    #[tokio::test]
    async fn drive_time_matrix_persists_and_hydrates_from_shared_data_records() {
        let db = DbStore::connect_url(":memory:", Some("sqlite"))
            .await
            .expect("sqlite memory db");
        crate::extensions::shared_cache::seed_builtin_shared_datasets(&db)
            .await
            .expect("seed shared datasets");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let closed_addr = listener.local_addr().expect("addr");
        drop(listener);

        let origin_a = LatLon::new(37.2872, -121.9500);
        let origin_b = LatLon::new(37.2874, -121.9498);
        let gh7_a = encode_geohash(origin_a.lat, origin_a.lon, 7).expect("gh7 a");
        let gh7_b = encode_geohash(origin_b.lat, origin_b.lon, 7).expect("gh7 b");
        assert_eq!(gh7_a, gh7_b, "two nearby origins share geohash7 cell");

        let dst = vec![
            LatLon::new(37.3382, -121.8863),
            LatLon::new(37.4220, -122.0841),
        ];
        let (dst_keys, _) = unique_keys(&dst);
        let dest_hash = drive_matrix_dest_hash(&dst_keys);
        let src_key = ((origin_a.lat * 1e5).round() as i32, (origin_a.lon * 1e5).round() as i32);

        let row = vec![
            Some(Resolved::Routed {
                seconds: 642.0,
                meters: 10_420.0,
                provider: RoutingProviderKind::Osrm,
            }),
            Some(Resolved::Routed {
                seconds: 1_510.0,
                meters: 31_800.0,
                provider: RoutingProviderKind::Osrm,
            }),
        ];
        persist_drive_matrix_to_shared_store(&db, &gh7_a, src_key, &dest_hash, &row, &dst_keys)
            .await;

        let (_stored, proto) = get_shared_dataset_proto::<DriveTimeMatrixProto>(
            None,
            &db,
            dataset_ids::DRIVE_TIME_MATRIX,
            ShareScope::Geohash7,
            &gh7_b,
            &dest_hash,
        )
        .await
        .expect("read proto")
        .expect("DRIVE_TIME_MATRIX persisted");
        assert_eq!(proto.origin_geohash7, gh7_b);
        assert_eq!(proto.entries.len(), 2);

        let mut env_map = HashMap::new();
        env_map.insert(ENV_OSRM_BASE_URL.to_string(), format!("http://{closed_addr}"));
        let env = EnvConfig::from_map(env_map);
        let http = HttpClient::from_env(&env, None);
        let hub = ExtensionHub::new_default();
        let client = RoutingClient::from_http(&http)
            .with_extension_hub(hub)
            .with_db(db);

        let matrix = client.drive_matrix(&[origin_b], &dst).await;
        assert_eq!(matrix.routed, 2);
        assert_eq!(matrix.estimated, 0);
        assert_eq!(matrix.requests, 0, "DB hit must avoid upstream HTTP calls");
        let first = matrix.cells[0][0].expect("first cell");
        assert!(!first.estimated);
        assert!((first.distance_mi - (10_420.0 / MILE_M)).abs() < 1e-6);
        assert!((first.free_flow_min - 642.0 / 60.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn foot_and_bike_routers_reject_public_demo_hosts_and_route_or_estimate_honestly() {
        use axum::{routing::any, Json, Router};

        for demo in [
            "https://routing.openstreetmap.de/routed-foot",
            "https://router.project-osrm.org",
            "http://foot.routing.openstreetmap.de",
        ] {
            let env = EnvConfig::from_map(HashMap::from([
                (ENV_OSRM_FOOT_BASE_URL.to_string(), demo.to_string()),
                (ENV_OSRM_BIKE_BASE_URL.to_string(), demo.to_string()),
            ]));
            let cfg = RoutingConfig::from_env(&env);
            assert!(cfg.foot_base_url.is_none(), "{demo} must be banned");
            assert!(cfg.bike_base_url.is_none(), "{demo} must be banned");
            assert_eq!(cfg.issues.len(), 2);
        }

        let app = Router::new()
            .route(
                "/table/v1/foot/:coords",
                any(|| async {
                    Json(json!({
                        "code": "Ok",
                        "durations": [[480.0]],
                        "distances": [[640.0]],
                        "sources": [{"distance": 2.0}],
                        "destinations": [{"distance": 3.0}]
                    }))
                }),
            )
            .route(
                "/trails/query",
                any(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "osm_id": 1001,
                                    "name": "Los Gatos Creek Trail",
                                    "highway": "cycleway",
                                    "foot": "designated",
                                    "bicycle": "designated",
                                    "lit": "yes"
                                }
                            }
                        ]
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");

        let env = EnvConfig::from_map(HashMap::from([(
            ENV_OSRM_FOOT_BASE_URL.to_string(),
            base.clone(),
        )]));
        let http = HttpClient::offline_loopback_only(None);
        let client = RoutingClient::new(http.clone(), RoutingConfig::from_env(&env));
        let orig = LatLon::new(37.2883, -121.9441);
        let dst = [LatLon::new(37.2850, -121.9480)];

        let foot = client
            .active_mobility_row(ActiveMobilityMode::Foot, orig, &dst, None, false)
            .await;
        let f0 = foot[0].expect("foot cell");
        assert!(!f0.estimated);
        assert_eq!(f0.basis, "self_hosted_osrm");
        assert!((f0.duration_min - 8.0).abs() < 1e-6);

        // Bike router is unset -> falls back honestly to estimated: true
        let bike = client
            .active_mobility_row(ActiveMobilityMode::Bike, orig, &dst, None, true)
            .await;
        let b0 = bike[0].expect("bike cell");
        assert!(b0.estimated);
        assert_eq!(b0.reason, Some(EstimateReason::RouterUnset));

        let corridor =
            fetch_osm_na_trails_corridor_with_layer(&http, &format!("{base}/trails"), orig.lat, orig.lon)
                .await;
        assert!(corridor.queried);
        assert!(corridor.has_sidewalk_corridor);
        assert!(corridor.has_bikeway_corridor);
        assert_eq!(corridor.named_trails, vec!["Los Gatos Creek Trail".to_string()]);
    }
}
