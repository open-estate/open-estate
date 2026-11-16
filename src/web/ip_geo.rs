//! Real, zero-mock IP-based US location & 5-digit ZIP detection (`GET /api/geo/ip`).
//!
//! Resolution pipeline (strictly honest — never falls back to a hardcoded default ZIP):
//! 1. **Admin Enablement Gate**: Checks `IP_GEOLOCATION_ENABLED` env flag plus `system_modules`
//!    and `installed_extensions` (`ip_geolocation`) in `DbStore`. Returns
//!    `{ "ok": false, "reason": "ip_geolocation_disabled" }` when disabled by an administrator.
//! 2. **Step A — Edge / Reverse-Proxy Geo Headers (zero-latency)**: Inspects Cloudflare
//!    (`cf-ipcountry`, `cf-ipcity`, `cf-region-code`, `cf-postal-code`, `cf-iplatitude`,
//!    `cf-iplongitude`), Vercel (`x-vercel-ip-*`), and Google Cloud / App Engine
//!    (`x-appengine-*`) headers.
//! 3. **Step B — Client IP Extraction & Prefix Bucketing**: Extracts the real client IP from
//!    `cf-connecting-ip`, `true-client-ip`, `x-real-ip`, `x-forwarded-for` (first public routable
//!    hop), or the socket peer address. Keys the 1-hour `moka` cache by `/24` IPv4 prefix, `/48`
//!    IPv6 prefix, or `"self_egress"` when the caller is on loopback/private LAN.
//! 4. **Step C — Keyless Upstream IP Geolocation Cascade**:
//!    - Public client IP: `https://ipwho.is/{ip}` -> `https://freeipapi.com/api/json/{ip}` ->
//!      `https://ipapi.co/{ip}/json/`.
//!    - Loopback/private client IP (e.g. local self-hosted browser): queries `https://ipwho.is/` ->
//!      `https://freeipapi.com/api/json` -> `https://ipapi.co/json/` with no IP path suffix so the
//!      upstream service geolocates the server's actual public internet egress IP.
//! 5. **Step D — U.S. Census TIGER Geographies ZCTA & Metro Verification**: Resolves US
//!    coordinates via `geocoding.geo.census.gov/geocoder/geographies/coordinates` to attach
//!    verified 5-digit Census ZCTA5 postal codes, city/state fallbacks, and CBSA metro names.
//! 6. **Step E — Strict Zero-Mock Honesty**: Outside the US or when network/postal resolution
//!    cannot confirm a real US 5-digit ZIP, returns `{ "ok": false, "reason": "...", ... }`.

use crate::guardrails::apply_security_headers;
use crate::registry::endpoints::CENSUS_GEOCODER_GEOS_BASE;
use crate::registry::us_geo::normalize_us_state_abbr;
use crate::shared::title_case;
use crate::web::server::{AppState, ClientIp};
use axum::{
    extract::{ConnectInfo, Query, State},
    http::{header, Extensions, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;
use url::Url;

pub const IP_GEOLOCATION_EXTENSION_ID: &str = "ip_geolocation";

const IP_GEO_CACHE_TTL_SECS: u64 = 3600;
const IP_GEO_CACHE_MAX_CAPACITY: u64 = 16_384;

/// Canonical IP-derived location result returned by `GET /api/geo/ip`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpGeoLocation {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metro: Option<String>,
    pub detection_method: String,
    pub attribution: String,
}

impl IpGeoLocation {
    pub fn failed(
        country_code: Option<String>,
        detection_method: impl Into<String>,
        attribution: impl Into<String>,
    ) -> Self {
        Self {
            ok: false,
            postal_code: None,
            city: None,
            region_code: None,
            country_code,
            lat: None,
            lon: None,
            metro: None,
            detection_method: detection_method.into(),
            attribution: attribution.into(),
        }
    }
}

/// Full outcome of an IP geolocation attempt, pairing `IpGeoLocation` with failure `reason`
/// and cache hit telemetry.
#[derive(Clone, Debug, PartialEq)]
pub struct IpGeoResolution {
    pub location: IpGeoLocation,
    pub reason: Option<String>,
    pub cache_hit: bool,
}

impl IpGeoResolution {
    pub fn ok(location: IpGeoLocation) -> Self {
        Self {
            location,
            reason: None,
            cache_hit: false,
        }
    }

    pub fn err(
        reason: impl Into<String>,
        country_code: Option<String>,
        detection_method: impl Into<String>,
        attribution: impl Into<String>,
    ) -> Self {
        Self {
            location: IpGeoLocation::failed(country_code, detection_method, attribution),
            reason: Some(reason.into()),
            cache_hit: false,
        }
    }

    pub fn to_api_json(&self) -> Value {
        if self.location.ok {
            let mut val =
                serde_json::to_value(&self.location).unwrap_or_else(|_| json!({ "ok": false }));
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "source".to_string(),
                    Value::String(self.location.attribution.clone()),
                );
            }
            val
        } else {
            let mut map = serde_json::Map::new();
            map.insert("ok".to_string(), Value::Bool(false));
            if let Some(ref r) = self.reason {
                map.insert("reason".to_string(), Value::String(r.clone()));
            }
            if let Some(ref cc) = self.location.country_code {
                map.insert("countryCode".to_string(), Value::String(cc.clone()));
            }
            if !self.location.detection_method.is_empty() {
                map.insert(
                    "detectionMethod".to_string(),
                    Value::String(self.location.detection_method.clone()),
                );
            }
            if !self.location.attribution.is_empty() {
                map.insert(
                    "attribution".to_string(),
                    Value::String(self.location.attribution.clone()),
                );
            }
            Value::Object(map)
        }
    }
}

static IP_GEO_CACHE: Lazy<Cache<String, IpGeoResolution>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(IP_GEO_CACHE_MAX_CAPACITY)
        .time_to_live(Duration::from_secs(IP_GEO_CACHE_TTL_SECS))
        .build()
});

