//! Consolidated pure Rust helpers shared across valuation, modules, CRE, search, scanner, AI, and web:
//! - Geodesy constants (`SQFT_PER_ACRE`, `WGS84_EQUATORIAL_RADIUS_M`) & `LocalTangentFrame`
//! - Number / currency / distance formatters (`fmt_commas`, `fmt_usd`, `fmt_usd_compact`, `fmt_mi_from_m`, `title_case`, `sha256_hex`)
//! - JSON readers (`unwrap_data`, `val_f64`, `val_i64`, `val_str`, `resolve_subject_price`)
//! - Mortgage amortization (`monthly_pi`, `remaining_balance_after_months`)
//! - Word-boundary-safe internal tech token scrubber (`scrub_internal_tech_tokens_word_safe`)
//! - Unified SSRF public-IP and public-HTTPS URL validator
//! - W3C Scroll-to-Text Fragment anchor builder (`with_data_point_anchor`)

use crate::shared::{normalize_retired_brand, round_n, wgs84_m_per_deg, MILE_M};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use url::Url;

pub const SQFT_PER_ACRE: f64 = 43_560.0;
pub const WGS84_EQUATORIAL_RADIUS_M: f64 = 6_378_137.0;

/// Canonical local east-north tangent plane (meters) centered at `(lat0, lon0)` using exact
/// WGS84 meridional and prime-vertical radii (`wgs84_m_per_deg`).
/// Shared by `modules::fengshui` and `modules::house3d::report_input` so site-plan rings and 3D
/// meshes align with zero spherical-vs-ellipsoidal drift.
#[derive(Debug, Clone, Copy)]
pub struct LocalTangentFrame {
    pub lat0: f64,
    pub lon0: f64,
    pub kx: f64,
    pub ky: f64,
}

impl LocalTangentFrame {
    #[inline]
    pub fn new(lat0: f64, lon0: f64) -> Self {
        let (kx, ky) = wgs84_m_per_deg(lat0);
        Self {
            lat0,
            lon0,
            kx: kx.max(1e-6),
            ky: ky.max(1e-6),
        }
    }

    #[inline]
    pub fn fwd(&self, lat: f64, lon: f64) -> (f64, f64) {
        ((lon - self.lon0) * self.kx, (lat - self.lat0) * self.ky)
    }

    #[inline]
    pub fn inv(&self, x: f64, y: f64) -> (f64, f64) {
        (self.lat0 + y / self.ky, self.lon0 + x / self.kx)
    }
}

fn round_coord_array_6dec(val: &mut Value) {
    if let Some(arr) = val.as_array_mut() {
        for elem in arr.iter_mut() {
            if let Some(n) = elem.as_f64() {
                if n.is_finite() {
                    let r = round_n(n, 6);
                    let r = if r == 0.0 { 0.0 } else { r };
                    if let Some(num) = serde_json::Number::from_f64(r) {
                        *elem = Value::Number(num);
                    }
                }
            } else if elem.is_array() {
                round_coord_array_6dec(elem);
            }
        }
    }
}

/// Recursively rounds numeric coordinates inside GeoJSON `"coordinates"` arrays to 6 decimal
/// places (~11 cm WGS84 precision) while leaving financial, score, tax, and ratio floats intact.
pub fn round_geojson_coordinates_6dec(val: &mut Value) {
    match val {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "coordinates" {
                    round_coord_array_6dec(v);
                } else {
                    round_geojson_coordinates_6dec(v);
                }
            }
        }
        Value::Array(arr) => {
            for elem in arr.iter_mut() {
                round_geojson_coordinates_6dec(elem);
            }
        }
        _ => {}
    }
}

// =================================================================================================
// 1. Number, Currency, Distance & String Formatters
// =================================================================================================

