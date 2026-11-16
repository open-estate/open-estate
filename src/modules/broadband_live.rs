//! Live address-level broadband verification (`broadband_live`).
//!
//! Performs real upstream verification rather than inferring availability from
//! county/state/tract assumptions:
//! 1. **Sonic Official Availability Verification (`www.sonic.com`)**:
//!    Executes Sonic's stateful CSRF + session-cookie workflow
//!    (`GET /availability` -> `POST /availability/verify_address/` ->
//!    `POST /availability` -> explicit redirect follow to `/availability/qual` or
//!    `/availability/no_products`).
//! 2. **Honest FCC National Broadband Map (BDC) Fixed Lookup**:
//!    Queries `listAvailableFixedProvidersByLocation` with optional
//!    `FCC_BDC_USERNAME` + `FCC_BDC_API_TOKEN` (`username` / `hash_value` headers)
//!    and reports explicit `unavailable` / `unconfigured` status whenever FCC
//!    returns HTTP 401/405, times out, or offline test mode blocks egress.

use crate::extensions::adapter::{BoxFuture, DataExtensionAdapter};
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, BroadbandOptionProto, CapabilityDomain,
    DriverKind, ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, GeoScopeProto,
    ProvenanceTier, RateLimitSpecProto, SourceAttributionProto,
};
use crate::shared::{now_iso, EnvConfig, HttpClient};
use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{COOKIE, LOCATION, SET_COOKIE};
use reqwest::redirect::Policy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use url::Url;

pub const SONIC_DEFAULT_BASE_URL: &str = "https://www.sonic.com";
pub const SONIC_CHECK_PAGE_URL: &str = "https://www.sonic.com/availability";
pub const SONIC_CACHE_TTL_SECS: i64 = 24 * 60 * 60;

pub const FCC_BDC_FIXED_DEFAULT_URL: &str =
    "https://broadbandmap.fcc.gov/api/public/map/listAvailableFixedProvidersByLocation";
pub const FCC_BDC_MAP_HOME_URL: &str = "https://broadbandmap.fcc.gov/";
pub const FCC_CACHE_TTL_SECS: i64 = 6 * 60 * 60;

static RE_CSRF_INPUT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?is)<input\b[^>]*(?:id|name)=["']csrf_token["'][^>]*\bvalue=["']([^"']+)["']|<input\b[^>]*\bvalue=["']([^"']+)["'][^>]*(?:id|name)=["']csrf_token["']"#,
    )
    .expect("sonic csrf input regex")
});

static RE_GBPS_PLAN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(10(?:\.0)?|5(?:\.0)?|2\.5|1(?:\.0)?)\s*gbps\s+fiber(?:\s+internet)?\b")
        .expect("sonic gbps plan regex")
});

static RE_NUTRITION_DOWN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)id=["']nutritional_label_download["'][^>]*>\s*([0-9,]+)\s*mbps"#)
        .expect("sonic nutrition label download regex")
});

static RE_PRICE_MO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\$(\d+(?:\.\d{2})?)\s*/\s*mo").expect("sonic monthly price regex")
});

static RE_DATA_PRODUCT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)\bdata-product=["']([a-z0-9_-]+)["']"#).expect("sonic data-product regex")
});

static RE_HTML_TAGS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)<[^>]+>").expect("html strip regex"));

static SONIC_HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_millis(4_500))
        .timeout(Duration::from_millis(7_000))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .unwrap_or_default()
});

/// Canonical address-level verification state from `www.sonic.com/availability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SonicAvailabilityStatus {
    Available,
    Preorder,
    NotAvailable,
    Unverified,
}

impl SonicAvailabilityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Preorder => "preorder",
            Self::NotAvailable => "not_available",
            Self::Unverified => "unverified",
        }
    }
}

/// Specific plan offer extracted from a qualified `/availability/qual` page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SonicPlanOffer {
    pub product_code: Option<String>,
    pub name: String,
    pub max_download_mbps: Option<i64>,
    pub max_upload_mbps: Option<i64>,
    pub monthly_price_usd: Option<String>,
}

/// Input parameters for live Sonic address verification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SonicAddressInput {
    pub street: String,
    pub city: String,
    pub state: String,
    pub zip: String,
    pub unit: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

impl SonicAddressInput {
    pub fn new(
        street: impl Into<String>,
        city: impl Into<String>,
        state: impl Into<String>,
        zip: impl Into<String>,
    ) -> Self {
        Self {
            street: street.into(),
            city: city.into(),
            state: state.into(),
            zip: zip.into(),
            unit: None,
            lat: None,
            lon: None,
        }
    }
}

/// Result of live Sonic address verification. `available` is strictly `false`
/// unless `www.sonic.com/availability/qual` explicitly confirmed deliverable service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SonicAvailabilityResult {
    pub status: SonicAvailabilityStatus,
    pub available: bool,
    pub preorder: bool,
    pub carrier_name: String,
    pub status_label: String,
    pub final_url: Option<String>,
    pub verified_address: Option<String>,
    pub verified_lat: Option<f64>,
    pub verified_lon: Option<f64>,
    pub plans: Vec<SonicPlanOffer>,
    pub max_download_mbps: Option<i64>,
    pub max_upload_mbps: Option<i64>,
    pub monthly_price_usd: Option<String>,
    pub check_url: String,
    pub source_name: String,
    pub provenance: String,
    pub checked_at: String,
    pub cached: bool,
    pub reason: Option<String>,
}

impl SonicAvailabilityResult {
    pub fn unverified(reason: impl Into<String>) -> Self {
        Self {
            status: SonicAvailabilityStatus::Unverified,
            available: false,
            preorder: false,
            carrier_name: "Sonic Fiber".to_string(),
            status_label: "Sonic availability unverified (check address on sonic.com)".to_string(),
            final_url: None,
            verified_address: None,
            verified_lat: None,
            verified_lon: None,
            plans: Vec::new(),
            max_download_mbps: None,
            max_upload_mbps: None,
            monthly_price_usd: None,
            check_url: SONIC_CHECK_PAGE_URL.to_string(),
            source_name: "Sonic Official Availability Checker".to_string(),
            provenance: "sonic_live_unverified".to_string(),
            checked_at: now_iso(),
            cached: false,
            reason: Some(reason.into()),
        }
    }