/// Dedicated direct HTTPS client for IP geolocation and Census ZCTA coordinate verification.
/// Uses `.no_proxy()` by default so rotating data-plane proxy pools never mask `"self_egress"`
/// geolocation on self-hosted deployments.
static IP_GEO_HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let use_env_proxy = std::env::var("IP_GEO_USE_ENV_PROXY")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let mut builder = reqwest::Client::builder()
        .user_agent("Open Estate/1.0 (+https://openestate.org)")
        .connect_timeout(Duration::from_millis(2_500))
        .timeout(Duration::from_millis(4_500))
        .redirect(reqwest::redirect::Policy::limited(5))
        .tcp_nodelay(true);

    if !use_env_proxy {
        builder = builder.no_proxy();
    }

    builder.build().unwrap_or_default()
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientIpTarget {
    Public(IpAddr),
    SelfEgress,
}

impl ClientIpTarget {
    pub fn cache_key(&self) -> String {
        match self {
            Self::Public(ip) => ip_prefix_cache_key(*ip),
            Self::SelfEgress => "self_egress".to_string(),
        }
    }
}

/// Canonicalizes IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) to `IpAddr::V4`.
pub fn canonicalize_ip_addr(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
    }
}

/// Returns `true` strictly for globally routable public unicast IPv4/IPv6 addresses.
/// Loopback (`127.0.0.0/8`, `::1`), RFC 1918 private LAN (`10/8`, `172.16/12`, `192.168/16`),
/// CGNAT/Tailscale (`100.64/10`), link-local (`169.254/16`, `fe80::/10`), ULA (`fc00::/7`),
/// documentation/test ranges (`192.0.2/24`, `198.51.100/24`, `203.0.113/24`, `2001:db8::/32`),
/// benchmarking (`198.18/15`), multicast, unspecified, and broadcast addresses return `false`.
pub fn is_public_routable_ip(raw_ip: IpAddr) -> bool {
    match canonicalize_ip_addr(raw_ip) {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(v4: Ipv4Addr) -> bool {
    if v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
    {
        return false;
    }
    let o = v4.octets();
    // Shared Address Space / CGNAT / Tailscale (RFC 6598: 100.64.0.0/10)
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return false;
    }
    // IETF Protocol Assignments (192.0.0.0/24)
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return false;
    }
    // Network Benchmark Testing (RFC 2544: 198.18.0.0/15)
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return false;
    }
    // Reserved for future use (240.0.0.0/4) and 0.0.0.0/8 ("this" network)
    if o[0] == 0 || o[0] >= 240 {
        return false;
    }
    true
}

fn is_public_ipv6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
        return false;
    }
    let seg = v6.segments();
    // Unique Local Addresses (fc00::/7)
    if (seg[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // Link-Local Unicast (fe80::/10)
    if (seg[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // Documentation prefix (2001:db8::/32)
    if seg[0] == 0x2001 && seg[1] == 0x0db8 {
        return false;
    }
    // Discard-Only prefix (100::/64)
    if seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 {
        return false;
    }
    true
}

/// Computes the privacy-preserving cache prefix key: `/24` for IPv4 or `/48` for IPv6.
pub fn ip_prefix_cache_key(ip: IpAddr) -> String {
    match canonicalize_ip_addr(ip) {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("v4:{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("v6:{:04x}:{:04x}:{:04x}::/48", s[0], s[1], s[2])
        }
    }
}

fn parse_ip_token(raw: &str) -> Option<IpAddr> {
    let trimmed = raw.trim().trim_matches('"');
    if trimmed.is_empty() || trimmed.len() > 64 {
        return None;
    }
    if let Ok(ip) = IpAddr::from_str(trimmed) {
        return Some(canonicalize_ip_addr(ip));
    }
    if let Ok(sa) = SocketAddr::from_str(trimmed) {
        return Some(canonicalize_ip_addr(sa.ip()));
    }
    // Strip ":port" on unbracketed IPv4 strings such as "203.0.113.9:54321"
    if trimmed.matches(':').count() == 1 {
        if let Some((host, _port)) = trimmed.rsplit_once(':') {
            if let Ok(v4) = Ipv4Addr::from_str(host.trim()) {
                return Some(IpAddr::V4(v4));
            }
        }
    }
    None
}

/// Step B: Extracts the caller's real public IP from reverse-proxy headers (`cf-connecting-ip`,
/// `true-client-ip`, `x-real-ip`, `x-forwarded-for` first public hop) or socket peer address ONLY
/// when `trust_proxy` is enabled AND `peer_ip` passes `is_trusted_proxy_peer` (with Cloudflare
/// headers gated additionally on `trust_cf_headers`).
/// Falls back to `ClientIpTarget::SelfEgress` when the caller is on loopback/private LAN.
pub fn extract_client_ip_target(
    headers: &HeaderMap,
    peer_ip: Option<IpAddr>,
    middleware_ip: Option<&str>,
    trust_proxy: bool,
    trust_cf_headers: bool,
) -> ClientIpTarget {
    let trusted_proxy_peer =
        trust_proxy && peer_ip.is_some_and(crate::guardrails::is_trusted_proxy_peer);

    if trusted_proxy_peer {
        if trust_cf_headers {
            for hdr_name in ["cf-connecting-ip", "true-client-ip"] {
                if let Some(ip) = headers
                    .get(hdr_name)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_ip_token)
                {
                    if is_public_routable_ip(ip) {
                        return ClientIpTarget::Public(ip);
                    }
                }
            }
        }

        if let Some(ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_ip_token)
        {
            if is_public_routable_ip(ip) {
                return ClientIpTarget::Public(ip);
            }
        }

        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            for hop in xff.split(',').take(16) {
                if let Some(ip) = parse_ip_token(hop) {
                    if is_public_routable_ip(ip) {
                        return ClientIpTarget::Public(ip);
                    }
                }
            }
        }
    }

    if let Some(ip) = peer_ip.map(canonicalize_ip_addr) {
        if is_public_routable_ip(ip) {
            return ClientIpTarget::Public(ip);
        }
    }

    if let Some(ip) = middleware_ip.and_then(parse_ip_token) {
        if is_public_routable_ip(ip) {
            return ClientIpTarget::Public(ip);
        }
    }

    ClientIpTarget::SelfEgress
}

/// Validates and normalizes a US 5-digit ZIP / ZCTA5 (`00501`..=`99950`), stripping optional
/// `+4` suffix (`94043-1351` -> `94043`).
pub fn normalize_us_zip5(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() < 5 || trimmed.len() > 12 {
        return None;
    }
    let zip5 = &trimmed[..5];
    if !zip5.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rest = trimmed[5..].trim();
    if !rest.is_empty() {
        let valid_plus4 = rest.starts_with('-')
            && rest.len() == 5
            && rest[1..].chars().all(|c| c.is_ascii_digit());
        if !valid_plus4 {
            return None;
        }
    }
    let num: u32 = zip5.parse().ok()?;
    if !(501..=99950).contains(&num) {
        return None;
    }
    Some(zip5.to_string())
}

fn normalize_iso_country(raw: &str) -> Option<String> {
    let up = raw.trim().to_ascii_uppercase();
    if up.len() != 2 || !up.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    // Reject sentinel pseudo-codes emitted by Cloudflare ("XX", "T1") and App Engine ("ZZ").
    if matches!(up.as_str(), "XX" | "ZZ" | "T1") {
        return None;
    }
    Some(up)
}

fn sanitize_geo_label(raw: &str) -> Option<String> {
    let decoded = if raw.contains('%') {
        urlencoding::decode(raw)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| raw.to_string())
    } else {
        raw.to_string()
    };

    let cleaned: String = decoded
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | '"' | '`'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let trimmed = cleaned.trim().trim_matches('-').trim();
    if trimmed.is_empty() || trimmed.len() > 96 {
        return None;
    }
    let low = trimmed.to_ascii_lowercase();
    if matches!(
        low.as_str(),
        "unknown" | "null" | "none" | "n/a" | "na" | "?" | "unspecified"
    ) {
        return None;
    }
    // Title-case all-lowercase strings (such as App Engine `x-appengine-city: mountain view`).
    if trimmed.chars().any(|c| c.is_ascii_lowercase())
        && !trimmed.chars().any(|c| c.is_ascii_uppercase())
    {
        return Some(title_case(trimmed));
    }
    Some(trimmed.to_string())
}