/// Format an integer with ASCII thousands commas (`1234567` -> `"1,234,567"`, `-4500` -> `"-4,500"`).
pub fn fmt_commas(n: i64) -> String {
    let abs_str = n.unsigned_abs().to_string();
    let mut rev = String::with_capacity(abs_str.len() + abs_str.len() / 3 + 1);
    for (i, ch) in abs_str.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            rev.push(',');
        }
        rev.push(ch);
    }
    let formatted: String = rev.chars().rev().collect();
    if n < 0 {
        format!("-{formatted}")
    } else {
        formatted
    }
}

/// Format a floating-point USD value rounded to whole dollars (`"$1,450,000"`, `"-$500"`, or `"—"` if non-finite).
pub fn fmt_usd(x: f64) -> String {
    if !x.is_finite() {
        return "—".to_string();
    }
    let rounded = x.round() as i64;
    if rounded < 0 {
        format!("-${}", fmt_commas(-rounded))
    } else {
        format!("${}", fmt_commas(rounded))
    }
}

/// Format compact USD (`$1.45M`, `$650K`, `$850`).
pub fn fmt_usd_compact(x: f64) -> String {
    if !x.is_finite() {
        return "—".to_string();
    }
    let abs = x.abs();
    let sign = if x < 0.0 { "-" } else { "" };
    if abs >= 1_000_000.0 {
        format!("{sign}${:.2}M", abs / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{sign}${:.0}K", abs / 1_000.0)
    } else {
        format!("{sign}${:.0}", abs)
    }
}

/// Format meters as human-readable US miles (`"0.24 mi"` or `"1.8 mi"`).
pub fn fmt_mi_from_m(meters: f64) -> String {
    if !meters.is_finite() || meters < 0.0 {
        return "—".to_string();
    }
    let mi = meters / MILE_M;
    if mi < 1.0 {
        format!("{:.2} mi", round_n(mi, 2))
    } else {
        format!("{:.1} mi", round_n(mi, 1))
    }
}

static RE_WORD_START: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b([a-z])").expect("word start regex"));

/// ASCII/Unicode-safe title-casing (`"SAN JOSE"` -> `"San Jose"`).
pub fn title_case(s: &str) -> String {
    let low = s.to_lowercase();
    RE_WORD_START
        .replace_all(&low, |caps: &regex::Captures| caps[1].to_uppercase())
        .into_owned()
}

/// Lowercase hex SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// =================================================================================================
// 2. JSON Value Readers & Subject Price Resolver
// =================================================================================================

/// Unwraps `{ "data": { ... } }` module envelopes or returns `v` directly when already unwrapped.
#[inline]
pub fn unwrap_data<'a>(v: &'a Value) -> &'a Value {
    v.get("data").filter(|d| d.is_object()).unwrap_or(v)
}

#[inline]
pub fn val_f64(v: &Value, key: &str) -> Option<f64> {
    let cell = v.get(key)?;
    cell.as_f64()
        .or_else(|| cell.as_i64().map(|n| n as f64))
        .or_else(|| cell.as_str().and_then(|s| s.trim().replace([',', '$'], "").parse::<f64>().ok()))
        .filter(|x| x.is_finite())
}

#[inline]
pub fn val_i64(v: &Value, key: &str) -> Option<i64> {
    let cell = v.get(key)?;
    cell.as_i64()
        .or_else(|| cell.as_f64().filter(|x| x.is_finite()).map(|x| x.round() as i64))
        .or_else(|| cell.as_str().and_then(|s| s.trim().replace([',', '$'], "").parse::<i64>().ok()))
}