    pub fn is_available(&self) -> bool {
        self.available && self.status == SonicAvailabilityStatus::Available
    }

    /// Converts a confirmed `Available` result into an extension `BroadbandOptionProto`.
    /// Returns `None` for `NotAvailable`, `Preorder`, and `Unverified` so unavailable
    /// addresses are never injected as serviceable fiber carriers.
    pub fn into_broadband_option(&self) -> Option<BroadbandOptionProto> {
        if !self.is_available() {
            return None;
        }
        let dl = self.max_download_mbps.unwrap_or(10_000);
        let ul = self.max_upload_mbps.unwrap_or(dl);
        let monthly = self
            .monthly_price_usd
            .clone()
            .unwrap_or_else(|| "Verified on sonic.com".to_string());
        Some(BroadbandOptionProto {
            carrier_name: self.carrier_name.clone(),
            technology: "Fiber (XGS-PON / FTTH)".to_string(),
            tier_category: "fiber".to_string(),
            max_download_mbps: dl,
            max_upload_mbps: ul,
            symmetrical: true,
            typical_monthly_usd: monthly,
            availability_status: "confirmed_address".to_string(),
            check_url: self
                .final_url
                .clone()
                .unwrap_or_else(|| self.check_url.clone()),
        })
    }
}

/// Honest status for FCC National Broadband Map (BDC) fixed queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FccLookupStatus {
    LiveVerified,
    EmptyResult,
    Unavailable,
    Unconfigured,
}

impl FccLookupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveVerified => "live_verified",
            Self::EmptyResult => "empty_result",
            Self::Unavailable => "unavailable",
            Self::Unconfigured => "unconfigured",
        }
    }
}

/// Single fixed provider filing parsed from FCC BDC JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccProviderRecord {
    pub name: String,
    pub holding_company: Option<String>,
    pub technology_code: Option<i64>,
    pub technology: String,
    pub category: String,
    pub max_download_mbps: i64,
    pub max_upload_mbps: i64,
    pub low_latency: Option<bool>,
    pub business_residential_code: Option<String>,
}

impl FccProviderRecord {
    pub fn to_broadband_option(&self, fcc_map_url: &str) -> BroadbandOptionProto {
        BroadbandOptionProto {
            carrier_name: self.name.clone(),
            technology: self.technology.clone(),
            tier_category: self.category.clone(),
            max_download_mbps: self.max_download_mbps,
            max_upload_mbps: self.max_upload_mbps,
            symmetrical: self.category == "fiber"
                || (self.max_download_mbps > 0 && self.max_upload_mbps >= self.max_download_mbps),
            typical_monthly_usd: String::new(),
            availability_status: "confirmed_fcc".to_string(),
            check_url: fcc_map_url.to_string(),
        }
    }
}

/// Honest FCC BDC fixed lookup result (`unavailable` on HTTP 401/405/offline).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccFixedBroadbandResult {
    pub status: FccLookupStatus,
    pub queried: bool,
    pub has_credentials: bool,
    pub http_status: Option<u16>,
    pub providers: Vec<FccProviderRecord>,
    pub fiber_available: bool,
    pub cable_available: bool,
    pub max_download_mbps: Option<i64>,
    pub max_upload_mbps: Option<i64>,
    pub source_name: String,
    pub source_url: String,
    pub provenance: String,
    pub checked_at: String,
    pub cached: bool,
    pub reason: Option<String>,
}

impl FccFixedBroadbandResult {
    fn unavailable(
        source_url: String,
        queried: bool,
        has_credentials: bool,
        http_status: Option<u16>,
        status: FccLookupStatus,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status,
            queried,
            has_credentials,
            http_status,
            providers: Vec::new(),
            fiber_available: false,
            cable_available: false,
            max_download_mbps: None,
            max_upload_mbps: None,
            source_name: "FCC National Broadband Map (BDC)".to_string(),
            source_url,
            provenance: match status {
                FccLookupStatus::Unconfigured => "fcc_bdc_unconfigured",
                _ => "fcc_bdc_unavailable",
            }
            .to_string(),
            checked_at: now_iso(),
            cached: false,
            reason: Some(reason.into()),
        }
    }
}

/// Combined live broadband audit returned by [`check_live_broadband`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveBroadbandCheck {
    pub sonic: SonicAvailabilityResult,
    pub fcc_fixed: FccFixedBroadbandResult,
}

fn is_loopback_host(host: &str) -> bool {
    let lc = host.trim().to_ascii_lowercase();
    lc == "localhost" || lc == "127.0.0.1" || lc == "::1" || lc == "[::1]"
}

fn validate_sonic_base_url(raw: &str) -> Option<Url> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = Url::parse(trimmed).ok()?;
    let host = url.host_str()?;
    match url.scheme() {
        "https" => Some(url),
        "http" if is_loopback_host(host) => Some(url),
        _ => None,
    }
}

fn extract_set_cookies(headers: &reqwest::header::HeaderMap, jar: &mut BTreeMap<String, String>) {
    for val in headers.get_all(SET_COOKIE) {
        let Ok(raw) = val.to_str() else {
            continue;
        };
        let Some(first_pair) = raw.split(';').next() else {
            continue;
        };
        let Some((k, v)) = first_pair.trim().split_once('=') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim();
        if key.is_empty() {
            continue;
        }
        if val.is_empty() {
            jar.remove(key);
        } else {
            jar.insert(key.to_string(), val.to_string());
        }
    }
}

