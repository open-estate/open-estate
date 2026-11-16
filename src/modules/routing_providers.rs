//! Multi-provider driving matrix adapters (`osrm_routing` Tier-1 primary + 4 Tier-2 cloud
//! fallback services: `mapbox_routing`, `locationiq_routing`, `freeroute_routing`,
//! `openrouteservice_routing`), strict credential/URL redaction, per-service rate limiting,
//! cooldown tracking, and non-secret `/api/ops/telemetry` counters.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU32;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use governor::{DefaultKeyedRateLimiter, Quota};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::extensions::proto::{
    extension_envelope_proto::Payload, CapabilityDomain, ExtensionEnvelopeProto,
    ExtensionManifestProto, ExtensionQueryProto, ProvenanceTier, RoutingMatrixProto,
    SourceAttributionProto,
};
use crate::extensions::ExtensionHub;
use crate::modules::commute::{
    active_router_cooldown, clear_router_cooldown, is_public_demo_host, is_router_in_cooldown,
    record_router_cooldown, LatLon, RoutingConfig, OSM_COPYRIGHT_URL, OSRM_ATTRIBUTION, PROFILE,
};
use crate::shared::{now_iso, sanitize_user_warning, url_host_label, EnvConfig, HttpClient, SourceItem};

// ------------------------------------------------------------------------------------------
// Environment variable names & defaults
// ------------------------------------------------------------------------------------------

pub const ENV_MAPBOX_ACCESS_TOKEN: &str = "MAPBOX_ACCESS_TOKEN";
pub const ENV_MAPBOX_API_KEY: &str = "MAPBOX_API_KEY";
pub const ENV_MAPBOX_BASE_URL: &str = "MAPBOX_BASE_URL";

pub const ENV_LOCATIONIQ_API_KEY: &str = "LOCATIONIQ_API_KEY";
pub const ENV_LOCATIONIQ_ACCESS_TOKEN: &str = "LOCATIONIQ_ACCESS_TOKEN";
pub const ENV_LOCATIONIQ_BASE_URL: &str = "LOCATIONIQ_BASE_URL";

pub const ENV_FREEROUTE_API_KEY: &str = "FREEROUTE_API_KEY";
pub const ENV_FREEROUTE_BASE_URL: &str = "FREEROUTE_BASE_URL";
pub const ENV_FREEROUTE_ALLOW_KEYLESS: &str = "FREEROUTE_ALLOW_KEYLESS";

pub const ENV_OPENROUTESERVICE_API_KEY: &str = "OPENROUTESERVICE_API_KEY";
pub const ENV_ORS_API_KEY: &str = "ORS_API_KEY";
pub const ENV_OPENROUTESERVICE_BASE_URL: &str = "OPENROUTESERVICE_BASE_URL";

pub const ENV_ROUTING_FALLBACK_STRATEGY: &str = "ROUTING_FALLBACK_STRATEGY";
pub const ENV_ROUTING_FALLBACK_ORDER: &str = "ROUTING_FALLBACK_ORDER";
pub const ENV_ROUTING_EXTERNAL_TIMEOUT_MS: &str = "ROUTING_EXTERNAL_TIMEOUT_MS";
pub const ENV_ROUTING_EXTERNAL_COOLDOWN_SECS: &str = "ROUTING_EXTERNAL_COOLDOWN_SECS";

pub const DEFAULT_MAPBOX_BASE_URL: &str = "https://api.mapbox.com";
pub const DEFAULT_LOCATIONIQ_BASE_URL: &str = "https://us1.locationiq.com";
pub const DEFAULT_FREEROUTE_BASE_URL: &str = "https://api.maps.freeroute.org";
pub const DEFAULT_OPENROUTESERVICE_BASE_URL: &str = "https://api.openrouteservice.org";

/// Maximum total coordinates (`sources.len() + destinations.len()`) per cloud matrix request.
pub const MAX_CLOUD_MATRIX_LOCATIONS: usize = 25;
pub const DEFAULT_EXTERNAL_TIMEOUT_MS: u64 = 4_500;
pub const DEFAULT_ORS_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_EXTERNAL_COOLDOWN_SECS: u64 = 25;
pub const AUTH_OR_RATE_LIMIT_COOLDOWN_SECS: u64 = 120;

/// Mapbox `mapbox/driving` incorporates typical traffic speeds, so default multiplier is `1.0`.
pub const MAPBOX_TRAFFIC_BUFFER: f64 = 1.0;
/// Raw OSM cloud matrix engines (`locationiq_routing`, `freeroute_routing`, `openrouteservice_routing`)
/// default to `1.15` unless `OSRM_TRAFFIC_BUFFER` / `with_traffic_buffer` overrides it.
pub const CLOUD_OSM_TRAFFIC_BUFFER: f64 = 1.15;

const MIN_EXTERNAL_TIMEOUT_MS: u64 = 400;
const MAX_EXTERNAL_TIMEOUT_MS: u64 = 30_000;
const MAX_EXTERNAL_COOLDOWN_SECS: u64 = 600;

// ------------------------------------------------------------------------------------------
// Provider enum, strategy, and configuration
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingProviderKind {
    Osrm,
    Mapbox,
    LocationIq,
    FreeRoute,
    OpenRouteService,
}

impl RoutingProviderKind {
    pub const ALL: [Self; 5] = [
        Self::Osrm,
        Self::Mapbox,
        Self::LocationIq,
        Self::FreeRoute,
        Self::OpenRouteService,
    ];

    pub const CLOUD_PROVIDERS: [Self; 4] = [
        Self::Mapbox,
        Self::LocationIq,
        Self::FreeRoute,
        Self::OpenRouteService,
    ];

    pub fn from_id_or_alias(raw: &str) -> Option<Self> {
        let norm = raw.trim().to_ascii_lowercase().replace('-', "_");
        match norm.as_str() {
            "osrm_routing" | "osrm" => Some(Self::Osrm),
            "mapbox_routing" | "mapbox" => Some(Self::Mapbox),
            "locationiq_routing" | "locationiq" | "location_iq" => Some(Self::LocationIq),
            "freeroute_routing" | "freeroute" | "free_route" => Some(Self::FreeRoute),
            "openrouteservice_routing" | "openrouteservice" | "ors" => Some(Self::OpenRouteService),
            _ => None,
        }
    }