fn parse_coord_pair(lat_opt: Option<f64>, lon_opt: Option<f64>) -> (Option<f64>, Option<f64>) {
    match (lat_opt, lon_opt) {
        (Some(lat), Some(lon))
            if lat.is_finite()
                && lon.is_finite()
                && (-90.0..=90.0).contains(&lat)
                && (-180.0..=180.0).contains(&lon)
                && (lat.abs() > 1e-4 || lon.abs() > 1e-4) =>
        {
            (
                Some((lat * 1_000_000.0).round() / 1_000_000.0),
                Some((lon * 1_000_000.0).round() / 1_000_000.0),
            )
        }
        _ => (None, None),
    }
}

fn is_us_coordinate_bounds(lat: f64, lon: f64) -> bool {
    (13.0..=72.0).contains(&lat)
        && ((-179.9..=-64.5).contains(&lon) || (144.0..=146.5).contains(&lon))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    header_str(headers, name).and_then(|s| s.parse::<f64>().ok())
}

#[derive(Clone, Debug)]
enum EdgeHeaderOutcome {
    Resolved(IpGeoResolution),
    NeedsCensusZcta {
        city: Option<String>,
        region_code: Option<String>,
        lat: f64,
        lon: f64,
        detection_method: String,
        attribution: String,
    },
}