#[inline]
pub fn val_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Resolve the subject property's positive reference price from a full report JSON:
/// prefers `valuation.estimate.value` -> `valuation.ownership.price` -> `facts.listPrice` ->
/// `facts.estimatedValue` -> `facts.lastSale.price` -> `market.zhvi.latest`.
pub fn resolve_subject_price(report: &Value) -> Option<f64> {
    let val_mod = report
        .pointer("/modules/valuation")
        .map(unwrap_data)
        .unwrap_or(&Value::Null);
    let facts = report
        .get("facts")
        .map(unwrap_data)
        .unwrap_or(&Value::Null);
    let mkt = report
        .pointer("/modules/market")
        .map(unwrap_data)
        .unwrap_or(&Value::Null);

    [
        val_mod.pointer("/estimate/value").and_then(|v| v.as_f64()),
        val_mod.pointer("/ownership/price").and_then(|v| v.as_f64()),
        val_mod.pointer("/ownershipCost/price").and_then(|v| v.as_f64()),
        facts.get("listPrice").and_then(|v| v.as_f64()),
        facts.get("estimatedValue").and_then(|v| v.as_f64()),
        facts.pointer("/lastSale/price").and_then(|v| v.as_f64()),
        mkt.pointer("/zhvi/latest").and_then(|v| v.as_f64()),
    ]
    .into_iter()
    .flatten()
    .find(|&p| p.is_finite() && p > 0.0)
}

// =================================================================================================
// 3. Mortgage Amortization Math
// =================================================================================================

/// Monthly level-payment principal & interest (`P&I`) in USD for `principal` at `annual_rate_pct` over `years`.
#[inline]
pub fn monthly_pi(principal: f64, annual_rate_pct: f64, years: usize) -> f64 {
    if !principal.is_finite() || principal <= 0.0 || years == 0 {
        return 0.0;
    }
    let n = (years * 12) as f64;
    let r = annual_rate_pct / 1200.0;
    if !r.is_finite() || r <= 0.0 {
        return principal / n;
    }
    let factor = (1.0 + r).powf(n);
    (principal * r * factor) / (factor - 1.0)
}

/// Remaining principal balance after `months_paid` payments on a level-payment amortizing mortgage.
#[inline]
pub fn remaining_balance_after_months(
    principal: f64,
    annual_rate_pct: f64,
    total_years: usize,
    months_paid: usize,
) -> f64 {
    if !principal.is_finite() || principal <= 0.0 || total_years == 0 {
        return 0.0;
    }
    let n = (total_years * 12) as f64;
    let p = (months_paid as f64).min(n);
    let r = annual_rate_pct / 1200.0;
    if !r.is_finite() || r <= 0.0 {
        return (principal * (1.0 - p / n)).max(0.0);
    }
    let f_n = (1.0 + r).powf(n);
    let f_p = (1.0 + r).powf(p);
    (principal * (f_n - f_p) / (f_n - 1.0)).max(0.0)
}

// =================================================================================================
// 4. Word-Boundary-Safe Tech Token Scrubber (fixes `"effortless"` -> `"workless"` corruption)
// =================================================================================================

static RE_EFFORTLESSLY: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)(eff", "ortlessly)")).expect("effortlessly regex"));
static RE_EFFORTLESS: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)(eff", "ortless)")).expect("effortless regex"));
static RE_EFFORTS: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)(eff", "orts)")).expect("efforts regex"));
static RE_EFFORT: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)(eff", "ort)")).expect("effort regex"));
static RE_PROVIDERS: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)(pro", "viders)")).expect("providers regex"));
static RE_PROVIDER: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)(pro", "vider)")).expect("provider regex"));

fn match_casing(matched: &str, lower_repl: &str) -> String {
    if lower_repl.is_empty() {
        return String::new();
    }
    if matched.chars().all(|c| !c.is_ascii_lowercase()) {
        lower_repl.to_ascii_uppercase()
    } else if matched.starts_with(|c: char| c.is_ascii_uppercase()) {
        let mut out = lower_repl[..1].to_ascii_uppercase();
        out.push_str(&lower_repl[1..]);
        out
    } else {
        lower_repl.to_string()
    }
}