    pub fn extension_id(self) -> &'static str {
        match self {
            Self::Osrm => "osrm_routing",
            Self::Mapbox => "mapbox_routing",
            Self::LocationIq => "locationiq_routing",
            Self::FreeRoute => "freeroute_routing",
            Self::OpenRouteService => "openrouteservice_routing",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Osrm => "Self-Hosted OSRM Routing Matrix",
            Self::Mapbox => "Mapbox Directions Matrix API",
            Self::LocationIq => "LocationIQ Matrix API",
            Self::FreeRoute => "FreeRoute Matrix API",
            Self::OpenRouteService => "OpenRouteService Matrix API",
        }
    }

    pub fn tier(self) -> &'static str {
        match self {
            Self::Osrm => "primary",
            Self::Mapbox | Self::LocationIq | Self::FreeRoute | Self::OpenRouteService => {
                "fallback"
            }
        }
    }

    pub fn profile_name(self) -> &'static str {
        match self {
            Self::Osrm | Self::LocationIq => "driving",
            Self::Mapbox => "mapbox/driving",
            Self::FreeRoute | Self::OpenRouteService => "driving-car",
        }
    }

    pub fn default_priority(self) -> i32 {
        match self {
            Self::Osrm => 200,
            Self::Mapbox => 120,
            Self::LocationIq => 115,
            Self::FreeRoute => 110,
            Self::OpenRouteService => 105,
        }
    }

    pub fn effective_traffic_buffer(self, configured_buffer: f64, overridden: bool) -> f64 {
        if overridden {
            return configured_buffer;
        }
        match self {
            Self::Osrm => configured_buffer,
            Self::Mapbox => MAPBOX_TRAFFIC_BUFFER,
            Self::LocationIq | Self::FreeRoute | Self::OpenRouteService => CLOUD_OSM_TRAFFIC_BUFFER,
        }
    }

    /// Honest report citation (`SourceItem`) — never triggers `has_competitor_brand` and avoids
    /// words scrubbed by `scrub_internal_tech_words`.
    pub fn report_source_item(self) -> SourceItem {
        match self {
            Self::Osrm => SourceItem::new(OSRM_ATTRIBUTION, OSM_COPYRIGHT_URL),
            Self::Mapbox => SourceItem::new(
                "Mapbox Directions Matrix · © OpenStreetMap",
                "https://docs.mapbox.com/api/navigation/matrix/",
            ),
            Self::LocationIq => SourceItem::new(
                "LocationIQ Matrix · © OpenStreetMap",
                "https://docs.locationiq.com/reference/matrix",
            ),
            Self::FreeRoute => SourceItem::new(
                "FreeRoute Matrix API · © OpenStreetMap",
                "https://maps.freeroute.org",
            ),
            Self::OpenRouteService => SourceItem::new(
                "openrouteservice.org by HeiGIT · © OpenStreetMap",
                "https://giscience.github.io/openrouteservice/api-reference/endpoints/matrix/",
            ),
        }
    }

    pub fn extension_source_attribution(self) -> (&'static str, &'static str) {
        match self {
            Self::Osrm => ("OpenStreetMap road network (OSRM)", "https://project-osrm.org/"),
            Self::Mapbox => (
                "Mapbox Directions Matrix · © OpenStreetMap",
                "https://docs.mapbox.com/api/navigation/matrix/",
            ),
            Self::LocationIq => (
                "LocationIQ Matrix · © OpenStreetMap",
                "https://docs.locationiq.com/reference/matrix",
            ),
            Self::FreeRoute => ("FreeRoute Matrix API · © OpenStreetMap", "https://maps.freeroute.org"),
            Self::OpenRouteService => (
                "openrouteservice.org by HeiGIT · © OpenStreetMap",
                "https://giscience.github.io/openrouteservice/api-reference/endpoints/matrix/",
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingFallbackStrategy {
    #[default]
    RoundRobin,
    Priority,
}

impl RoutingFallbackStrategy {
    pub fn from_raw(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("priority" | "ordered" | "failover") => Self::Priority,
            _ => Self::RoundRobin,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoundRobin => "round_robin",
            Self::Priority => "priority",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CloudProviderEndpoint {
    pub kind: RoutingProviderKind,
    pub api_key: String,
    pub base_url: String,
    pub timeout_ms: u64,
}

impl CloudProviderEndpoint {
    pub fn cooldown_key(&self) -> String {
        cloud_cooldown_key(self.kind, &self.base_url)
    }

    pub fn host_only(&self) -> String {
        Url::parse(&self.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| self.kind.extension_id().to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CloudFallbackPoolConfig {
    pub strategy: RoutingFallbackStrategy,
    pub fallback_order: Vec<RoutingProviderKind>,
    pub external_timeout_ms: u64,
    pub external_timeout_overridden: bool,
    pub external_cooldown_secs: u64,
    pub providers: Vec<CloudProviderEndpoint>,
}

impl Default for CloudFallbackPoolConfig {
    fn default() -> Self {
        Self {
            strategy: RoutingFallbackStrategy::RoundRobin,
            fallback_order: Vec::new(),
            external_timeout_ms: DEFAULT_EXTERNAL_TIMEOUT_MS,
            external_timeout_overridden: false,
            external_cooldown_secs: DEFAULT_EXTERNAL_COOLDOWN_SECS,
            providers: Vec::new(),
        }
    }
}

impl CloudFallbackPoolConfig {
    pub fn from_env(env: &EnvConfig, issues: &mut Vec<String>) -> Self {
        let mut cfg = Self {
            strategy: RoutingFallbackStrategy::from_raw(env.routing_fallback_strategy()),
            ..Self::default()
        };

        if let Some(order_raw) = env.routing_fallback_order() {
            for tok in order_raw
                .split([',', ';', ' '])
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if let Some(kind) = RoutingProviderKind::from_id_or_alias(tok) {
                    if kind != RoutingProviderKind::Osrm && !cfg.fallback_order.contains(&kind) {
                        cfg.fallback_order.push(kind);
                    }
                }
            }
        }

        if let Some(raw_to) = env.get(ENV_ROUTING_EXTERNAL_TIMEOUT_MS) {
            match raw_to.trim().parse::<u64>() {
                Ok(v) => {
                    cfg.external_timeout_ms =
                        v.clamp(MIN_EXTERNAL_TIMEOUT_MS, MAX_EXTERNAL_TIMEOUT_MS);
                    cfg.external_timeout_overridden = true;
                }
                Err(_) => issues.push(format!(
                    "{ENV_ROUTING_EXTERNAL_TIMEOUT_MS} is not an integer; using {DEFAULT_EXTERNAL_TIMEOUT_MS}"
                )),
            }
        }

        if let Some(raw_cd) = env.get(ENV_ROUTING_EXTERNAL_COOLDOWN_SECS) {
            match raw_cd.trim().parse::<u64>() {
                Ok(v) => cfg.external_cooldown_secs = v.min(MAX_EXTERNAL_COOLDOWN_SECS),
                Err(_) => issues.push(format!(
                    "{ENV_ROUTING_EXTERNAL_COOLDOWN_SECS} is not an integer; using {DEFAULT_EXTERNAL_COOLDOWN_SECS}"
                )),
            }
        }

        let specs: [
            (
                RoutingProviderKind,
                Option<&str>,
                Option<&str>,
                &str,
                &str,
                u64,
            );
            4
        ] = [
            (
                RoutingProviderKind::Mapbox,
                env.mapbox_access_token(),
                env.mapbox_base_url(),
                DEFAULT_MAPBOX_BASE_URL,
                ENV_MAPBOX_BASE_URL,
                DEFAULT_EXTERNAL_TIMEOUT_MS,
            ),
            (
                RoutingProviderKind::LocationIq,
                env.locationiq_api_key(),
                env.locationiq_base_url(),
                DEFAULT_LOCATIONIQ_BASE_URL,
                ENV_LOCATIONIQ_BASE_URL,
                DEFAULT_EXTERNAL_TIMEOUT_MS,
            ),
            (
                RoutingProviderKind::FreeRoute,
                env.freeroute_api_key(),
                env.freeroute_base_url(),
                DEFAULT_FREEROUTE_BASE_URL,
                ENV_FREEROUTE_BASE_URL,
                DEFAULT_EXTERNAL_TIMEOUT_MS,
            ),
            (
                RoutingProviderKind::OpenRouteService,
                env.openrouteservice_api_key(),
                env.openrouteservice_base_url(),
                DEFAULT_OPENROUTESERVICE_BASE_URL,
                ENV_OPENROUTESERVICE_BASE_URL,
                DEFAULT_ORS_TIMEOUT_MS,
            ),
        ];

        for (kind, key_opt, raw_base_opt, default_base, base_env_name, provider_default_to) in specs
        {
            let secret_opt = key_opt
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let secret = match secret_opt {
                Some(s) => s,
                None if kind == RoutingProviderKind::FreeRoute
                    && env_allows_freeroute_keyless(env) =>
                {
                    String::new()
                }
                None => continue,
            };
            let base_candidate = raw_base_opt.unwrap_or(default_base);
            match normalize_cloud_base_url(base_candidate, base_env_name) {
                Ok(base_url) => {
                    let timeout_ms = if cfg.external_timeout_overridden {
                        cfg.external_timeout_ms
                    } else {
                        provider_default_to
                    };
                    cfg.providers.push(CloudProviderEndpoint {
                        kind,
                        api_key: secret,
                        base_url,
                        timeout_ms,
                    });
                }
                Err(issue) => issues.push(issue),
            }
        }

        cfg
    }

    pub fn get_provider(&self, kind: RoutingProviderKind) -> Option<&CloudProviderEndpoint> {
        self.providers.iter().find(|p| p.kind == kind)
    }

    pub fn has_any_configured(&self) -> bool {
        !self.providers.is_empty()
    }

    /// Signature of configured Tier-2 endpoints used to scope atomic round-robin counters so
    /// production `RoutingClient::from_http` calls share round-robin across requests while
    /// concurrent tests with distinct `127.0.0.1:<port>` mock servers never race each other.
    pub fn pool_signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        for p in &self.providers {
            p.kind.hash(&mut hasher);
            p.base_url.hash(&mut hasher);
        }
        hasher.finish()
    }
}

pub fn normalize_cloud_base_url(raw: &str, env_name: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = Url::parse(trimmed)
        .map_err(|e| format!("{env_name} is not a valid URL ({e}); fallback routing entry disabled"))?;
    if !matches!(url.scheme(), "http" | "https") || url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "{env_name} must be an http(s) base URL without query or fragment; fallback routing entry disabled"
        ));
    }
    let host = url.host_str().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("{env_name} has no host; fallback routing entry disabled"));
    }
    if is_public_demo_host(host) {
        return Err(format!(
            "{env_name} points at the public OSRM demo server; fallback routing entry disabled"
        ));
    }
    Ok(trimmed.to_string())
}

pub fn cloud_cooldown_key(kind: RoutingProviderKind, base_url: &str) -> String {
    format!("{}|{}", kind.extension_id(), base_url.trim().trim_end_matches('/'))
}

pub fn is_cloud_provider_in_cooldown(kind: RoutingProviderKind, base_url: &str) -> bool {
    is_router_in_cooldown(&cloud_cooldown_key(kind, base_url))
}

pub fn clear_cloud_provider_cooldown(kind: RoutingProviderKind, base_url: &str) {
    clear_router_cooldown(&cloud_cooldown_key(kind, base_url));
}

// ------------------------------------------------------------------------------------------
// Round-robin counters & LocationIQ 2 RPS rate limiter
// ------------------------------------------------------------------------------------------

static POOL_RR_COUNTERS: Lazy<DashMap<u64, Arc<AtomicUsize>>> = Lazy::new(DashMap::new);

pub fn round_robin_counter_for_signature(sig: u64) -> Arc<AtomicUsize> {
    if let Some(existing) = POOL_RR_COUNTERS.get(&sig) {
        return Arc::clone(existing.value());
    }
    if POOL_RR_COUNTERS.len() > 1024 {
        POOL_RR_COUNTERS.clear();
    }
    POOL_RR_COUNTERS
        .entry(sig)
        .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
        .clone()
}

/// Enforces LocationIQ's 2 requests/sec free-tier ceiling per base URL with burst=1 (`>=600ms`
/// spacing) so sequential multi-chunk matrix calls stay strictly under 2 requests in any 1.0s
/// sliding window and never self-429.
static LOCATIONIQ_LIMITER: Lazy<DefaultKeyedRateLimiter<String>> = Lazy::new(|| {
    let quota = Quota::with_period(Duration::from_millis(600))
        .expect("valid period")
        .allow_burst(NonZeroU32::new(1).expect("nonzero"));
    DefaultKeyedRateLimiter::keyed(quota)
});

// ------------------------------------------------------------------------------------------
// Process-wide non-secret atomic telemetry counters for `/api/ops/telemetry`
// ------------------------------------------------------------------------------------------

struct ProviderMetricsSlot {
    requests_succeeded: AtomicU64,
    requests_failed: AtomicU64,
    failovers: AtomicU64,
    last_error_code: RwLock<Option<String>>,
}

impl ProviderMetricsSlot {
    fn new() -> Self {
        Self {
            requests_succeeded: AtomicU64::new(0),
            requests_failed: AtomicU64::new(0),
            failovers: AtomicU64::new(0),
            last_error_code: RwLock::new(None),
        }
    }
}

static ROUTING_METRICS: Lazy<[ProviderMetricsSlot; 5]> = Lazy::new(|| {
    [
        ProviderMetricsSlot::new(),
        ProviderMetricsSlot::new(),
        ProviderMetricsSlot::new(),
        ProviderMetricsSlot::new(),
        ProviderMetricsSlot::new(),
    ]
});

fn metrics_slot(kind: RoutingProviderKind) -> &'static ProviderMetricsSlot {
    match kind {
        RoutingProviderKind::Osrm => &ROUTING_METRICS[0],
        RoutingProviderKind::Mapbox => &ROUTING_METRICS[1],
        RoutingProviderKind::LocationIq => &ROUTING_METRICS[2],
        RoutingProviderKind::FreeRoute => &ROUTING_METRICS[3],
        RoutingProviderKind::OpenRouteService => &ROUTING_METRICS[4],
    }
}