/// Step A: Checks Cloudflare (`cf-*`), Vercel (`x-vercel-ip-*`), and Google Cloud / App Engine
/// (`x-appengine-*`) edge geolocation headers ONLY when `trust_proxy` is enabled AND `peer_ip`
/// passes `is_trusted_proxy_peer` (with Cloudflare headers additionally gated on `trust_cf_headers`).
fn inspect_edge_geo_headers(
    headers: &HeaderMap,
    peer_ip: Option<IpAddr>,
    trust_proxy: bool,
    trust_cf_headers: bool,
) -> Option<EdgeHeaderOutcome> {
    if !trust_proxy || !peer_ip.is_some_and(crate::guardrails::is_trusted_proxy_peer) {
        return None;
    }

    // 1. Cloudflare Edge Geolocation (requires `OPEN_ESTATE_TRUST_CF_HEADERS=true`)
    if trust_cf_headers {
        if let Some(cc) = header_str(headers, "cf-ipcountry").and_then(normalize_iso_country) {
            if cc != "US" {
                return Some(EdgeHeaderOutcome::Resolved(IpGeoResolution::err(
                    "outside_us",
                    Some(cc),
                    "edge_cloudflare",
                    "Cloudflare Edge IP Geolocation",
                )));
            }
            let postal = header_str(headers, "cf-postal-code").and_then(normalize_us_zip5);
            let city = header_str(headers, "cf-ipcity").and_then(sanitize_geo_label);
            let region_code = header_str(headers, "cf-region-code")
                .or_else(|| header_str(headers, "cf-region"))
                .and_then(normalize_us_state_abbr)
                .map(str::to_string);
            let (lat, lon) = parse_coord_pair(
                header_f64(headers, "cf-iplatitude"),
                header_f64(headers, "cf-iplongitude"),
            );
            if let Some(zip) = postal {
                return Some(EdgeHeaderOutcome::Resolved(IpGeoResolution::ok(
                    IpGeoLocation {
                        ok: true,
                        postal_code: Some(zip),
                        city,
                        region_code,
                        country_code: Some("US".to_string()),
                        lat,
                        lon,
                        metro: None,
                        detection_method: "edge_cloudflare".to_string(),
                        attribution: "Cloudflare Edge IP Geolocation".to_string(),
                    },
                )));
            }
            if let (Some(la), Some(lo)) = (lat, lon) {
                if is_us_coordinate_bounds(la, lo) {
                    return Some(EdgeHeaderOutcome::NeedsCensusZcta {
                        city,
                        region_code,
                        lat: la,
                        lon: lo,
                        detection_method: "edge_cloudflare+census_zcta".to_string(),
                        attribution: "Cloudflare Edge + U.S. Census Bureau TIGER Geographies"
                            .to_string(),
                    });
                }
            }
        }
    }

    // 2. Vercel Edge Network Geolocation
    if let Some(cc) = header_str(headers, "x-vercel-ip-country").and_then(normalize_iso_country) {
        if cc != "US" {
            return Some(EdgeHeaderOutcome::Resolved(IpGeoResolution::err(
                "outside_us",
                Some(cc),
                "edge_vercel",
                "Vercel Edge Network Geolocation",
            )));
        }
        let postal = header_str(headers, "x-vercel-ip-postal-code").and_then(normalize_us_zip5);
        let city = header_str(headers, "x-vercel-ip-city").and_then(sanitize_geo_label);
        let region_code = header_str(headers, "x-vercel-ip-country-region")
            .and_then(normalize_us_state_abbr)
            .map(str::to_string);
        let (lat, lon) = parse_coord_pair(
            header_f64(headers, "x-vercel-ip-latitude"),
            header_f64(headers, "x-vercel-ip-longitude"),
        );
        if let Some(zip) = postal {
            return Some(EdgeHeaderOutcome::Resolved(IpGeoResolution::ok(
                IpGeoLocation {
                    ok: true,
                    postal_code: Some(zip),
                    city,
                    region_code,
                    country_code: Some("US".to_string()),
                    lat,
                    lon,
                    metro: None,
                    detection_method: "edge_vercel".to_string(),
                    attribution: "Vercel Edge Network Geolocation".to_string(),
                },
            )));
        }
        if let (Some(la), Some(lo)) = (lat, lon) {
            if is_us_coordinate_bounds(la, lo) {
                return Some(EdgeHeaderOutcome::NeedsCensusZcta {
                    city,
                    region_code,
                    lat: la,
                    lon: lo,
                    detection_method: "edge_vercel+census_zcta".to_string(),
                    attribution: "Vercel Edge + U.S. Census Bureau TIGER Geographies".to_string(),
                });
            }
        }
    }

    // 3. Google Cloud / App Engine Edge Geolocation
    if let Some(cc) = header_str(headers, "x-appengine-country").and_then(normalize_iso_country) {
        if cc != "US" {
            return Some(EdgeHeaderOutcome::Resolved(IpGeoResolution::err(
                "outside_us",
                Some(cc),
                "edge_appengine",
                "Google Cloud Edge Geolocation",
            )));
        }
        let postal = header_str(headers, "x-appengine-postal-code").and_then(normalize_us_zip5);
        let city = header_str(headers, "x-appengine-city").and_then(sanitize_geo_label);
        let region_code = header_str(headers, "x-appengine-region")
            .and_then(normalize_us_state_abbr)
            .map(str::to_string);
        let (lat, lon) = header_str(headers, "x-appengine-citylatlong")
            .and_then(|s| s.split_once(','))
            .map(|(la_s, lo_s)| {
                parse_coord_pair(
                    la_s.trim().parse::<f64>().ok(),
                    lo_s.trim().parse::<f64>().ok(),
                )
            })
            .unwrap_or((None, None));

        if let Some(zip) = postal {
            return Some(EdgeHeaderOutcome::Resolved(IpGeoResolution::ok(
                IpGeoLocation {
                    ok: true,
                    postal_code: Some(zip),
                    city,
                    region_code,
                    country_code: Some("US".to_string()),
                    lat,
                    lon,
                    metro: None,
                    detection_method: "edge_appengine".to_string(),
                    attribution: "Google Cloud Edge Geolocation".to_string(),
                },
            )));
        }
        if let (Some(la), Some(lo)) = (lat, lon) {
            if is_us_coordinate_bounds(la, lo) {
                return Some(EdgeHeaderOutcome::NeedsCensusZcta {
                    city,
                    region_code,
                    lat: la,
                    lon: lo,
                    detection_method: "edge_appengine+census_zcta".to_string(),
                    attribution: "Google Cloud Edge + U.S. Census Bureau TIGER Geographies"
                        .to_string(),
                });
            }
        }
    }

    None
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawUpstreamGeo {
    pub country_code: String,
    pub region_code: Option<String>,
    pub city: Option<String>,
    pub postal_code: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub detection_method: String,
    pub attribution: String,
}

fn val_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn val_f64(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| {
        x.as_f64()
            .or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    })
}

/// Parses `https://ipwho.is/{ip}` / `https://ipwho.is/` JSON response.
pub(crate) fn parse_ipwhois_json(j: &Value) -> Option<RawUpstreamGeo> {
    if j.get("success").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let country_code = val_str(j, "country_code").and_then(normalize_iso_country)?;
    let region_code = val_str(j, "region_code")
        .and_then(normalize_us_state_abbr)
        .or_else(|| val_str(j, "region").and_then(normalize_us_state_abbr))
        .map(str::to_string);
    let city = val_str(j, "city").and_then(sanitize_geo_label);
    let postal_code = if country_code == "US" {
        val_str(j, "postal").and_then(normalize_us_zip5)
    } else {
        None
    };
    let (lat, lon) = parse_coord_pair(val_f64(j, "latitude"), val_f64(j, "longitude"));

    Some(RawUpstreamGeo {
        country_code,
        region_code,
        city,
        postal_code,
        lat,
        lon,
        detection_method: "ipwhois".to_string(),
        attribution: "ipwho.is Keyless IP Geolocation (https://ipwho.is/)".to_string(),
    })
}

/// Parses `https://freeipapi.com/api/json/{ip}` / `https://freeipapi.com/api/json` JSON response.
pub(crate) fn parse_freeipapi_json(j: &Value) -> Option<RawUpstreamGeo> {
    let country_code = val_str(j, "countryCode").and_then(normalize_iso_country)?;
    let region_code = val_str(j, "regionCode")
        .and_then(normalize_us_state_abbr)
        .or_else(|| val_str(j, "regionName").and_then(normalize_us_state_abbr))
        .map(str::to_string);
    let city = val_str(j, "cityName").and_then(sanitize_geo_label);
    let postal_code = if country_code == "US" {
        val_str(j, "zipCode").and_then(normalize_us_zip5)
    } else {
        None
    };
    let (lat, lon) = parse_coord_pair(val_f64(j, "latitude"), val_f64(j, "longitude"));

    Some(RawUpstreamGeo {
        country_code,
        region_code,
        city,
        postal_code,
        lat,
        lon,
        detection_method: "freeipapi".to_string(),
        attribution: "FreeIPAPI Keyless IP Geolocation (https://freeipapi.com/)".to_string(),
    })
}