/// Scrubs internal `providers`/`provider`/`effort` tokens while turning `effortlessly` -> `seamlessly`
/// and `effortless` -> `seamless` first so English words are never mangled into `"workless"`/`"scopeless"`/`"modeless"`,
/// while also guaranteeing zero residual `"effort"` or `"provider"` substrings in rendered HTML/JSON.
pub fn scrub_internal_tech_words(
    input: &str,
    effort_repl_lower: &str,
    provider_singular_lower: &str,
    provider_plural_lower: &str,
) -> String {
    let mut out = normalize_retired_brand(input);
    for (re, repl) in [
        (&*RE_EFFORTLESSLY, "seamlessly"),
        (&*RE_EFFORTLESS, "seamless"),
        (&*RE_PROVIDERS, provider_plural_lower),
        (&*RE_PROVIDER, provider_singular_lower),
        (&*RE_EFFORTS, effort_repl_lower),
        (&*RE_EFFORT, effort_repl_lower),
    ] {
        if re.is_match(&out) {
            out = re
                .replace_all(&out, |c: &regex::Captures| match_casing(&c[1], repl))
                .into_owned();
        }
    }
    out
}

// =================================================================================================
// 5. Unified SSRF Public IP & Public HTTPS URL Validation
// =================================================================================================

pub fn is_public_routable_ipv4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    if v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || v4.is_multicast()
    {
        return false;
    }
    // 0.0.0.0/8, 100.64.0.0/10 (CGNAT), 169.254.169.254 (cloud metadata), 192.0.0.0/24,
    // 198.18.0.0/15 (benchmarking), 240.0.0.0/4 (reserved)
    if o[0] == 0
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] >= 240
    {
        return false;
    }
    true
}

pub fn is_public_routable_ipv6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
        return false;
    }
    if let Some(mapped_v4) = v6.to_ipv4_mapped() {
        return is_public_routable_ipv4(mapped_v4);
    }
    let segs = v6.segments();
    if segs[..6] == [0, 0, 0, 0, 0, 0] {
        let compat_v4 = Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            (segs[6] & 0xff) as u8,
            (segs[7] >> 8) as u8,
            (segs[7] & 0xff) as u8,
        );
        return is_public_routable_ipv4(compat_v4);
    }
    // Unique local fc00::/7, link-local fe80::/10, site-local fec0::/10,
    // documentation 2001:db8::/32, NAT64 64:ff9b::/96 & 64:ff9b:1::/48
    let is_nat64 = (segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0 && segs[3] == 0)
        || (segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001);
    if (segs[0] & 0xfe00) == 0xfc00
        || (segs[0] & 0xffc0) == 0xfe80
        || (segs[0] & 0xffc0) == 0xfec0
        || (segs[0] == 0x2001 && segs[1] == 0x0db8)
        || is_nat64
    {
        return false;
    }
    true
}

pub fn is_public_routable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_routable_ipv4(v4),
        IpAddr::V6(v6) => is_public_routable_ipv6(v6),
    }
}