pub fn record_routing_success(kind: RoutingProviderKind) {
    metrics_slot(kind)
        .requests_succeeded
        .fetch_add(1, Ordering::Relaxed);
}

pub fn record_routing_failure(kind: RoutingProviderKind, code_label: &str) {
    let slot = metrics_slot(kind);
    slot.requests_failed.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut guard) = slot.last_error_code.write() {
        *guard = Some(code_label.to_string());
    }
}

pub fn record_routing_failover(kind: RoutingProviderKind) {
    metrics_slot(kind).failovers.fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingExtensionTelemetryItem {
    pub extension_id: String,
    pub name: String,
    pub tier: String,
    pub enabled: bool,
    pub configured: bool,
    pub in_cooldown: bool,
    pub requests_succeeded: u64,
    pub requests_failed: u64,
    pub failovers: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
}

fn env_allows_freeroute_keyless(env: &EnvConfig) -> bool {
    env.get(ENV_FREEROUTE_ALLOW_KEYLESS)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Checks whether a routing extension's parent system module is enabled (`neighborhood_livability`
/// plus optional `commute_transit` if registered).
pub(crate) fn is_routing_module_enabled(hub: &ExtensionHub) -> bool {
    hub.is_module_enabled("neighborhood_livability") && hub.is_module_enabled("commute_transit")
}

/// Checks whether a routing extension (`osrm_routing` or any of the 4 Tier-2 cloud extensions)
/// has valid credentials/endpoints configured in `env`, checking all supported alias env vars
/// without ever returning or logging secret values.
pub fn is_routing_extension_configured(
    extension_id: &str,
    env: Option<&EnvConfig>,
) -> Option<bool> {
    let kind = RoutingProviderKind::from_id_or_alias(extension_id)?;
    let owned_default;
    let cfg_env = match env {
        Some(e) => e,
        None => {
            owned_default = EnvConfig::from_env();
            &owned_default
        }
    };
    let configured = match kind {
        RoutingProviderKind::Osrm => RoutingConfig::from_env(cfg_env).is_enabled(),
        RoutingProviderKind::Mapbox => cfg_env.mapbox_access_token().is_some(),
        RoutingProviderKind::LocationIq => cfg_env.locationiq_api_key().is_some(),
        RoutingProviderKind::FreeRoute => {
            cfg_env.freeroute_api_key().is_some() || env_allows_freeroute_keyless(cfg_env)
        }
        RoutingProviderKind::OpenRouteService => cfg_env.openrouteservice_api_key().is_some(),
    };
    Some(configured)
}

/// Builds the non-secret telemetry array & summary for all 5 routing extensions for `/api/ops/telemetry`.
pub fn build_routing_fallback_telemetry(
    env: &EnvConfig,
    hub: &ExtensionHub,
) -> (Value, Vec<RoutingExtensionTelemetryItem>) {
    let routing_cfg = RoutingConfig::from_env(env);
    let mod_enabled = is_routing_module_enabled(hub);
    let mut items = Vec::with_capacity(RoutingProviderKind::ALL.len());
    let mut tier2_ready = 0_usize;
    let mut tier2_configured = 0_usize;
    let mut total_failovers = 0_u64;

    for kind in RoutingProviderKind::ALL {
        let ext_id = kind.extension_id();
        let enabled = mod_enabled && hub.is_enabled(ext_id);
        let (configured, in_cooldown) = match kind {
            RoutingProviderKind::Osrm => {
                let conf = routing_cfg.is_enabled();
                let mut targets: Vec<&str> =
                    routing_cfg.regions.iter().map(|r| r.base_url.as_str()).collect();
                if let Some(ref b) = routing_cfg.base_url {
                    if !targets.contains(&b.as_str()) {
                        targets.push(b.as_str());
                    }
                }
                let all_cooled = !targets.is_empty() && targets.iter().all(|u| is_router_in_cooldown(u));
                (conf, all_cooled)
            }
            _ => {
                if let Some(ep) = routing_cfg.cloud.get_provider(kind) {
                    tier2_configured += 1;
                    let cooled = is_cloud_provider_in_cooldown(kind, &ep.base_url);
                    if enabled && !cooled {
                        tier2_ready += 1;
                    }
                    (true, cooled)
                } else {
                    (false, false)
                }
            }
        };

        let slot = metrics_slot(kind);
        let req_ok = slot.requests_succeeded.load(Ordering::Relaxed);
        let req_err = slot.requests_failed.load(Ordering::Relaxed);
        let failovers = slot.failovers.load(Ordering::Relaxed);
        total_failovers += failovers;
        let last_error_code = slot.last_error_code.read().ok().and_then(|g| g.clone());

        items.push(RoutingExtensionTelemetryItem {
            extension_id: ext_id.to_string(),
            name: kind.display_name().to_string(),
            tier: kind.tier().to_string(),
            enabled,
            configured,
            in_cooldown,
            requests_succeeded: req_ok,
            requests_failed: req_err,
            failovers,
            last_error_code,
        });
    }

    let summary = json!({
        "strategy": routing_cfg.cloud.strategy.as_str(),
        "configuredFallbacks": tier2_configured,
        "healthyFallbacks": tier2_ready,
        "totalFailovers": total_failovers,
        "services": items,
    });
    (summary, items)
}

// ------------------------------------------------------------------------------------------
// Strict secret & URL redaction
// ------------------------------------------------------------------------------------------

static SECRET_QUERY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)([?&](?:access_token|api_key|apikey|key|token)=)[^&\s\x22']+").unwrap()
});
static BEARER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(Bearer\s+)[A-Za-z0-9._~+/=-]+").unwrap());