fn format_cookie_header(jar: &BTreeMap<String, String>) -> Option<String> {
    if jar.is_empty() {
        return None;
    }
    Some(
        jar.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Extracts `csrf_token` from Sonic's `/availability` HTML form.
pub fn extract_sonic_csrf_token(html: &str) -> Option<String> {
    let caps = RE_CSRF_INPUT.captures(html)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn origin_string(url: &Url) -> String {
    match url.port() {
        Some(p) => format!("{}://{}:{}", url.scheme(), url.host_str().unwrap_or(""), p),
        None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")),
    }
}

fn same_sonic_origin_or_subdomain(base: &Url, target: &Url) -> bool {
    let Some(bh) = base.host_str().map(|s| s.to_ascii_lowercase()) else {
        return false;
    };
    let Some(th) = target.host_str().map(|s| s.to_ascii_lowercase()) else {
        return false;
    };
    if bh == th && base.port_or_known_default() == target.port_or_known_default() {
        return target.scheme() == base.scheme();
    }
    target.scheme() == "https" && (th == "sonic.com" || th.ends_with(".sonic.com"))
}

fn json_f64(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|n| n.is_finite())
}

fn json_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn extract_plans_from_html(html: &str, text_lc: &str) -> Vec<SonicPlanOffer> {
    let mut plans = Vec::new();
    let product_code = RE_DATA_PRODUCT
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());
    let price = RE_PRICE_MO
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| format!("${}/mo", m.as_str()));
    let nutrition_down_mbps = RE_NUTRITION_DOWN.captures(html).and_then(|c| {
        c.get(1)
            .map(|m| m.as_str().replace(',', ""))
            .and_then(|d| d.parse::<i64>().ok())
    });

    if let Some(cap) = RE_GBPS_PLAN.captures(text_lc) {
        let gbps = cap
            .get(1)
            .and_then(|m| m.as_str().parse::<f64>().ok())
            .unwrap_or(10.0);
        let headline_mbps = (gbps * 1000.0).round() as i64;
        let name = if (gbps - 10.0).abs() < 0.01 {
            "10 Gbps Fiber Internet".to_string()
        } else {
            format!("{gbps} Gbps Fiber Internet")
        };
        plans.push(SonicPlanOffer {
            product_code,
            name,
            max_download_mbps: Some(nutrition_down_mbps.unwrap_or(headline_mbps).max(headline_mbps)),
            max_upload_mbps: Some(nutrition_down_mbps.unwrap_or(headline_mbps).max(headline_mbps)),
            monthly_price_usd: price,
        });
    } else if text_lc.contains("xgspon") || text_lc.contains("fiber internet is available") {
        let dl = nutrition_down_mbps.unwrap_or(10_000);
        plans.push(SonicPlanOffer {
            product_code,
            name: "Sonic Fiber Internet".to_string(),
            max_download_mbps: Some(dl),
            max_upload_mbps: Some(dl),
            monthly_price_usd: price,
        });
    }
    plans
}

/// Classifies the final Sonic `/availability/qual` or `/availability/no_products` response.
pub fn parse_sonic_qual_html(
    final_url: &str,
    html: &str,
) -> (SonicAvailabilityStatus, Vec<SonicPlanOffer>, String) {
    let path_lc = Url::parse(final_url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_else(|_| final_url.to_ascii_lowercase());
    let plain = RE_HTML_TAGS.replace_all(html, " ");
    let text_lc = plain.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();

    if path_lc.contains("no_products")
        || text_lc.contains("not available yet at your address")
        || text_lc.contains("service is not available")
        || text_lc.contains("no products are currently available")
    {
        return (
            SonicAvailabilityStatus::NotAvailable,
            Vec::new(),
            "Not available at address (verified on sonic.com)".to_string(),
        );
    }

    let is_positive = text_lc.contains("fiber internet is available at your address")
        || text_lc.contains("sonic fiber internet is available")
        || (text_lc.contains("congratulations") && text_lc.contains("fiber internet"))
        || (path_lc.ends_with("/qual") && RE_DATA_PRODUCT.is_match(html) && RE_GBPS_PLAN.is_match(&text_lc));

    if is_positive {
        let plans = extract_plans_from_html(html, &text_lc);
        let top_name = plans
            .first()
            .map(|p| p.name.as_str())
            .unwrap_or("Sonic Fiber");
        let label = format!("Available at address ({top_name})");
        return (SonicAvailabilityStatus::Available, plans, label);
    }

    if text_lc.contains("pre-order")
        || text_lc.contains("preorder")
        || text_lc.contains("coming soon to your")
        || text_lc.contains("under construction")
        || text_lc.contains("reserve your spot")
    {
        return (
            SonicAvailabilityStatus::Preorder,
            Vec::new(),
            "Pre-order / construction area on sonic.com (service not yet active)".to_string(),
        );
    }

    (
        SonicAvailabilityStatus::Unverified,
        Vec::new(),
        "Sonic availability response did not match a known qualified or unavailable state"
            .to_string(),
    )
}

/// Checks address-level fiber availability directly against Sonic's official availability
/// workflow (`GET /availability` -> `POST /availability/verify_address/` ->
/// `POST /availability` -> redirect loop to `/availability/qual` or `/no_products`).
pub async fn check_sonic_availability(
    http: &HttpClient,
    env: &EnvConfig,
    input: &SonicAddressInput,
) -> SonicAvailabilityResult {
    let street = input.street.trim();
    let city = input.city.trim();
    let state = input.state.trim();
    let zip = input.zip.trim();
    let unit = input.unit.as_deref().unwrap_or("").trim();

    if street.is_empty() || (zip.is_empty() && (city.is_empty() || state.is_empty())) {
        return SonicAvailabilityResult::unverified("missing_street_or_location");
    }

    if env.get("SONIC_AVAILABILITY_DISABLED") == Some("1") {
        return SonicAvailabilityResult::unverified("sonic_check_disabled");
    }

    let raw_base = env
        .get("SONIC_AVAILABILITY_BASE_URL")
        .unwrap_or(SONIC_DEFAULT_BASE_URL);
    let Some(base_url) = validate_sonic_base_url(raw_base) else {
        return SonicAvailabilityResult::unverified("invalid_sonic_base_url");
    };
    let avail_url = match base_url.join("/availability") {
        Ok(u) => u,
        Err(_) => return SonicAvailabilityResult::unverified("invalid_sonic_base_url"),
    };

    if http.direct_client_for_url(avail_url.as_str()).is_err() {
        return SonicAvailabilityResult::unverified("offline_mode_blocked");
    }

    let base_tag = base_url.as_str().trim_end_matches('/');
    let cache_key = format!(
        "sonic_avail:v1:{}:{}:{}:{}:{}:{}",
        base_tag,
        street.to_ascii_lowercase(),
        city.to_ascii_lowercase(),
        state.to_ascii_uppercase(),
        zip,
        unit.to_ascii_lowercase()
    );
    if !http.is_bypassing_cache_reads() {
        if let Some(cached_json) = http.read_cache(&cache_key, SONIC_CACHE_TTL_SECS).await {
            if let Ok(mut parsed) = serde_json::from_str::<SonicAvailabilityResult>(&cached_json) {
                parsed.cached = true;
                return parsed;
            }
        }
    }

    let timeout_ms = env
        .get("SONIC_TIMEOUT_MS")
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n.clamp(500, 30_000))
        .unwrap_or(9_500);

    let res = match tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        run_sonic_http_workflow(&base_url, input),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(reason)) => SonicAvailabilityResult::unverified(reason),
        Err(_) => SonicAvailabilityResult::unverified("sonic_lookup_timeout"),
    };

    if matches!(
        res.status,
        SonicAvailabilityStatus::Available
            | SonicAvailabilityStatus::Preorder
            | SonicAvailabilityStatus::NotAvailable
    ) {
        if let Ok(encoded) = serde_json::to_string(&res) {
            http.write_cache(&cache_key, &encoded, SONIC_CACHE_TTL_SECS)
                .await;
        }
    }

    res
}