/// Validates that `url_str` uses `https://` (or `http://` when `allow_test_loopback` is explicitly set
/// for unit tests), carries no credentials, targets a non-internal hostname, and resolves strictly to
/// public routable IP addresses.
pub async fn validate_ssrf_safe_public_url(
    url_str: &str,
    allow_test_loopback: bool,
) -> Result<Url, String> {
    let parsed = Url::parse(url_str.trim()).map_err(|_| "Malformed URL".to_string())?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if allow_test_loopback {
        if scheme != "https" && scheme != "http" {
            return Err("Unsupported URL scheme".to_string());
        }
    } else if scheme != "https" {
        return Err("Only HTTPS public URLs are allowed".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URLs with embedded credentials are prohibited".to_string());
    }

    let host = parsed
        .host_str()
        .map(|h| h.trim_matches(['[', ']']).to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "Missing URL host".to_string())?;

    if allow_test_loopback && (host == "127.0.0.1" || host == "localhost" || host == "::1") {
        return Ok(parsed);
    }

    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".svc")
        || host.ends_with(".arpa")
        || host == "metadata.google.internal"
    {
        return Err("Internal/local hostnames are blocked by SSRF policy".to_string());
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_routable_ip(ip) {
            return Err(format!("Non-public IP {ip} blocked by SSRF policy"));
        }
        return Ok(parsed);
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    let resolved = tokio::time::timeout(
        Duration::from_millis(1_800),
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    .map_err(|_| "DNS lookup timed out".to_string())?
    .map_err(|_| "DNS resolution failed".to_string())?;

    let mut count = 0usize;
    for sock in resolved {
        count += 1;
        if !is_public_routable_ip(sock.ip()) {
            return Err(format!(
                "Host resolved to non-public IP {} (blocked by SSRF policy)",
                sock.ip()
            ));
        }
    }
    if count == 0 {
        return Err("Host resolved to zero addresses".to_string());
    }
    Ok(parsed)
}

// =================================================================================================
// 6. W3C Scroll-to-Text Fragment Anchor Helper (`#:~:text=...`)
// =================================================================================================

fn sanitize_anchor_fragment_text(raw: &str) -> Option<String> {
    let collapsed: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let s = collapsed.trim();
    if s.is_empty() || s == "-" || s == "—" {
        return None;
    }
    let low = s.to_ascii_lowercase();
    if matches!(
        low.as_str(),
        "n/a" | "none" | "null" | "undefined" | "unknown" | "yes" | "no" | "true" | "false" | "verify"
    ) {
        return None;
    }
    let low_owner_check = low.replace("homeowners", "").replace("homeowner", "");
    if low_owner_check.contains("owner") {
        return None;
    }
    for bad in [
        "assessee",
        "grantor",
        "grantee",
        "trustee",
        "taxpayer",
        "mailing",
        "ssn",
        "@",
        "gpt-",
        "sqlx",
        "fallback",
        "reasoning",
        "aimodel",
        "llm-proxy",
        "effort",
        "provider",
        "homelens",
    ] {
        if low.contains(bad) {
            return None;
        }
    }
    let clipped: String = s.chars().take(72).collect();
    let out = clipped.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// Validate a clean `http://` or `https://` URL while preserving its exact path/slash formatting
/// and rejecting forbidden internal keywords (`provider`, `effort`).
pub fn clean_http_url_verbatim(u: &str) -> Option<String> {
    let t = u.trim();
    if !(t.starts_with("https://") || t.starts_with("http://")) {
        return None;
    }
    let low = t.to_ascii_lowercase();
    if low.contains("provider") || low.contains("effort") {
        return None;
    }
    Url::parse(t).ok().map(|_| t.to_string())
}

/// Attach an optional `#section` anchor and/or W3C Scroll-to-Text Fragment (`#:~:text=...`)
/// pointing directly to the exact data point on the target page, preserving `#map=` fragments
/// and stripping any PII or internal tokens.
pub fn with_data_point_anchor(
    base_url: &str,
    section_anchor: Option<&str>,
    text_fragment: Option<&str>,
) -> String {
    let Some(u) = clean_http_url_verbatim(base_url) else {
        return base_url.trim().to_string();
    };
    let clean_txt = text_fragment.and_then(sanitize_anchor_fragment_text);
    let clean_sec_arg = section_anchor
        .and_then(sanitize_anchor_fragment_text)
        .map(|sec| {
            sec.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());

    if u.contains(":~:text=") && clean_txt.is_none() && clean_sec_arg.is_none() {
        return u;
    }

    let (base_no_hash, raw_hash) = match u.find('#') {
        Some(idx) => (&u[..idx], &u[idx + 1..]),
        None => (u.as_str(), ""),
    };
    if raw_hash.to_ascii_lowercase().starts_with("map=") {
        return u;
    }
    let (existing_sec, existing_tf) = match raw_hash.find(":~:text=") {
        Some(idx) => (&raw_hash[..idx], &raw_hash[idx + 8..]),
        None => (raw_hash, ""),
    };
    let eff_sec = clean_sec_arg.or_else(|| {
        let s: String = existing_sec
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
            .collect();
        (!s.is_empty()).then_some(s)
    });
    let eff_tf = if let Some(ref ct) = clean_txt {
        let mut enc = urlencoding::encode(ct).into_owned();
        if enc.starts_with('-') {
            enc = format!("%2D{}", &enc[1..]);
        }
        if enc.ends_with('-') {
            enc.truncate(enc.len() - 1);
            enc.push_str("%2D");
        }
        Some(enc)
    } else if !existing_tf.is_empty() {
        Some(existing_tf.to_string())
    } else {
        None
    };

    match (eff_sec, eff_tf) {
        (Some(sec), Some(tf)) => format!("{base_no_hash}#{sec}:~:text={tf}"),
        (Some(sec), None) => format!("{base_no_hash}#{sec}"),
        (None, Some(tf)) => format!("{base_no_hash}#:~:text={tf}"),
        (None, None) => base_no_hash.to_string(),
    }
}

/// Returns `true` only when `raw_url` points to a static HTML document/profile page where
/// W3C Scroll-to-Text Fragments (`#:~:text=`) reliably match rendered text — never on generic
/// homepages, search forms, SPAs/GIS viewers, JSON/REST APIs, or query-param parcel lookups.
pub fn url_supports_text_fragment(raw_url: &str) -> bool {
    let before_hash = raw_url.split('#').next().unwrap_or(raw_url).trim();
    let low = before_hash.to_ascii_lowercase();
    if low.is_empty()
        || low.contains("/mapserver")
        || low.contains("/featureserver")
        || low.contains("/imageserver")
        || low.contains("/rest/services/")
        || low.contains("experiencebuilder")
        || low.contains("webappviewer")
        || low.contains("maps.arcgis.com")
        || low.contains("experience.arcgis.com")
        || low.contains("geocoding.geo.census.gov")
        || low.contains("epqs.nationalmap.gov")
        || low.ends_with(".json")
        || low.contains(".json?")
        || low.contains("f=json")
        || low.contains("f=pjson")
        || low.contains("f=html")
    {
        return false;
    }
    low.contains("censusreporter.org/profiles/")
        || low.contains("flsenate.gov/laws/statutes/")
        || low.contains("app.leg.wa.gov/")
        || low.contains("leg.state.nv.us/")
        || low.contains("ecfr.gov/")
        || low.contains("epa.gov/lead/")
        || low.contains("projects.propublica.org/nonprofits/organizations/")
}

/// Clean `base_url` and attach `#section` and/or `#:~:text=` ONLY when `url_supports_text_fragment`
/// confirms the destination is a verifiable static HTML document (stripping any stale `#:~:text=`
/// on homepages, SPAs, APIs, and query-param parcel portals).
pub fn with_verified_data_anchor(
    base_url: &str,
    section_anchor: Option<&str>,
    text_fragment: Option<&str>,
) -> String {
    let Some(u) = clean_http_url_verbatim(base_url) else {
        return base_url.trim().to_string();
    };
    let (base_no_hash, raw_hash) = match u.find('#') {
        Some(idx) => (&u[..idx], &u[idx + 1..]),
        None => (u.as_str(), ""),
    };
    if raw_hash.to_ascii_lowercase().starts_with("map=") {
        return u;
    }
    if url_supports_text_fragment(base_no_hash) {
        return with_data_point_anchor(&u, section_anchor, text_fragment);
    }
    let existing_sec = match raw_hash.find(":~:text=") {
        Some(idx) => &raw_hash[..idx],
        None => raw_hash,
    };
    let eff_sec = section_anchor
        .and_then(sanitize_anchor_fragment_text)
        .or_else(|| (!existing_sec.is_empty()).then(|| existing_sec.to_string()))
        .map(|sec| {
            sec.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());
    match eff_sec {
        Some(sec) => format!("{base_no_hash}#{sec}"),
        None => base_no_hash.to_string(),
    }
}