/// Redacts sensitive query parameters (`access_token`, `key`, `api_key`, `token`, plus optional
/// custom `extra_param`) in a URL string while keeping scheme, host, path, and non-secret params.
pub fn redact_url_secrets(raw_url: &str, extra_param: Option<&str>, secrets: &[&str]) -> String {
    let mut out = if let Ok(mut parsed) = Url::parse(raw_url) {
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| {
                let kl = k.to_ascii_lowercase();
                let is_secret_key = matches!(
                    kl.as_str(),
                    "access_token" | "key" | "api_key" | "apikey" | "token" | "secret"
                ) || extra_param
                    .map(|ep| !ep.is_empty() && kl == ep.to_ascii_lowercase())
                    .unwrap_or(false);
                if is_secret_key {
                    (k.into_owned(), "[REDACTED]".to_string())
                } else {
                    (k.into_owned(), v.into_owned())
                }
            })
            .collect();
        if !pairs.is_empty() {
            parsed.set_query(None);
            {
                let mut qp = parsed.query_pairs_mut();
                for (k, v) in &pairs {
                    qp.append_pair(k, v);
                }
            }
        }
        // Preserve readable `[REDACTED]` instead of `%5BREDACTED%5D`
        parsed.to_string().replace("%5BREDACTED%5D", "[REDACTED]")
    } else {
        SECRET_QUERY_RE
            .replace_all(raw_url, "${1}[REDACTED]")
            .into_owned()
    };

    for s in secrets {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            out = out.replace(trimmed, "[REDACTED]");
        }
    }
    BEARER_RE.replace_all(&out, "${1}[REDACTED]").into_owned()
}

/// Redacts API tokens, Bearer credentials, and raw URLs from error/warning messages.
pub fn redact_routing_error(raw: &str, secrets: &[&str]) -> String {
    let mut out = SECRET_QUERY_RE
        .replace_all(raw, "${1}[REDACTED]")
        .into_owned();
    out = BEARER_RE.replace_all(&out, "${1}[REDACTED]").into_owned();
    for s in secrets {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            out = out.replace(trimmed, "[REDACTED]");
        }
    }
    sanitize_user_warning(&out)
}

// ------------------------------------------------------------------------------------------
// Wire builders & response parsers for all 4 Tier-2 Cloud Matrix Providers
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCloudMatrix {
    pub durations: Vec<Vec<Option<f64>>>,
    pub distances: Vec<Vec<Option<f64>>>,
    pub source_snap_m: Vec<Option<f64>>,
    pub destination_snap_m: Vec<Option<f64>>,
}

fn format_semicolon_coords(sources: &[(i32, i32)], destinations: &[(i32, i32)]) -> String {
    let mut out = String::with_capacity((sources.len() + destinations.len()) * 24);
    for (i, &(lat, lon)) in sources.iter().chain(destinations.iter()).enumerate() {
        if i > 0 {
            out.push(';');
        }
        let _ = write!(out, "{:.5},{:.5}", f64::from(lon) / 1e5, f64::from(lat) / 1e5);
    }
    out
}

fn format_semicolon_indices(range: Range<usize>) -> String {
    range
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(";")
}

/// Mapbox Directions Matrix API v1 URL:
/// `GET {base}/directions-matrix/v1/mapbox/driving/{lon,lat;...}?sources=0;1&destinations=2;3&annotations=duration,distance&access_token={token}`
///
/// Mapbox Matrix v1 requires `sources.len() * destinations.len() >= 2`. For a 1×1 request
/// (`sources.len() == 1 && destinations.len() == 1`), we keep the 2 coordinates `{src};{dst}` and
/// pass `sources=0&destinations=0;1` (a 1×2 matrix), then extract column `1` in
/// [`parse_cloud_matrix_response`].
pub fn build_mapbox_matrix_url(
    base_url: &str,
    sources: &[(i32, i32)],
    destinations: &[(i32, i32)],
    access_token: &str,
) -> String {
    let coords = format_semicolon_coords(sources, destinations);
    let n = sources.len() + destinations.len();
    let src_idx = format_semicolon_indices(0..sources.len());
    let dst_idx = if sources.len() == 1 && destinations.len() == 1 {
        "0;1".to_string()
    } else {
        format_semicolon_indices(sources.len()..n)
    };
    let encoded_token: String =
        url::form_urlencoded::byte_serialize(access_token.as_bytes()).collect();
    format!(
        "{}/directions-matrix/v1/mapbox/driving/{coords}?sources={src_idx}&destinations={dst_idx}&annotations=duration,distance&access_token={encoded_token}",
        base_url.trim_end_matches('/')
    )
}