async fn run_sonic_http_workflow(
    base_url: &Url,
    input: &SonicAddressInput,
) -> std::result::Result<SonicAvailabilityResult, String> {
    let client = &*SONIC_HTTP_CLIENT;
    let avail_url = base_url
        .join("/availability")
        .map_err(|e| format!("invalid_url:{e}"))?;
    let verify_url = base_url
        .join("/availability/verify_address/")
        .map_err(|e| format!("invalid_url:{e}"))?;
    let origin = origin_string(base_url);
    let mut jar = BTreeMap::new();

    // 1. GET /availability to obtain session cookie + CSRF token
    let step1 = client
        .get(avail_url.clone())
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| format!("sonic_get_failed:{e}"))?;
    if !step1.status().is_success() {
        return Err(format!("sonic_get_http_{}", step1.status().as_u16()));
    }
    extract_set_cookies(step1.headers(), &mut jar);
    let page_html = step1
        .text()
        .await
        .map_err(|e| format!("sonic_get_body_failed:{e}"))?;
    let csrf = extract_sonic_csrf_token(&page_html)
        .ok_or_else(|| "missing_sonic_csrf_token".to_string())?;

    // 2. POST /availability/verify_address/
    let unit = input.unit.as_deref().unwrap_or("").trim();
    let verify_body = json!({
        "street": input.street.trim(),
        "city": input.city.trim(),
        "state": input.state.trim(),
        "unit": unit,
        "zipcode": input.zip.trim(),
        "address_type": "residential",
    });
    let mut req2 = client
        .post(verify_url)
        .header("X-CSRFToken", &csrf)
        .header("Referer", avail_url.as_str())
        .header("Origin", &origin)
        .header("Accept", "application/json, */*")
        .json(&verify_body);
    if let Some(ch) = format_cookie_header(&jar) {
        req2 = req2.header(COOKIE, ch);
    }
    let step2 = req2
        .send()
        .await
        .map_err(|e| format!("sonic_verify_failed:{e}"))?;
    if !step2.status().is_success() {
        return Err(format!("sonic_verify_http_{}", step2.status().as_u16()));
    }
    extract_set_cookies(step2.headers(), &mut jar);
    let verify_json: Value = step2
        .json()
        .await
        .map_err(|e| format!("sonic_verify_json_failed:{e}"))?;
    if verify_json.get("success").and_then(Value::as_bool) == Some(false) {
        return Err("sonic_address_not_verified".to_string());
    }
    let va = verify_json.get("verified_address").unwrap_or(&Value::Null);
    let v_street = json_str(va.get("address").or_else(|| va.get("street")))
        .unwrap_or(input.street.trim())
        .to_string();
    let v_city = json_str(va.get("city"))
        .unwrap_or(input.city.trim())
        .to_string();
    let v_state = json_str(va.get("state"))
        .unwrap_or(input.state.trim())
        .to_string();
    let v_zip = json_str(va.get("zip").or_else(|| va.get("zipcode")))
        .unwrap_or(input.zip.trim())
        .to_string();
    let v_unit = json_str(va.get("unit")).unwrap_or(unit).to_string();
    let v_lat = json_f64(va.get("latitude")).or(input.lat);
    let v_lon = json_f64(va.get("longitude")).or(input.lon);

    let lat_str = v_lat.map(|n| format!("{n:.6}")).unwrap_or_default();
    let lon_str = v_lon.map(|n| format!("{n:.6}")).unwrap_or_default();

    // 3. POST /availability (form submit; follow redirect hops manually keeping jar)
    let form_fields = [
        ("csrf_token", csrf.as_str()),
        ("street", v_street.as_str()),
        ("city", v_city.as_str()),
        ("state", v_state.as_str()),
        ("zipcode", v_zip.as_str()),
        ("unit", v_unit.as_str()),
        ("latitude", lat_str.as_str()),
        ("longitude", lon_str.as_str()),
        ("address_type", "residential"),
    ];
    let mut req3 = client
        .post(avail_url.clone())
        .header("Referer", avail_url.as_str())
        .header("Origin", &origin)
        .header("Accept", "text/html,application/xhtml+xml")
        .form(&form_fields);
    if let Some(ch) = format_cookie_header(&jar) {
        req3 = req3.header(COOKIE, ch);
    }
    let mut resp = req3
        .send()
        .await
        .map_err(|e| format!("sonic_submit_failed:{e}"))?;
    extract_set_cookies(resp.headers(), &mut jar);

    let mut current_url = avail_url.clone();
    for _hop in 0..5 {
        if !resp.status().is_redirection() {
            break;
        }
        let Some(loc) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()) else {
            break;
        };
        let next_url = current_url
            .join(loc)
            .map_err(|e| format!("sonic_redirect_invalid:{e}"))?;
        if !same_sonic_origin_or_subdomain(base_url, &next_url) {
            return Err("sonic_redirect_external_blocked".to_string());
        }
        let mut hop_req = client
            .get(next_url.clone())
            .header("Referer", current_url.as_str())
            .header("Accept", "text/html,application/xhtml+xml");
        if let Some(ch) = format_cookie_header(&jar) {
            hop_req = hop_req.header(COOKIE, ch);
        }
        resp = hop_req
            .send()
            .await
            .map_err(|e| format!("sonic_redirect_fetch_failed:{e}"))?;
        extract_set_cookies(resp.headers(), &mut jar);
        current_url = next_url;
    }

    if !resp.status().is_success() {
        return Err(format!("sonic_qual_http_{}", resp.status().as_u16()));
    }
    let final_url = current_url.to_string();
    let final_html = resp
        .text()
        .await
        .map_err(|e| format!("sonic_qual_body_failed:{e}"))?;

    let (status, plans, status_label) = parse_sonic_qual_html(&final_url, &final_html);
    let max_download_mbps = plans.iter().filter_map(|p| p.max_download_mbps).max();
    let max_upload_mbps = plans.iter().filter_map(|p| p.max_upload_mbps).max();
    let monthly_price_usd = plans.first().and_then(|p| p.monthly_price_usd.clone());
    let carrier_name = match max_download_mbps {
        Some(dl) if dl >= 10_000 => "Sonic 10G Fiber".to_string(),
        _ => "Sonic Fiber".to_string(),
    };
    let verified_addr_fmt = if v_unit.is_empty() {
        format!("{v_street}, {v_city}, {v_state} {v_zip}")
    } else {
        format!("{v_street} Unit {v_unit}, {v_city}, {v_state} {v_zip}")
    };

    Ok(SonicAvailabilityResult {
        status,
        available: status == SonicAvailabilityStatus::Available,
        preorder: status == SonicAvailabilityStatus::Preorder,
        carrier_name,
        status_label,
        final_url: Some(final_url),
        verified_address: Some(verified_addr_fmt),
        verified_lat: v_lat,
        verified_lon: v_lon,
        plans,
        max_download_mbps,
        max_upload_mbps,
        monthly_price_usd,
        check_url: SONIC_CHECK_PAGE_URL.to_string(),
        source_name: "Sonic Official Availability Checker".to_string(),
        provenance: "sonic_official_live".to_string(),
        checked_at: now_iso(),
        cached: false,
        reason: match status {
            SonicAvailabilityStatus::NotAvailable => Some("sonic_no_products".to_string()),
            SonicAvailabilityStatus::Preorder => Some("sonic_preorder_area".to_string()),
            SonicAvailabilityStatus::Unverified => Some("unrecognized_sonic_response".to_string()),
            SonicAvailabilityStatus::Available => None,
        },
    })
}