/// Parses `https://ipapi.co/{ip}/json/` / `https://ipapi.co/json/` JSON response.
pub(crate) fn parse_ipapico_json(j: &Value) -> Option<RawUpstreamGeo> {
    if j.get("error").and_then(Value::as_bool) == Some(true)
        || j.get("reserved").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let country_code = val_str(j, "country_code")
        .or_else(|| val_str(j, "country"))
        .and_then(normalize_iso_country)?;
    let region_code = val_str(j, "region_code")
        .and_then(normalize_us_state_abbr)
        .or_else(|| val_str(j, "region").and_then(normalize_us_state_abbr))
        .map(str::to_string);
    let city = val_str(j, "city").and_then(sanitize_geo_label);
    let postal_code = if country_code == "US" {
        val_str(j, "postal").and_then(normalize_us_zip5)
    } else {
        None
    };
    let (lat, lon) = parse_coord_pair(val_f64(j, "latitude"), val_f64(j, "longitude"));

    Some(RawUpstreamGeo {
        country_code,
        region_code,
        city,
        postal_code,
        lat,
        lon,
        detection_method: "ipapico".to_string(),
        attribution: "ipapi.co Keyless IP Geolocation (https://ipapi.co/)".to_string(),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CensusCoordinateGeo {
    pub zcta5: Option<String>,
    pub city: Option<String>,
    pub state_abbr: Option<String>,
    pub metro: Option<String>,
}

fn find_census_layer<'a>(
    geos: &'a serde_json::Map<String, Value>,
    suffixes: &[&str],
) -> Option<&'a Value> {
    for &sfx in suffixes {
        if let Some((_, arr)) = geos
            .iter()
            .find(|(k, _)| k.as_str() == sfx || k.ends_with(sfx))
        {
            if let Some(first) = arr.as_array().and_then(|a| a.first()) {
                return Some(first);
            }
        }
    }
    None
}

/// Parses U.S. Census Bureau `geocoder/geographies/coordinates` payload into ZCTA5, city,
/// state abbreviation, and CBSA metropolitan/micropolitan area label.
pub(crate) fn parse_census_coordinates_json(j: &Value) -> CensusCoordinateGeo {
    let Some(geos) = j
        .get("result")
        .and_then(|r| r.get("geographies"))
        .and_then(Value::as_object)
    else {
        return CensusCoordinateGeo::default();
    };

    let zcta_obj = find_census_layer(geos, &["ZIP Code Tabulation Areas"]);
    let zcta5 = zcta_obj
        .and_then(|o| val_str(o, "ZCTA5").or_else(|| val_str(o, "GEOID")))
        .and_then(normalize_us_zip5);

    let place_obj = find_census_layer(geos, &["Incorporated Places", "Census Designated Places"]);
    let city = place_obj
        .and_then(|o| {
            val_str(o, "BASENAME").map(str::to_string).or_else(|| {
                val_str(o, "NAME").map(|n| {
                    n.trim_end_matches(" city")
                        .trim_end_matches(" town")
                        .trim_end_matches(" CDP")
                        .trim_end_matches(" village")
                        .trim_end_matches(" borough")
                        .trim()
                        .to_string()
                })
            })
        })
        .and_then(|s| sanitize_geo_label(&s));

    let state_obj = find_census_layer(geos, &["States"]);
    let state_abbr = state_obj
        .and_then(|o| val_str(o, "STUSAB"))
        .and_then(normalize_us_state_abbr)
        .map(str::to_string);

    let msa_obj = find_census_layer(
        geos,
        &[
            "Metropolitan Statistical Areas",
            "Micropolitan Statistical Areas",
        ],
    );
    let metro = msa_obj
        .and_then(|o| val_str(o, "NAME").or_else(|| val_str(o, "BASENAME")))
        .and_then(sanitize_geo_label);

    CensusCoordinateGeo {
        zcta5,
        city,
        state_abbr,
        metro,
    }
}

async fn fetch_json_direct(url: &str, timeout_ms: u64) -> Option<Value> {
    let resp = IP_GEO_HTTP_CLIENT
        .get(url)
        .header(header::ACCEPT, "application/json")
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// Step D: Queries U.S. Census TIGER/Line `geographies/coordinates` to resolve/verify exact
/// 5-digit ZCTA5, city, state, and CBSA metro for a US coordinate pair.
async fn lookup_census_reverse_geographies(lat: f64, lon: f64) -> Option<CensusCoordinateGeo> {
    let mut u = Url::parse(&format!("{CENSUS_GEOCODER_GEOS_BASE}/coordinates")).ok()?;
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("x", &format!("{lon:.6}"));
        qp.append_pair("y", &format!("{lat:.6}"));
        qp.append_pair("benchmark", "Public_AR_Current");
        qp.append_pair("vintage", "Current_Current");
        qp.append_pair("layers", "all");
        qp.append_pair("format", "json");
    }
    let j = fetch_json_direct(u.as_str(), 3_500).await?;
    let parsed = parse_census_coordinates_json(&j);
    if parsed.zcta5.is_none()
        && parsed.city.is_none()
        && parsed.state_abbr.is_none()
        && parsed.metro.is_none()
    {
        return None;
    }
    Some(parsed)
}

/// Step C: Queries keyless IP geolocation services in priority order:
/// 1. `https://ipwho.is/{ip}` (or `https://ipwho.is/` for `SelfEgress`)
/// 2. `https://freeipapi.com/api/json/{ip}` (or `https://freeipapi.com/api/json` for `SelfEgress`)
/// 3. `https://ipapi.co/{ip}/json/` (or `https://ipapi.co/json/` for `SelfEgress`)
async fn query_keyless_ip_upstreams(target: &ClientIpTarget) -> Option<RawUpstreamGeo> {
    let (ipwhois_url, freeipapi_url, ipapico_url) = match target {
        ClientIpTarget::Public(ip) => {
            let ip_s = ip.to_string();
            (
                format!("https://ipwho.is/{ip_s}"),
                format!("https://freeipapi.com/api/json/{ip_s}"),
                format!("https://ipapi.co/{ip_s}/json/"),
            )
        }
        ClientIpTarget::SelfEgress => (
            "https://ipwho.is/".to_string(),
            "https://freeipapi.com/api/json".to_string(),
            "https://ipapi.co/json/".to_string(),
        ),
    };

    if let Some(j) = fetch_json_direct(&ipwhois_url, 3_200).await {
        if let Some(parsed) = parse_ipwhois_json(&j) {
            if parsed.country_code != "US"
                || parsed.postal_code.is_some()
                || (parsed.lat.is_some() && parsed.lon.is_some())
            {
                return Some(parsed);
            }
        }
    }

    if let Some(j) = fetch_json_direct(&freeipapi_url, 3_200).await {
        if let Some(parsed) = parse_freeipapi_json(&j) {
            if parsed.country_code != "US"
                || parsed.postal_code.is_some()
                || (parsed.lat.is_some() && parsed.lon.is_some())
            {
                return Some(parsed);
            }
        }
    }

    if let Some(j) = fetch_json_direct(&ipapico_url, 3_200).await {
        if let Some(parsed) = parse_ipapico_json(&j) {
            return Some(parsed);
        }
    }

    None
}

/// Checks whether the `ip_geolocation` module or extension has been disabled by an operator
/// via environment config, `system_modules`, or `installed_extensions`.
pub async fn is_ip_geolocation_enabled(state: &AppState) -> bool {
    if let Some(flag) = state.env.get("IP_GEOLOCATION_ENABLED") {
        let low = flag.trim().to_ascii_lowercase();
        if matches!(low.as_str(), "0" | "false" | "no" | "off" | "disabled") {
            return false;
        }
    }

    if let Ok(Some(sys_mod)) = state
        .db
        .get_system_module(IP_GEOLOCATION_EXTENSION_ID)
        .await
    {
        if !sys_mod.enabled {
            return false;
        }
    }

    if let Ok(Some(ext_row)) = state
        .db
        .get_installed_extension(IP_GEOLOCATION_EXTENSION_ID)
        .await
    {
        if !ext_row.enabled {
            return false;
        }
    }

    true
}

fn is_external_network_blocked(state: Option<&AppState>) -> bool {
    if let Some(st) = state {
        if st.http.is_offline_blocked() {
            return true;
        }
        if let Some(v) = st.env.get("OFFLINE_BLOCK_EXTERNAL") {
            if matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
                return true;
            }
        }
    }
    std::env::var("OFFLINE_BLOCK_EXTERNAL")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Executes the complete Steps A -> E IP geolocation resolution pipeline.
pub async fn resolve_ip_geo(
    state: Option<&AppState>,
    headers: &HeaderMap,
    peer_ip: Option<IpAddr>,
    middleware_ip: Option<&str>,
) -> IpGeoResolution {
    if let Some(st) = state {
        if !is_ip_geolocation_enabled(st).await {
            return IpGeoResolution::err("ip_geolocation_disabled", None, "disabled", "");
        }
    }

    let (trust_proxy, trust_cf_headers) = state
        .map(|st| (st.shield.trust_proxy(), st.shield.trust_cf_headers()))
        .unwrap_or((false, false));

    // Step A: Zero-latency Edge / Reverse-Proxy Geo Headers
    if let Some(edge_outcome) =
        inspect_edge_geo_headers(headers, peer_ip, trust_proxy, trust_cf_headers)
    {
        match edge_outcome {
            EdgeHeaderOutcome::Resolved(res) => return res,
            EdgeHeaderOutcome::NeedsCensusZcta {
                city,
                region_code,
                lat,
                lon,
                detection_method,
                attribution,
            } => {
                if !is_external_network_blocked(state) {
                    let edge_cache_key = format!("edge_coord:{lat:.3},{lon:.3}");
                    if let Some(mut cached) = IP_GEO_CACHE.get(&edge_cache_key).await {
                        cached.cache_hit = true;
                        return cached;
                    }
                    if let Some(census) = lookup_census_reverse_geographies(lat, lon).await {
                        if let Some(zcta) = census.zcta5 {
                            let resolved = IpGeoResolution::ok(IpGeoLocation {
                                ok: true,
                                postal_code: Some(zcta),
                                city: city.or(census.city),
                                region_code: region_code.or(census.state_abbr),
                                country_code: Some("US".to_string()),
                                lat: Some(lat),
                                lon: Some(lon),
                                metro: census.metro,
                                detection_method,
                                attribution,
                            });
                            IP_GEO_CACHE.insert(edge_cache_key, resolved.clone()).await;
                            return resolved;
                        }
                    }
                }
            }
        }
    }

    // Step B: Real client IP extraction & privacy-preserving subnet cache key
    let target = extract_client_ip_target(
        headers,
        peer_ip,
        middleware_ip,
        trust_proxy,
        trust_cf_headers,
    );
    let cache_key = target.cache_key();

    if let Some(mut cached) = IP_GEO_CACHE.get(&cache_key).await {
        cached.cache_hit = true;
        return cached;
    }

    if is_external_network_blocked(state) {
        return IpGeoResolution::err("offline_mode", None, "offline", "");
    }

    // Step C: Live keyless IP geolocation cascade
    let mode_suffix = match target {
        ClientIpTarget::Public(_) => "client_ip",
        ClientIpTarget::SelfEgress => "self_egress",
    };

    let Some(raw) = query_keyless_ip_upstreams(&target).await else {
        return IpGeoResolution::err("upstream_unreachable", None, mode_suffix, "");
    };

    if raw.country_code != "US" {
        let non_us = IpGeoResolution::err(
            "outside_us",
            Some(raw.country_code),
            format!("{}_{}", raw.detection_method, mode_suffix),
            raw.attribution,
        );
        IP_GEO_CACHE.insert(cache_key, non_us.clone()).await;
        return non_us;
    }

    // Step D: Resolve / verify exact 5-digit Census ZCTA5, city, state, and CBSA metro from US lat/lon
    let mut postal_code = raw.postal_code;
    let mut city = raw.city;
    let mut region_code = raw.region_code;
    let mut metro: Option<String> = None;
    let mut detection_method = format!("{}_{}", raw.detection_method, mode_suffix);
    let mut attribution = raw.attribution;

    if let (Some(lat), Some(lon)) = (raw.lat, raw.lon) {
        if is_us_coordinate_bounds(lat, lon) {
            if let Some(census) = lookup_census_reverse_geographies(lat, lon).await {
                if let Some(zcta) = census.zcta5 {
                    if postal_code.as_deref() != Some(zcta.as_str()) {
                        detection_method = format!("{detection_method}+census_zcta");
                        attribution =
                            format!("{attribution} + U.S. Census Bureau TIGER Geographies");
                    }
                    postal_code = Some(zcta);
                }
                if city.is_none() {
                    city = census.city;
                }
                if region_code.is_none() {
                    region_code = census.state_abbr;
                }
                metro = census.metro;
            }
        }
    }

    // Step E: Strict Zero-Mock Rule — never invent a fallback ZIP code
    let Some(verified_zip) = postal_code else {
        return IpGeoResolution::err(
            "postal_code_unresolved",
            Some("US".to_string()),
            detection_method,
            attribution,
        );
    };

    let resolved = IpGeoResolution::ok(IpGeoLocation {
        ok: true,
        postal_code: Some(verified_zip),
        city,
        region_code,
        country_code: Some("US".to_string()),
        lat: raw.lat,
        lon: raw.lon,
        metro,
        detection_method,
        attribution,
    });

    IP_GEO_CACHE.insert(cache_key, resolved.clone()).await;
    resolved
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct IpGeoQuery {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

async fn resolve_coordinate_reverse_zcta(
    state: &AppState,
    lat: f64,
    lon: f64,
) -> Option<IpGeoResolution> {
    if !lat.is_finite() || !lon.is_finite() || !(17.5..=72.0).contains(&lat) || !(-179.9..=-64.0).contains(&lon) {
        return Some(IpGeoResolution::err(
            "outside_us",
            None,
            "browser_gps_coordinates",
            "U.S. Census TIGER/Line Geographies",
        ));
    }
    if is_external_network_blocked(Some(state)) {
        return None;
    }
    let cache_key = format!("gps_coord:{lat:.3},{lon:.3}");
    if let Some(mut cached) = IP_GEO_CACHE.get(&cache_key).await {
        cached.cache_hit = true;
        return Some(cached);
    }
    let census = lookup_census_reverse_geographies(lat, lon).await?;
    let zcta = census.zcta5?;
    let resolved = IpGeoResolution::ok(IpGeoLocation {
        ok: true,
        postal_code: Some(zcta),
        city: census.city,
        region_code: census.state_abbr,
        country_code: Some("US".to_string()),
        lat: Some(lat),
        lon: Some(lon),
        metro: census.metro,
        detection_method: "browser_gps_census_zcta".to_string(),
        attribution: "U.S. Census TIGER/Line ZCTA5 Geographies".to_string(),
    });
    IP_GEO_CACHE.insert(cache_key, resolved.clone()).await;
    Some(resolved)
}

/// Axum handler for `GET /api/geo/ip`.
pub async fn handle_ip_geo(
    State(state): State<AppState>,
    Query(q): Query<IpGeoQuery>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    let resolution = if let (Some(lat), Some(lon)) = (q.lat, q.lon) {
        if !is_ip_geolocation_enabled(&state).await {
            IpGeoResolution::err("ip_geolocation_disabled", None, "disabled", "")
        } else if let Some(coord_res) = resolve_coordinate_reverse_zcta(&state, lat, lon).await {
            coord_res
        } else {
            let peer_ip = extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip());
            let middleware_ip = extensions.get::<ClientIp>().map(|ci| ci.0.as_str());
            resolve_ip_geo(Some(&state), &headers, peer_ip, middleware_ip).await
        }
    } else {
        let peer_ip = extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());
        let middleware_ip = extensions.get::<ClientIp>().map(|ci| ci.0.as_str());
        resolve_ip_geo(Some(&state), &headers, peer_ip, middleware_ip).await
    };
    let body = resolution.to_api_json();

    let mut resp = (StatusCode::OK, Json(body)).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-openestate-cache"),
        HeaderValue::from_static(if resolution.cache_hit { "HIT" } else { "MISS" }),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn public_vs_private_ip_classification_and_prefix_keys() {
        assert!(!is_public_routable_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_routable_ip("::1".parse().unwrap()));
        assert!(!is_public_routable_ip("10.12.34.56".parse().unwrap()));
        assert!(!is_public_routable_ip("192.168.1.42".parse().unwrap()));
        assert!(!is_public_routable_ip("172.16.0.1".parse().unwrap()));
        assert!(!is_public_routable_ip("172.31.255.254".parse().unwrap()));
        assert!(!is_public_routable_ip("100.64.10.20".parse().unwrap()));
        assert!(!is_public_routable_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_public_routable_ip("fe80::1".parse().unwrap()));
        assert!(!is_public_routable_ip("fd00::1234".parse().unwrap()));

        let pub_v4: IpAddr = "8.8.8.8".parse().unwrap();
        let pub_172: IpAddr = "172.32.10.5".parse().unwrap();
        let pub_v6: IpAddr = "2606:4700:4700::1111".parse().unwrap();
        assert!(is_public_routable_ip(pub_v4));
        assert!(is_public_routable_ip(pub_172));
        assert!(is_public_routable_ip(pub_v6));

        assert_eq!(ip_prefix_cache_key(pub_v4), "v4:8.8.8.0/24");
        assert_eq!(ip_prefix_cache_key(pub_v6), "v6:2606:4700:4700::/48");
    }

    #[test]
    fn xff_extracts_first_public_ip_and_loopback_falls_back_to_self_egress() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("10.0.0.5, 192.168.1.9, 8.8.4.4, 1.1.1.1"),
        );
        assert_eq!(
            extract_client_ip_target(&h, Some("127.0.0.1".parse().unwrap()), None, true, false),
            ClientIpTarget::Public("8.8.4.4".parse().unwrap())
        );
        // Untrusted proxy / default trust_proxy=false ignores forwarded headers
        assert_eq!(
            extract_client_ip_target(&h, Some("127.0.0.1".parse().unwrap()), None, false, false),
            ClientIpTarget::SelfEgress
        );
        assert_eq!(
            extract_client_ip_target(&h, Some("8.8.8.8".parse().unwrap()), None, true, true),
            ClientIpTarget::Public("8.8.8.8".parse().unwrap())
        );

        let empty = HeaderMap::new();
        assert_eq!(
            extract_client_ip_target(&empty, Some("127.0.0.1".parse().unwrap()), None, true, false),
            ClientIpTarget::SelfEgress
        );
        assert_eq!(
            extract_client_ip_target(
                &empty,
                Some("192.168.0.15".parse().unwrap()),
                None,
                true,
                false
            ),
            ClientIpTarget::SelfEgress
        );
    }

    #[test]
    fn edge_headers_resolve_cloudflare_and_vercel_and_reject_non_us() {
        let trusted_proxy = Some("127.0.0.1".parse().unwrap());
        let untrusted_peer = Some("198.51.100.77".parse().unwrap());

        let mut cf = HeaderMap::new();
        cf.insert("cf-ipcountry", HeaderValue::from_static("US"));
        cf.insert("cf-ipcity", HeaderValue::from_static("Mountain View"));
        cf.insert("cf-region-code", HeaderValue::from_static("CA"));
        cf.insert("cf-postal-code", HeaderValue::from_static("94043"));
        cf.insert("cf-iplatitude", HeaderValue::from_static("37.38605"));
        cf.insert("cf-iplongitude", HeaderValue::from_static("-122.08385"));

        // Spoofed CF headers ignored when trust_proxy=false, peer untrusted, or trust_cf_headers=false
        assert!(inspect_edge_geo_headers(&cf, trusted_proxy, false, true).is_none());
        assert!(inspect_edge_geo_headers(&cf, untrusted_peer, true, true).is_none());
        assert!(inspect_edge_geo_headers(&cf, trusted_proxy, true, false).is_none());

        let Some(EdgeHeaderOutcome::Resolved(res)) =
            inspect_edge_geo_headers(&cf, trusted_proxy, true, true)
        else {
            panic!("expected resolved Cloudflare edge outcome");
        };
        assert!(res.location.ok);
        assert_eq!(res.location.postal_code.as_deref(), Some("94043"));
        assert_eq!(res.location.city.as_deref(), Some("Mountain View"));
        assert_eq!(res.location.region_code.as_deref(), Some("CA"));
        let api_json = res.to_api_json();
        assert_eq!(api_json["postalCode"], "94043");
        assert_eq!(api_json["regionCode"], "CA");
        assert!(api_json["source"].as_str().is_some());

        let mut vercel = HeaderMap::new();
        vercel.insert("x-vercel-ip-country", HeaderValue::from_static("US"));
        vercel.insert(
            "x-vercel-ip-city",
            HeaderValue::from_static("San%20Francisco"),
        );
        vercel.insert("x-vercel-ip-country-region", HeaderValue::from_static("CA"));
        vercel.insert(
            "x-vercel-ip-postal-code",
            HeaderValue::from_static("94107-1234"),
        );
        let Some(EdgeHeaderOutcome::Resolved(v_res)) =
            inspect_edge_geo_headers(&vercel, trusted_proxy, true, false)
        else {
            panic!("expected resolved Vercel edge outcome");
        };
        assert_eq!(v_res.location.city.as_deref(), Some("San Francisco"));
        assert_eq!(v_res.location.postal_code.as_deref(), Some("94107"));

        let mut non_us = HeaderMap::new();
        non_us.insert("cf-ipcountry", HeaderValue::from_static("DE"));
        let Some(EdgeHeaderOutcome::Resolved(de_res)) =
            inspect_edge_geo_headers(&non_us, trusted_proxy, true, true)
        else {
            panic!("expected non-US Cloudflare outcome");
        };
        assert!(!de_res.location.ok);
        assert_eq!(de_res.reason.as_deref(), Some("outside_us"));
        assert_eq!(de_res.location.country_code.as_deref(), Some("DE"));
        assert!(de_res.location.postal_code.is_none());
    }

    #[test]
    fn upstream_and_census_parsers_handle_real_schemas() {
        let ipwhois = json!({
            "ip": "8.8.8.8",
            "success": true,
            "country_code": "US",
            "region": "California",
            "region_code": "CA",
            "city": "Mountain View",
            "latitude": 37.38605,
            "longitude": -122.08385,
            "postal": "94043"
        });
        let p1 = parse_ipwhois_json(&ipwhois).expect("ipwhois parse");
        assert_eq!(p1.postal_code.as_deref(), Some("94043"));
        assert_eq!(p1.region_code.as_deref(), Some("CA"));

        let freeip = json!({
            "countryCode": "US",
            "regionName": "California",
            "cityName": "Mountain View",
            "zipCode": "94043",
            "latitude": 37.422,
            "longitude": -122.085
        });
        let p2 = parse_freeipapi_json(&freeip).expect("freeipapi parse");
        assert_eq!(p2.region_code.as_deref(), Some("CA"));
        assert_eq!(p2.postal_code.as_deref(), Some("94043"));

        let census = json!({
            "result": {
                "geographies": {
                    "2020 Census ZIP Code Tabulation Areas": [{"ZCTA5": "95110", "GEOID": "95110"}],
                    "Incorporated Places": [{"BASENAME": "San Jose", "NAME": "San Jose city"}],
                    "States": [{"STUSAB": "CA"}],
                    "Metropolitan Statistical Areas": [{"NAME": "San Jose-Sunnyvale-Santa Clara, CA Metro Area"}]
                }
            }
        });
        let c = parse_census_coordinates_json(&census);
        assert_eq!(c.zcta5.as_deref(), Some("95110"));
        assert_eq!(c.city.as_deref(), Some("San Jose"));
        assert_eq!(c.state_abbr.as_deref(), Some("CA"));
        assert_eq!(
            c.metro.as_deref(),
            Some("San Jose-Sunnyvale-Santa Clara, CA Metro Area")
        );
    }
}