/// LocationIQ Matrix API v1 URL:
/// `GET {base}/v1/matrix/driving/{lon,lat;...}?sources=0;1&destinations=2;3&annotations=duration,distance&key={key}`
pub fn build_locationiq_matrix_url(
    base_url: &str,
    sources: &[(i32, i32)],
    destinations: &[(i32, i32)],
    api_key: &str,
) -> String {
    let coords = format_semicolon_coords(sources, destinations);
    let n = sources.len() + destinations.len();
    let src_idx = format_semicolon_indices(0..sources.len());
    let dst_idx = format_semicolon_indices(sources.len()..n);
    let encoded_key: String = url::form_urlencoded::byte_serialize(api_key.as_bytes()).collect();
    format!(
        "{}/v1/matrix/driving/{coords}?sources={src_idx}&destinations={dst_idx}&annotations=duration,distance&key={encoded_key}",
        base_url.trim_end_matches('/')
    )
}

/// FreeRoute Matrix API v1 POST URL + JSON body:
/// `POST {base}/v1/matrix/driving-car`
pub fn build_freeroute_matrix_request(
    base_url: &str,
    sources: &[(i32, i32)],
    destinations: &[(i32, i32)],
) -> (String, Value) {
    let url = format!("{}/v1/matrix/driving-car", base_url.trim_end_matches('/'));
    let locations: Vec<[f64; 2]> = sources
        .iter()
        .chain(destinations.iter())
        .map(|&(lat, lon)| {
            [
                (f64::from(lon) / 1e5 * 1e5).round() / 1e5,
                (f64::from(lat) / 1e5 * 1e5).round() / 1e5,
            ]
        })
        .collect();
    let n = sources.len() + destinations.len();
    let src_idx: Vec<usize> = (0..sources.len()).collect();
    let dst_idx: Vec<usize> = (sources.len()..n).collect();
    let body = json!({
        "locations": locations,
        "sources": src_idx,
        "destinations": dst_idx,
        "metrics": ["duration", "distance"],
    });
    (url, body)
}

/// OpenRouteService v2 Matrix API POST URL + JSON body:
/// `POST {base}/v2/matrix/driving-car`
pub fn build_openrouteservice_matrix_request(
    base_url: &str,
    sources: &[(i32, i32)],
    destinations: &[(i32, i32)],
) -> (String, Value) {
    let url = format!("{}/v2/matrix/driving-car", base_url.trim_end_matches('/'));
    let locations: Vec<[f64; 2]> = sources
        .iter()
        .chain(destinations.iter())
        .map(|&(lat, lon)| {
            [
                (f64::from(lon) / 1e5 * 1e5).round() / 1e5,
                (f64::from(lat) / 1e5 * 1e5).round() / 1e5,
            ]
        })
        .collect();
    let n = sources.len() + destinations.len();
    let src_idx: Vec<usize> = (0..sources.len()).collect();
    let dst_idx: Vec<usize> = (sources.len()..n).collect();
    let body = json!({
        "locations": locations,
        "sources": src_idx,
        "destinations": dst_idx,
        "metrics": ["duration", "distance"],
        "units": "m",
    });
    (url, body)
}

fn parse_2d_opt_f64(v: Option<&Value>, rows: usize, cols: usize) -> Option<Vec<Vec<Option<f64>>>> {
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

fn parse_snap_distances(v: Option<&Value>, n: usize) -> Vec<Option<f64>> {
    let arr = v.and_then(Value::as_array);
    (0..n)
        .map(|i| {
            arr.and_then(|a| a.get(i))
                .and_then(|w| w.get("snapped_distance").or_else(|| w.get("distance")))
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite() && *x >= 0.0)
        })
        .collect()
}

pub fn parse_cloud_matrix_response(
    kind: RoutingProviderKind,
    payload: &Value,
    rows: usize,
    cols: usize,
) -> Result<ParsedCloudMatrix, String> {
    if let Some(err_obj) = payload.get("error") {
        let detail = err_obj
            .as_str()
            .or_else(|| err_obj.get("message").and_then(Value::as_str))
            .unwrap_or("upstream error");
        let short: String = detail.chars().take(120).collect();
        return Err(format!("{} returned error ({short})", kind.display_name()));
    }

    match kind {
        RoutingProviderKind::Mapbox | RoutingProviderKind::LocationIq | RoutingProviderKind::Osrm => {
            let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
            if code != "Ok" {
                let msg: String = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect();
                return Err(format!("{} returned code {code:?} {msg}", kind.display_name())
                    .trim()
                    .to_string());
            }
        }
        RoutingProviderKind::FreeRoute => {
            if let Some(code) = payload.get("code").and_then(Value::as_str) {
                if !code.eq_ignore_ascii_case("Ok") {
                    let msg: String = payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .collect();
                    return Err(
                        format!("{} returned code {code:?} {msg}", kind.display_name())
                            .trim()
                            .to_string(),
                    );
                }
            }
        }
        RoutingProviderKind::OpenRouteService => {
            // ORS 200 OK JSON has no `"code"` field (`{"durations": [...], "distances": [...]}`).
        }
    }

    // Mapbox Matrix v1 requires `sources * destinations >= 2`, so 1x1 calls request
    // `sources=0&destinations=0;1` (1x2) and extract column `1` (`[0][1]`) + `destinations[1]` snap drift.
    if kind == RoutingProviderKind::Mapbox && rows == 1 && cols == 1 {
        if let (Some(durs_1x2), Some(dists_1x2)) = (
            parse_2d_opt_f64(payload.get("durations"), 1, 2),
            parse_2d_opt_f64(payload.get("distances"), 1, 2),
        ) {
            let dest_snaps = parse_snap_distances(payload.get("destinations"), 2);
            return Ok(ParsedCloudMatrix {
                durations: vec![vec![durs_1x2[0][1]]],
                distances: vec![vec![dists_1x2[0][1]]],
                source_snap_m: parse_snap_distances(payload.get("sources"), 1),
                destination_snap_m: vec![dest_snaps.into_iter().nth(1).flatten()],
            });
        }
    }

    let durations = parse_2d_opt_f64(payload.get("durations"), rows, cols).ok_or_else(|| {
        format!(
            "{} response missing {rows}x{cols} durations matrix",
            kind.display_name()
        )
    })?;
    let distances = parse_2d_opt_f64(payload.get("distances"), rows, cols).ok_or_else(|| {
        format!(
            "{} response missing {rows}x{cols} distances matrix",
            kind.display_name()
        )
    })?;

    Ok(ParsedCloudMatrix {
        durations,
        distances,
        source_snap_m: parse_snap_distances(payload.get("sources"), rows),
        destination_snap_m: parse_snap_distances(payload.get("destinations"), cols),
    })
}

// ------------------------------------------------------------------------------------------
// Direct-egress cloud matrix execution (honoring `offline_loopback_only`, bypassing proxies)
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CloudFetchFailure {
    pub status_code: Option<u16>,
    pub code_label: String,
    pub sanitized_reason: String,
    pub offline_blocked: bool,
}