fn classify_fcc_technology(tech_code: Option<i64>, raw_tech: &str) -> (&'static str, String) {
    let lc = raw_tech.to_ascii_lowercase();
    if matches!(tech_code, Some(50)) || lc.contains("fiber") || lc.contains("fttp") || lc.contains("ftth") {
        (
            "fiber",
            if raw_tech.is_empty() {
                "Fiber (FTTH)".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(40 | 41 | 42 | 43))
        || lc.contains("cable")
        || lc.contains("docsis")
    {
        (
            "cable",
            if raw_tech.is_empty() {
                "Cable (DOCSIS)".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(70 | 71 | 72)) || lc.contains("wireless") || lc.contains("5g") {
        (
            "wireless",
            if raw_tech.is_empty() {
                "Fixed Wireless".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(60 | 61)) || lc.contains("satellite") {
        (
            "satellite",
            if raw_tech.is_empty() {
                "Satellite".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(10 | 11 | 12)) || lc.contains("dsl") || lc.contains("copper") {
        (
            "dsl",
            if raw_tech.is_empty() {
                "DSL / Copper".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else {
        (
            "other",
            if raw_tech.is_empty() {
                "Fixed Broadband".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    }
}

/// Parses an FCC BDC fixed location JSON payload, distinguishing FCC API error envelopes
/// (`{"status_code": 405, ...}`) from real provider arrays (`data` / `providers`).
pub fn parse_fcc_fixed_json(v: &Value) -> std::result::Result<Vec<FccProviderRecord>, String> {
    if let Some(code) = v.get("status_code").and_then(Value::as_i64) {
        if code >= 400 {
            return Err(format!("fcc_status_code_{code}"));
        }
    }
    let arr_opt = v
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| v.get("providers").and_then(Value::as_array))
        .or_else(|| v.as_array());
    let Some(arr) = arr_opt else {
        return Err("fcc_missing_carrier_array".to_string());
    };

    let mut out = Vec::new();
    for row in arr.iter().take(40) {
        let name = json_str(
            row.get("providerName")
                .or_else(|| row.get("carrierName"))
                .or_else(|| row.get("brand_name"))
                .or_else(|| row.get("holding_company_name"))
                .or_else(|| row.get("name")),
        )
        .unwrap_or("Broadband Carrier")
        .to_string();
        let holding_company = json_str(row.get("holding_company_name")).map(String::from);
        let tech_code = row
            .get("technology_code")
            .or_else(|| row.get("techCode"))
            .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
        let raw_tech = json_str(
            row.get("technology")
                .or_else(|| row.get("technology_code_desc"))
                .or_else(|| row.get("tech_name")),
        )
        .unwrap_or("");
        let (category, technology) = classify_fcc_technology(tech_code, raw_tech);
        let max_download_mbps = row
            .get("maxDownloadMbps")
            .or_else(|| row.get("max_advertised_download_speed"))
            .or_else(|| row.get("down"))
            .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64)))
            .unwrap_or(0);
        let max_upload_mbps = row
            .get("maxUploadMbps")
            .or_else(|| row.get("max_advertised_upload_speed"))
            .or_else(|| row.get("up"))
            .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64)))
            .unwrap_or(0);
        let low_latency = row.get("low_latency").and_then(Value::as_bool);
        let business_residential_code =
            json_str(row.get("business_residential_code")).map(String::from);

        out.push(FccProviderRecord {
            name,
            holding_company,
            technology_code: tech_code,
            technology,
            category: category.to_string(),
            max_download_mbps,
            max_upload_mbps,
            low_latency,
            business_residential_code,
        });
    }
    Ok(out)
}

/// Executes an honest live FCC BDC fixed broadband lookup. Supports optional
/// `FCC_BDC_USERNAME` + `FCC_BDC_API_TOKEN` (sent as `username` / `hash_value`
/// headers) and `FCC_BDC_API_URL`. Never fabricates carriers when FCC returns
/// HTTP 401/405, errors, times out, or runs in offline mode.
pub async fn check_fcc_fixed_broadband(
    http: &HttpClient,
    env: &EnvConfig,
    lat: Option<f64>,
    lon: Option<f64>,
) -> FccFixedBroadbandResult {
    let fcc_map_url = match (lat, lon) {
        (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => format!(
            "https://broadbandmap.fcc.gov/location-summary/fixed?latlon={la:.6}%2C{lo:.6}"
        ),
        _ => FCC_BDC_MAP_HOME_URL.to_string(),
    };

    let username = env.get("FCC_BDC_USERNAME").map(str::trim).filter(|s| !s.is_empty());
    let api_token = env
        .get("FCC_BDC_API_TOKEN")
        .or_else(|| env.get("FCC_BDC_API_KEY"))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let has_creds = username.is_some() && api_token.is_some();

    let (Some(la), Some(lo)) = (lat, lon) else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unconfigured,
            "missing_coordinates",
        );
    };
    if !la.is_finite() || !lo.is_finite() || env.get("FCC_BDC_DISABLED") == Some("1") {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unconfigured,
            "fcc_lookup_disabled_or_invalid_coords",
        );
    }

    let base = env
        .get("FCC_BDC_API_URL")
        .unwrap_or(FCC_BDC_FIXED_DEFAULT_URL);
    let Ok(mut url) = Url::parse(base) else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unconfigured,
            "invalid_fcc_api_url",
        );
    };
    url.query_pairs_mut()
        .append_pair("latitude", &format!("{la:.6}"))
        .append_pair("longitude", &format!("{lo:.6}"))
        .append_pair("category", "State");

    let Ok(client) = http.direct_client_for_url(url.as_str()) else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unavailable,
            "offline_mode_blocked",
        );
    };

    let cache_key = format!("fcc_fixed:v1:{}:{la:.5}:{lo:.5}", base);
    if !http.is_bypassing_cache_reads() {
        if let Some(cached_json) = http.read_cache(&cache_key, FCC_CACHE_TTL_SECS).await {
            if let Ok(mut parsed) = serde_json::from_str::<FccFixedBroadbandResult>(&cached_json) {
                parsed.cached = true;
                return parsed;
            }
        }
    }

    let mut req = client
        .get(url.clone())
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(2_500));
    if let (Some(user), Some(tok)) = (username, api_token) {
        req = req.header("username", user).header("hash_value", tok);
    }

    let resp = match tokio::time::timeout(Duration::from_millis(2_800), req.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            return FccFixedBroadbandResult::unavailable(
                fcc_map_url,
                true,
                has_creds,
                None,
                FccLookupStatus::Unavailable,
                "fcc_request_failed",
            );
        }
        Err(_) => {
            return FccFixedBroadbandResult::unavailable(
                fcc_map_url,
                true,
                has_creds,
                None,
                FccLookupStatus::Unavailable,
                "fcc_request_timeout",
            );
        }
    };

    let code = resp.status().as_u16();
    if code == 401 {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "http_401_unauthorized",
        );
    }
    if code == 405 {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "http_405_method_not_available",
        );
    }
    if !resp.status().is_success() {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            format!("http_{code}"),
        );
    }

    let Ok(val) = resp.json::<Value>().await else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "fcc_invalid_json",
        );
    };

    match parse_fcc_fixed_json(&val) {
        Ok(providers) => {
            let fiber_available = providers.iter().any(|p| p.category == "fiber");
            let cable_available = providers.iter().any(|p| p.category == "cable");
            let max_dl = providers.iter().map(|p| p.max_download_mbps).max();
            let max_ul = providers.iter().map(|p| p.max_upload_mbps).max();
            let status = if providers.is_empty() {
                FccLookupStatus::EmptyResult
            } else {
                FccLookupStatus::LiveVerified
            };
            let result = FccFixedBroadbandResult {
                status,
                queried: true,
                has_credentials: has_creds,
                http_status: Some(code),
                providers,
                fiber_available,
                cable_available,
                max_download_mbps: max_dl.filter(|&n| n > 0),
                max_upload_mbps: max_ul.filter(|&n| n > 0),
                source_name: "FCC National Broadband Map (BDC)".to_string(),
                source_url: fcc_map_url,
                provenance: "fcc_bdc_live".to_string(),
                checked_at: now_iso(),
                cached: false,
                reason: None,
            };
            if let Ok(encoded) = serde_json::to_string(&result) {
                http.write_cache(&cache_key, &encoded, FCC_CACHE_TTL_SECS)
                    .await;
            }
            result
        }
        Err(reason) => FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            reason,
        ),
    }
}

/// Runs both live Sonic verification and live FCC BDC fixed verification concurrently.
pub async fn check_live_broadband(
    http: &HttpClient,
    env: &EnvConfig,
    input: &SonicAddressInput,
) -> LiveBroadbandCheck {
    let (sonic, fcc_fixed) = tokio::join!(
        check_sonic_availability(http, env, input),
        check_fcc_fixed_broadband(http, env, input.lat, input.lon),
    );
    LiveBroadbandCheck { sonic, fcc_fixed }
}

/// Native `DataExtensionAdapter` exposing Sonic live address verification to `ExtensionHub`.
pub struct SonicAvailabilityAdapter;

impl DataExtensionAdapter for SonicAvailabilityAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: "sonic_fiber_availability".to_string(),
            name: "Sonic Official Address Availability".to_string(),
            version: "1.0.0".to_string(),
            description: "Live address-level Sonic Fiber verification via sonic.com/availability (no regional guessing)."
                .to_string(),
            domain: CapabilityDomain::DomainBroadband as i32,
            additional_domains: Vec::new(),
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                states: vec!["CA".to_string()],
                counties: Vec::new(),
                zip_prefixes: Vec::new(),
                zip_codes: Vec::new(),
                cities: Vec::new(),
                nationwide: false,
                specificity_override: Some(500),
                country: "US".to_string(),
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                env_var: String::new(),
                header_name: String::new(),
                query_param: String::new(),
                header_prefix: None,
                optional_key: true,
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 2,
                burst: 4,
                timeout_ms: 9_500,
                max_retries: 0,
            }),
            cache_ttl_secs: SONIC_CACHE_TTL_SECS as u64,
            priority: 450,
            enabled: true,
            builtin: true,
            proto_message_type: "openestate.extension.v1.BroadbandOptionProto".to_string(),
            openapi_spec_file: None,
            documentation_url: Some(SONIC_CHECK_PAGE_URL.to_string()),
            codeowners: vec!["@openestate/broadband".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            depends_on: Vec::new(),
            input_from_domains: Vec::new(),
            upstream_require_mode: "any".to_string(),
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let started = Instant::now();
            let input = SonicAddressInput {
                street: query.street.clone(),
                city: query.city.clone(),
                state: query.state.clone(),
                zip: query.zip.clone(),
                unit: query.extra_params.get("unit").cloned(),
                lat: query.lat,
                lon: query.lon,
            };
            let res = check_sonic_availability(http, env, &input).await;
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let prov_tier = match res.status {
                SonicAvailabilityStatus::Unverified => ProvenanceTier::ProvenanceUnverified as i32,
                _ => ProvenanceTier::ProvenanceLiveVerified as i32,
            };
            let attribution = Some(SourceAttributionProto {
                extension_id: "sonic_fiber_availability".to_string(),
                source_name: res.source_name.clone(),
                source_url: res
                    .final_url
                    .clone()
                    .unwrap_or_else(|| res.check_url.clone()),
                provenance_tier: prov_tier,
                fetched_at_rfc3339: res.checked_at.clone(),
                schedule_or_version: Some(res.status.as_str().to_string()),
                notes: Some(res.status_label.clone()),
            });

            if let Some(opt) = res.into_broadband_option() {
                Ok(vec![ExtensionEnvelopeProto {
                    extension_id: "sonic_fiber_availability".to_string(),
                    domain: CapabilityDomain::DomainBroadband as i32,
                    priority: 450,
                    geo_specificity: 500,
                    attribution,
                    latency_ms,
                    cached: res.cached,
                    payload: Some(Payload::BroadbandOption(opt)),
                }])
            } else {
                // Record verified unavailability / unverified probe outcome without
                // injecting a serviceable carrier into BroadbandCatalogProto.
                Ok(vec![ExtensionEnvelopeProto {
                    extension_id: "sonic_fiber_availability".to_string(),
                    domain: CapabilityDomain::DomainBroadband as i32,
                    priority: 450,
                    geo_specificity: 500,
                    attribution,
                    latency_ms,
                    cached: res.cached,
                    payload: Some(Payload::RawJson(serde_json::to_string(&res)?)),
                }])
            }
        })
    }
}