impl CloudFetchFailure {
    pub fn cooldown_secs(&self, configured_external_cooldown_secs: u64) -> u64 {
        if self.offline_blocked || configured_external_cooldown_secs == 0 {
            return 0;
        }
        match self.status_code {
            Some(401 | 403 | 429) => AUTH_OR_RATE_LIMIT_COOLDOWN_SECS,
            _ => configured_external_cooldown_secs,
        }
    }
}

pub async fn execute_cloud_matrix_request(
    http: &HttpClient,
    endpoint: &CloudProviderEndpoint,
    sources: &[(i32, i32)],
    destinations: &[(i32, i32)],
    external_cooldown_secs: u64,
    attempted_requests: &AtomicUsize,
) -> Result<ParsedCloudMatrix, CloudFetchFailure> {
    if sources.len() + destinations.len() > MAX_CLOUD_MATRIX_LOCATIONS {
        return Err(CloudFetchFailure {
            status_code: None,
            code_label: "CHUNK_TOO_LARGE".to_string(),
            sanitized_reason: format!(
                "{} chunk exceeded {} coordinates",
                endpoint.kind.display_name(),
                MAX_CLOUD_MATRIX_LOCATIONS
            ),
            offline_blocked: false,
        });
    }

    let ck = endpoint.cooldown_key();
    if external_cooldown_secs > 0 {
        if let Some(cached_warn) = active_router_cooldown(&ck) {
            return Err(CloudFetchFailure {
                status_code: None,
                code_label: "COOLDOWN".to_string(),
                sanitized_reason: cached_warn,
                offline_blocked: false,
            });
        }
    }

    if endpoint.kind == RoutingProviderKind::LocationIq {
        let limiter_key = endpoint.base_url.clone();
        LOCATIONIQ_LIMITER.until_key_ready(&limiter_key).await;
    }

    let (method, url, headers, body): (&str, String, Vec<(&str, String)>, Option<Value>) =
        match endpoint.kind {
            RoutingProviderKind::Mapbox => (
                "GET",
                build_mapbox_matrix_url(
                    &endpoint.base_url,
                    sources,
                    destinations,
                    &endpoint.api_key,
                ),
                vec![("Accept", "application/json".to_string())],
                None,
            ),
            RoutingProviderKind::LocationIq => (
                "GET",
                build_locationiq_matrix_url(
                    &endpoint.base_url,
                    sources,
                    destinations,
                    &endpoint.api_key,
                ),
                vec![("Accept", "application/json".to_string())],
                None,
            ),
            RoutingProviderKind::FreeRoute => {
                let (u, b) =
                    build_freeroute_matrix_request(&endpoint.base_url, sources, destinations);
                let mut hdrs = vec![("Accept", "application/json".to_string())];
                if !endpoint.api_key.trim().is_empty() {
                    hdrs.push(("Authorization", format!("Bearer {}", endpoint.api_key)));
                }
                ("POST", u, hdrs, Some(b))
            }
            RoutingProviderKind::OpenRouteService => {
                let (u, b) = build_openrouteservice_matrix_request(
                    &endpoint.base_url,
                    sources,
                    destinations,
                );
                (
                    "POST",
                    u,
                    vec![
                        ("Authorization", endpoint.api_key.clone()),
                        ("Accept", "application/json".to_string()),
                    ],
                    Some(b),
                )
            }
            RoutingProviderKind::Osrm => {
                return Err(CloudFetchFailure {
                    status_code: None,
                    code_label: "INVALID_ENGINE".to_string(),
                    sanitized_reason: "OSRM is Tier-1 primary".to_string(),
                    offline_blocked: false,
                });
            }
        };

    let client = match http.direct_client_for_url(&url) {
        Ok(c) => c,
        Err(e) => {
            let msg = redact_routing_error(&e.to_string(), &[endpoint.api_key.as_str()]);
            return Err(CloudFetchFailure {
                status_code: None,
                code_label: "OFFLINE_BLOCKED".to_string(),
                sanitized_reason: msg,
                offline_blocked: true,
            });
        }
    };

    attempted_requests.fetch_add(1, Ordering::Relaxed);
    let timeout = Duration::from_millis(endpoint.timeout_ms.clamp(MIN_EXTERNAL_TIMEOUT_MS, MAX_EXTERNAL_TIMEOUT_MS));
    let mut req = match method {
        "POST" => client.post(&url),
        _ => client.get(&url),
    }
    .timeout(timeout);

    for (hk, hv) in headers {
        req = req.header(hk, hv);
    }
    if let Some(ref json_body) = body {
        req = req.json(json_body);
    }

    let send_res = req.send().await;
    let resp = match send_res {
        Ok(r) => r,
        Err(e) => {
            let code_lbl = if e.is_timeout() {
                "TIMEOUT"
            } else {
                "TRANSPORT"
            };
            let clean = redact_routing_error(
                &format!(
                    "{} transport error ({})",
                    endpoint.kind.display_name(),
                    if e.is_timeout() { "timed out" } else { "connection failed" }
                ),
                &[endpoint.api_key.as_str()],
            );
            record_routing_failure(endpoint.kind, code_lbl);
            let fail = CloudFetchFailure {
                status_code: None,
                code_label: code_lbl.to_string(),
                sanitized_reason: clean.clone(),
                offline_blocked: false,
            };
            let cd = fail.cooldown_secs(external_cooldown_secs);
            record_router_cooldown(&ck, cd, &clean);
            return Err(fail);
        }
    };

    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let code_lbl = format!("HTTP_{status}");
        let clean = format!("{} returned HTTP {status}", endpoint.kind.display_name());
        record_routing_failure(endpoint.kind, &code_lbl);
        let fail = CloudFetchFailure {
            status_code: Some(status),
            code_label: code_lbl,
            sanitized_reason: clean.clone(),
            offline_blocked: false,
        };
        let cd = fail.cooldown_secs(external_cooldown_secs);
        record_router_cooldown(&ck, cd, &clean);
        return Err(fail);
    }

    let payload: Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => {
            let clean = format!("{} returned invalid JSON payload", endpoint.kind.display_name());
            record_routing_failure(endpoint.kind, "INVALID_JSON");
            let fail = CloudFetchFailure {
                status_code: Some(status),
                code_label: "INVALID_JSON".to_string(),
                sanitized_reason: clean.clone(),
                offline_blocked: false,
            };
            let cd = fail.cooldown_secs(external_cooldown_secs);
            record_router_cooldown(&ck, cd, &clean);
            return Err(fail);
        }
    };

    match parse_cloud_matrix_response(endpoint.kind, &payload, sources.len(), destinations.len()) {
        Ok(parsed) => {
            clear_router_cooldown(&ck);
            record_routing_success(endpoint.kind);
            Ok(parsed)
        }
        Err(parse_err) => {
            let clean = redact_routing_error(&parse_err, &[endpoint.api_key.as_str()]);
            record_routing_failure(endpoint.kind, "INVALID_PAYLOAD");
            let fail = CloudFetchFailure {
                status_code: Some(status),
                code_label: "INVALID_PAYLOAD".to_string(),
                sanitized_reason: clean.clone(),
                offline_blocked: false,
            };
            let cd = fail.cooldown_secs(external_cooldown_secs);
            record_router_cooldown(&ck, cd, &clean);
            Err(fail)
        }
    }
}