/// Native `DataExtensionAdapter` exposing honest FCC BDC fixed availability lookups.
pub struct FccBroadbandBdcAdapter;

impl DataExtensionAdapter for FccBroadbandBdcAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: "fcc_broadband_bdc_live".to_string(),
            name: "FCC National Broadband Map (BDC) Live".to_string(),
            version: "1.0.0".to_string(),
            description: "Honest live FCC BDC fixed carrier lookup; reports unavailable cleanly on HTTP 401/405 without fabricating carriers."
                .to_string(),
            domain: CapabilityDomain::DomainBroadband as i32,
            additional_domains: Vec::new(),
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
                auth_type: "api_key_header".to_string(),
                env_var: "FCC_BDC_API_TOKEN".to_string(),
                header_name: "hash_value".to_string(),
                query_param: String::new(),
                header_prefix: None,
                optional_key: true,
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 5,
                burst: 10,
                timeout_ms: 3_000,
                max_retries: 0,
            }),
            cache_ttl_secs: FCC_CACHE_TTL_SECS as u64,
            priority: 300,
            enabled: true,
            builtin: true,
            proto_message_type: "openestate.extension.v1.BroadbandOptionProto".to_string(),
            openapi_spec_file: None,
            documentation_url: Some(FCC_BDC_MAP_HOME_URL.to_string()),
            codeowners: vec!["@openestate/broadband".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            depends_on: Vec::new(),
            input_from_domains: Vec::new(),
            upstream_require_mode: "any".to_string(),
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let started = Instant::now();
            let res = check_fcc_fixed_broadband(http, env, query.lat, query.lon).await;
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let prov_tier = match res.status {
                FccLookupStatus::LiveVerified | FccLookupStatus::EmptyResult => {
                    ProvenanceTier::ProvenanceLiveVerified as i32
                }
                FccLookupStatus::Unavailable | FccLookupStatus::Unconfigured => {
                    ProvenanceTier::ProvenanceUnverified as i32
                }
            };
            let attribution = SourceAttributionProto {
                extension_id: "fcc_broadband_bdc_live".to_string(),
                source_name: res.source_name.clone(),
                source_url: res.source_url.clone(),
                provenance_tier: prov_tier,
                fetched_at_rfc3339: res.checked_at.clone(),
                schedule_or_version: Some(res.status.as_str().to_string()),
                notes: res.reason.clone(),
            };

            if res.status == FccLookupStatus::LiveVerified && !res.providers.is_empty() {
                let mut envelopes = Vec::with_capacity(res.providers.len());
                for p in &res.providers {
                    envelopes.push(ExtensionEnvelopeProto {
                        extension_id: "fcc_broadband_bdc_live".to_string(),
                        domain: CapabilityDomain::DomainBroadband as i32,
                        priority: 300,
                        geo_specificity: 500,
                        attribution: Some(attribution.clone()),
                        latency_ms,
                        cached: res.cached,
                        payload: Some(Payload::BroadbandOption(
                            p.to_broadband_option(&res.source_url),
                        )),
                    });
                }
                Ok(envelopes)
            } else {
                Ok(vec![ExtensionEnvelopeProto {
                    extension_id: "fcc_broadband_bdc_live".to_string(),
                    domain: CapabilityDomain::DomainBroadband as i32,
                    priority: 300,
                    geo_specificity: 500,
                    attribution: Some(attribution),
                    latency_ms,
                    cached: res.cached,
                    payload: Some(Payload::RawJson(serde_json::to_string(&res)?)),
                }])
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let s = String::from_utf8_lossy(&buf);
            if let Some(hdr_end) = s.find("\r\n\r\n") {
                let content_len = s[..hdr_end]
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            v.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                if buf.len() >= hdr_end + 4 + content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn spawn_local_broadband_server(sonic_available: bool, fcc_status_code: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback port");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let req = read_http_request(&mut stream).await;
                let first_line = req.lines().next().unwrap_or("");
                let has_session_cookie = req
                    .lines()
                    .any(|l| l.to_ascii_lowercase().starts_with("cookie:") && l.contains("COMAVAILID=ok"));

                let resp = if first_line.starts_with("GET /availability ") {
                    let body = r#"<html><form><input id="csrf_token" name="csrf_token" type="hidden" value="csrf_test_123"/></form></html>"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: availability-session=s1; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else if first_line.starts_with("POST /availability/verify_address/") {
                    let body = r#"{"success":true,"verified_address":{"address":"126 Synth Way","city":"Campbell","state":"CA","zip":"95008","unit":"","latitude":37.28178,"longitude":-121.97359},"errors":{}}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else if first_line.starts_with("POST /availability ") {
                    "HTTP/1.1 302 Found\r\nLocation: /availability/qual\r\nSet-Cookie: COMAVAILID=ok; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else if first_line.starts_with("GET /availability/qual") {
                    if !has_session_cookie {
                        "HTTP/1.1 302 Found\r\nLocation: /availability/no_products\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if sonic_available {
                        let body = r#"<html><h1><strong>Congratulations!</strong> Sonic Fiber Internet is available at your address.</h1><div data-product="xgspon_standard">10 Gbps Fiber Internet $49.99/mo <span id="nutritional_label_download">8,700 Mbps</span></div></html>"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    } else {
                        "HTTP/1.1 302 Found\r\nLocation: /availability/no_products\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    }
                } else if first_line.starts_with("GET /availability/no_products") {
                    let body = r#"<html><p>Sonic service is not available yet at your address.</p></html>"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else if first_line.starts_with("GET /fcc/fixed") {
                    if fcc_status_code == 405 {
                        let body = r#"{"status_code":405,"message":"Method Not Available"}"#;
                        format!(
                            "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    } else {
                        let body = r#"{"data":[{"brand_name":"AT&T","technology_code":50,"technology_code_desc":"Optical Carrier / Fiber to the Premises","max_advertised_download_speed":5000,"max_advertised_upload_speed":5000}]}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn sonic_live_check_follows_cookie_redirect_to_no_products() {
        let base = spawn_local_broadband_server(false, 405).await;
        let mut map = HashMap::new();
        map.insert("SONIC_AVAILABILITY_BASE_URL".to_string(), base.clone());
        let env = EnvConfig::from_map(map);
        let http = HttpClient::offline_loopback_only(None);
        let input = SonicAddressInput::new("126 Synth Way", "Campbell", "CA", "95008");

        let res = check_sonic_availability(&http, &env, &input).await;
        assert_eq!(res.status, SonicAvailabilityStatus::NotAvailable);
        assert!(!res.available);
        assert!(res
            .final_url
            .as_deref()
            .unwrap_or("")
            .ends_with("/availability/no_products"));
        assert!(res.into_broadband_option().is_none());
    }

    #[tokio::test]
    async fn sonic_live_check_extracts_qualified_10g_plan_when_available() {
        let base = spawn_local_broadband_server(true, 200).await;
        let mut map = HashMap::new();
        map.insert("SONIC_AVAILABILITY_BASE_URL".to_string(), base);
        let env = EnvConfig::from_map(map);
        let http = HttpClient::offline_loopback_only(None);
        let input = SonicAddressInput::new("1230 Synth Ave", "San Jose", "CA", "95125");

        let res = check_sonic_availability(&http, &env, &input).await;
        assert_eq!(res.status, SonicAvailabilityStatus::Available);
        assert!(res.available);
        assert_eq!(res.carrier_name, "Sonic 10G Fiber");
        assert_eq!(res.max_download_mbps, Some(10_000));
        assert_eq!(res.monthly_price_usd.as_deref(), Some("$49.99/mo"));
        let opt = res.into_broadband_option().expect("confirmed option");
        assert_eq!(opt.availability_status, "confirmed_address");
    }

    #[tokio::test]
    async fn sonic_live_check_never_guesses_when_offline_external() {
        let env = EnvConfig::from_map(HashMap::new());
        let http = HttpClient::offline_loopback_only(None);
        let input = SonicAddressInput::new("126 Synth Way", "Campbell", "CA", "95008");

        let res = check_sonic_availability(&http, &env, &input).await;
        assert_eq!(res.status, SonicAvailabilityStatus::Unverified);
        assert!(!res.available);
        assert_eq!(res.reason.as_deref(), Some("offline_mode_blocked"));
    }

    #[tokio::test]
    async fn fcc_fixed_reports_unavailable_on_http_405_without_fabricating() {
        let base = spawn_local_broadband_server(false, 405).await;
        let mut map = HashMap::new();
        map.insert("FCC_BDC_API_URL".to_string(), format!("{base}/fcc/fixed"));
        let env = EnvConfig::from_map(map);
        let http = HttpClient::offline_loopback_only(None);

        let fcc = check_fcc_fixed_broadband(&http, &env, Some(37.282354), Some(-121.961661)).await;
        assert_eq!(fcc.status, FccLookupStatus::Unavailable);
        assert!(fcc.queried);
        assert_eq!(fcc.http_status, Some(405));
        assert!(fcc.providers.is_empty());
        assert!(!fcc.fiber_available);
        assert_eq!(fcc.reason.as_deref(), Some("http_405_method_not_available"));
    }

    #[test]
    fn sonic_preorder_classifier_never_marks_available() {
        let html = "<h1>Sonic Fiber is Coming Soon to Your Neighborhood! Pre-order now to reserve your spot.</h1>";
        let (status, plans, _label) =
            parse_sonic_qual_html("https://www.sonic.com/availability/qual", html);
        assert_eq!(status, SonicAvailabilityStatus::Preorder);
        assert!(plans.is_empty());
    }
}