/// Orders the currently healthy, enabled, configured Tier-2 fallback services according to
/// `ROUTING_FALLBACK_ORDER` (when set) or `ExtensionHub` manifest priorities.
pub fn build_eligible_cloud_pool(
    cloud_cfg: &CloudFallbackPoolConfig,
    hub: &ExtensionHub,
) -> Vec<CloudProviderEndpoint> {
    if !is_routing_module_enabled(hub) {
        return Vec::new();
    }
    let manifests = hub.list_manifests();
    let prio_map: HashMap<&str, i32> = manifests
        .iter()
        .map(|m| (m.extension_id.as_str(), m.priority))
        .collect();

    let mut available: Vec<CloudProviderEndpoint> = cloud_cfg
        .providers
        .iter()
        .filter(|ep| {
            hub.is_enabled(ep.kind.extension_id())
                && !is_cloud_provider_in_cooldown(ep.kind, &ep.base_url)
        })
        .cloned()
        .collect();

    if !cloud_cfg.fallback_order.is_empty() {
        let mut ordered = Vec::with_capacity(available.len());
        for wanted in &cloud_cfg.fallback_order {
            if let Some(pos) = available.iter().position(|ep| ep.kind == *wanted) {
                ordered.push(available.remove(pos));
            }
        }
        available.sort_by(|a, b| {
            let pa = prio_map
                .get(a.kind.extension_id())
                .copied()
                .unwrap_or_else(|| a.kind.default_priority());
            let pb = prio_map
                .get(b.kind.extension_id())
                .copied()
                .unwrap_or_else(|| b.kind.default_priority());
            pb.cmp(&pa)
                .then_with(|| a.kind.extension_id().cmp(b.kind.extension_id()))
        });
        ordered.extend(available);
        ordered
    } else {
        available.sort_by(|a, b| {
            let pa = prio_map
                .get(a.kind.extension_id())
                .copied()
                .unwrap_or_else(|| a.kind.default_priority());
            let pb = prio_map
                .get(b.kind.extension_id())
                .copied()
                .unwrap_or_else(|| b.kind.default_priority());
            pb.cmp(&pa)
                .then_with(|| a.kind.extension_id().cmp(b.kind.extension_id()))
        });
        available
    }
}

// ------------------------------------------------------------------------------------------
// Live 2-point diagnostic probe for `ExtensionHub::test_fetch` & `execute_single_entry`
// ------------------------------------------------------------------------------------------

const DEFAULT_PROBE_ORIGIN: LatLon = LatLon {
    lat: 37.3382,
    lon: -121.8863,
};

fn probe_pair_from_query(query: &ExtensionQueryProto) -> (LatLon, LatLon) {
    let origin = match (query.lat, query.lon) {
        (Some(lat), Some(lon)) if LatLon::new(lat, lon).is_valid() => LatLon::new(lat, lon),
        _ => DEFAULT_PROBE_ORIGIN,
    };
    let dest = LatLon::new(
        (origin.lat + 0.01).clamp(-89.9, 89.9),
        (origin.lon + 0.01).clamp(-179.9, 179.9),
    );
    (origin, dest)
}

fn to_key_pair(pt: LatLon) -> (i32, i32) {
    ((pt.lat * 1e5).round() as i32, (pt.lon * 1e5).round() as i32)
}

/// Builds a strictly redacted `http_preview` JSON summary for `/admin` `test_fetch` on any of
/// the 5 built-in routing extensions (never exposing tokens or raw secret headers).
pub fn preview_routing_extension_http(
    extension_id: &str,
    query: &ExtensionQueryProto,
    env: &EnvConfig,
) -> Option<Value> {
    let kind = RoutingProviderKind::from_id_or_alias(extension_id)?;
    let (origin, dest) = probe_pair_from_query(query);
    let src = [to_key_pair(origin)];
    let dst = [to_key_pair(dest)];
    let cfg = RoutingConfig::from_env(env);

    let preview = match kind {
        RoutingProviderKind::Osrm => {
            let candidates = cfg.resolve_endpoints(origin, dest);
            let target = candidates
                .first()
                .cloned()
                .or_else(|| cfg.regions.first().map(|r| r.base_url.clone()))
                .or_else(|| cfg.base_url.clone());
            match target {
                Some(base) => {
                    let host = url_host_label(&base);
                    json!({
                        "resolved": true,
                        "method": "GET",
                        "url": format!("http(s)://{host}/table/v1/{PROFILE}/{}?sources=0&destinations=1&annotations=duration,distance&generate_hints=false", format_semicolon_coords(&src, &dst)),
                        "headerNames": ["Accept"],
                    })
                }
                None => json!({
                    "resolved": false,
                    "reason": "AUTH_MISSING: neither OSRM_REGIONS_JSON nor OSRM_BASE_URL is configured",
                }),
            }
        }
        RoutingProviderKind::Mapbox => match cfg.cloud.get_provider(RoutingProviderKind::Mapbox) {
            Some(ep) => {
                let raw_url = build_mapbox_matrix_url(&ep.base_url, &src, &dst, &ep.api_key);
                json!({
                    "resolved": true,
                    "method": "GET",
                    "url": redact_url_secrets(&raw_url, Some("access_token"), &[ep.api_key.as_str()]),
                    "headerNames": ["Accept"],
                })
            }
            None => json!({
                "resolved": false,
                "reason": "AUTH_MISSING: MAPBOX_ACCESS_TOKEN is not set",
            }),
        },
        RoutingProviderKind::LocationIq => {
            match cfg.cloud.get_provider(RoutingProviderKind::LocationIq) {
                Some(ep) => {
                    let raw_url =
                        build_locationiq_matrix_url(&ep.base_url, &src, &dst, &ep.api_key);
                    json!({
                        "resolved": true,
                        "method": "GET",
                        "url": redact_url_secrets(&raw_url, Some("key"), &[ep.api_key.as_str()]),
                        "headerNames": ["Accept"],
                    })
                }
                None => json!({
                    "resolved": false,
                    "reason": "AUTH_MISSING: LOCATIONIQ_API_KEY is not set",
                }),
            }
        }
        RoutingProviderKind::FreeRoute => {
            match cfg.cloud.get_provider(RoutingProviderKind::FreeRoute) {
                Some(ep) => {
                    let (u, _) = build_freeroute_matrix_request(&ep.base_url, &src, &dst);
                    let hdr_names = if ep.api_key.trim().is_empty() {
                        vec!["Content-Type", "Accept"]
                    } else {
                        vec!["Authorization", "Content-Type", "Accept"]
                    };
                    json!({
                        "resolved": true,
                        "method": "POST",
                        "url": redact_url_secrets(&u, None, &[ep.api_key.as_str()]),
                        "headerNames": hdr_names,
                    })
                }
                None => json!({
                    "resolved": false,
                    "reason": "AUTH_MISSING: FREEROUTE_API_KEY is not set",
                }),
            }
        }
        RoutingProviderKind::OpenRouteService => {
            match cfg.cloud.get_provider(RoutingProviderKind::OpenRouteService) {
                Some(ep) => {
                    let (u, _) = build_openrouteservice_matrix_request(&ep.base_url, &src, &dst);
                    json!({
                        "resolved": true,
                        "method": "POST",
                        "url": redact_url_secrets(&u, None, &[ep.api_key.as_str()]),
                        "headerNames": ["Authorization", "Content-Type", "Accept"],
                    })
                }
                None => json!({
                    "resolved": false,
                    "reason": "AUTH_MISSING: OPENROUTESERVICE_API_KEY is not set",
                }),
            }
        }
    };
    Some(preview)
}

/// Executes a live 2-point routing matrix probe through the native Rust routing adapter for
/// `ExtensionHub::test_fetch` and `execute_single_entry`, returning `Err(anyhow!("AUTH_MISSING: ..."))`
/// when unconfigured or a populated `RoutingMatrixProto` envelope on success.
pub async fn probe_routing_extension(
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    http: &HttpClient,
    env: &EnvConfig,
) -> Result<Vec<ExtensionEnvelopeProto>> {
    let Some(kind) = RoutingProviderKind::from_id_or_alias(&manifest.extension_id) else {
        return Err(anyhow!(
            "Unsupported built-in routing extension '{}'",
            manifest.extension_id
        ));
    };

    let cfg = RoutingConfig::from_env(env);
    let (origin, dest) = probe_pair_from_query(query);
    let src = [to_key_pair(origin)];
    let dst = [to_key_pair(dest)];
    let started = Instant::now();
    let attempted = AtomicUsize::new(0);

    let (endpoint_host, max_coordinates, sample_dur, sample_dist) = match kind {
        RoutingProviderKind::Osrm => {
            if !cfg.is_enabled() {
                return Err(anyhow!(
                    "AUTH_MISSING: configure OSRM_REGIONS_JSON or OSRM_BASE_URL for '{}'",
                    manifest.extension_id
                ));
            }
            let mut candidates = cfg.resolve_endpoints(origin, dest);
            if candidates.is_empty() {
                for r in &cfg.regions {
                    if !candidates.contains(&r.base_url) {
                        candidates.push(r.base_url.clone());
                    }
                }
                if let Some(ref b) = cfg.base_url {
                    if !candidates.contains(b) {
                        candidates.push(b.clone());
                    }
                }
            }
            let mut last_err = String::from("no reachable OSRM endpoint");
            let mut hit: Option<(String, Option<f64>, Option<f64>)> = None;
            for base in candidates {
                match crate::modules::commute::probe_osrm_table_pair(
                    http,
                    &base,
                    &src,
                    &dst,
                    cfg.timeout_ms,
                    &attempted,
                )
                .await
                {
                    Ok((dur, dist)) => {
                        let host = Url::parse(&base)
                            .ok()
                            .and_then(|u| u.host_str().map(str::to_string))
                            .unwrap_or_else(|| "osrm".to_string());
                        hit = Some((host, dur, dist));
                        break;
                    }
                    Err(e) => {
                        last_err = redact_routing_error(&e, &[]);
                    }
                }
            }
            let (host, dur, dist) = hit.ok_or_else(|| anyhow!("{last_err}"))?;
            (host, cfg.max_table_locations as u32, dur, dist)
        }
        _ => {
            let allow_keyless_fr =
                kind == RoutingProviderKind::FreeRoute && env_allows_freeroute_keyless(env);
            let missing_env = match kind {
                RoutingProviderKind::Mapbox => "MAPBOX_ACCESS_TOKEN (or MAPBOX_API_KEY)",
                RoutingProviderKind::LocationIq => "LOCATIONIQ_API_KEY (or LOCATIONIQ_ACCESS_TOKEN)",
                RoutingProviderKind::FreeRoute => "FREEROUTE_API_KEY",
                RoutingProviderKind::OpenRouteService => {
                    "OPENROUTESERVICE_API_KEY (or ORS_API_KEY)"
                }
                RoutingProviderKind::Osrm => unreachable!(),
            };
            let ep_owned: CloudProviderEndpoint;
            let ep = match cfg.cloud.get_provider(kind) {
                Some(found) => found,
                None if allow_keyless_fr => {
                    let base = env
                        .freeroute_base_url()
                        .unwrap_or(DEFAULT_FREEROUTE_BASE_URL)
                        .trim()
                        .trim_end_matches('/')
                        .to_string();
                    ep_owned = CloudProviderEndpoint {
                        kind: RoutingProviderKind::FreeRoute,
                        api_key: String::new(),
                        base_url: base,
                        timeout_ms: cfg.cloud.external_timeout_ms,
                    };
                    &ep_owned
                }
                None => {
                    return Err(anyhow!(
                        "AUTH_MISSING: environment variable {} is not set for extension '{}'",
                        missing_env,
                        manifest.extension_id
                    ));
                }
            };
            let parsed = execute_cloud_matrix_request(
                http,
                ep,
                &src,
                &dst,
                cfg.cloud.external_cooldown_secs,
                &attempted,
            )
            .await
            .map_err(|f| anyhow!("{}", redact_routing_error(&f.sanitized_reason, &[ep.api_key.as_str()])))?;

            let dur = parsed
                .durations
                .first()
                .and_then(|r| r.first())
                .copied()
                .flatten();
            let dist = parsed
                .distances
                .first()
                .and_then(|r| r.first())
                .copied()
                .flatten();
            (
                ep.host_only(),
                MAX_CLOUD_MATRIX_LOCATIONS as u32,
                dur,
                dist,
            )
        }
    };

    let latency_ms = started.elapsed().as_millis() as u32;
    let (attr_name, attr_url) = kind.extension_source_attribution();
    let proto = RoutingMatrixProto {
        provider_id: kind.extension_id().to_string(),
        provider_name: kind.display_name().to_string(),
        tier: kind.tier().to_string(),
        profile: kind.profile_name().to_string(),
        endpoint_host,
        max_coordinates,
        origin_count: 1,
        destination_count: 1,
        sample_duration_sec: sample_dur,
        sample_distance_m: sample_dist,
        healthy: true,
    };

    Ok(vec![ExtensionEnvelopeProto {
        extension_id: manifest.extension_id.clone(),
        domain: CapabilityDomain::DomainRouting as i32,
        priority: manifest.priority,
        geo_specificity: 10,
        attribution: Some(SourceAttributionProto {
            extension_id: manifest.extension_id.clone(),
            source_name: attr_name.to_string(),
            source_url: attr_url.to_string(),
            provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
            fetched_at_rfc3339: now_iso(),
            schedule_or_version: None,
            notes: None,
        }),
        latency_ms,
        cached: false,
        payload: Some(Payload::RoutingMatrix(proto)),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Instant;

    #[tokio::test]
    async fn unit_locationiq_limiter_enforces_burst_one_spacing_across_three_calls() {
        let key = format!("unit-liq-{}", uuid::Uuid::new_v4());
        let t0 = Instant::now();
        LOCATIONIQ_LIMITER.until_key_ready(&key).await;
        LOCATIONIQ_LIMITER.until_key_ready(&key).await;
        LOCATIONIQ_LIMITER.until_key_ready(&key).await;
        let elapsed = t0.elapsed();
        assert!(
            elapsed.as_millis() >= 1100,
            "LOCATIONIQ_LIMITER must enforce >=1100ms across 3 consecutive acquisitions (burst=1), got {:?}",
            elapsed
        );
    }

    #[test]
    fn unit_routing_module_enable_and_freeroute_keyless_opt_in() {
        let hub = crate::extensions::ExtensionHub::new_default();
        assert!(is_routing_module_enabled(&hub));

        hub.set_module_enabled("neighborhood_livability", false)
            .unwrap();
        assert!(
            !is_routing_module_enabled(&hub),
            "routing must disable when parent neighborhood_livability module is off"
        );

        hub.set_module_enabled("neighborhood_livability", true)
            .unwrap();
        hub.set_module_enabled("commute_transit", false).unwrap();
        assert!(
            !is_routing_module_enabled(&hub),
            "routing must disable when commute_transit module is off"
        );

        // FreeRoute stays unconfigured by default so AUTH_MISSING contract holds unless explicitly opted in
        let empty = EnvConfig::from_map(HashMap::new());
        assert_eq!(
            is_routing_extension_configured("freeroute_routing", Some(&empty)),
            Some(false)
        );
        let opted_in = EnvConfig::from_map(HashMap::from([(
            ENV_FREEROUTE_ALLOW_KEYLESS.to_string(),
            "true".to_string(),
        )]));
        assert_eq!(
            is_routing_extension_configured("freeroute_routing", Some(&opted_in)),
            Some(true)
        );
    }
}
