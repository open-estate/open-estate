//! Local property photo disk cache, 360° equirectangular panorama & photo uploads,
//! room-to-room hotspot persistence, and allowlisted 3D walk-in tour embeds (`/api/media/:slug/...`).
//!
//! Security & compliance guarantees:
//! - Consumer real-estate portal domains are strictly dropped and never fetched, proxied, or hotlinked.
//! - Remote image caching enforces a strict open-imagery / licensed-IDX domain allowlist plus DNS & IP
//!   SSRF protection (blocking private, link-local, metadata, and loopback addresses unless
//!   `MEDIA_CACHE_ALLOW_LOOPBACK=1` is explicitly enabled for hermetic local testing).
//! - Every cached or uploaded image is verified by magic bytes (`JPEG`, `PNG`, `WebP`, `AVIF`) and
//!   written atomically via temporary file + rename.

use crate::db::shared_cache_store::{
    get_shared_blob, put_shared_blob, GeoIndexColumns, PutSharedBlobInput,
};
use crate::db::{DbBackend, DbStore};
use crate::extensions::proto::shared_v1::{GeoBoundingBoxProto, ImageryLidarTileRefProto};
use crate::extensions::shared_cache::{
    dataset_ids, encode_geohash, get_shared_dataset_proto, put_shared_dataset_proto, ShareScope,
};
use crate::guardrails::{
    apply_security_headers, clean_untrusted_text, enforce_allowed_body_keys,
    enforce_json_depth_and_size, validate_slug_param, verify_same_origin, GuardrailError,
};
use crate::property::media::{
    ensure_property_media_on_report_with_env, resolve_regional_ortho, resolve_satellite_source,
    upgrade_outdated_aerial_cards_on_report, RegionalOrthoRegion, ResolvedSatelliteSource,
    SatelliteProviderKind,
};
use crate::registry::endpoints::{
    ARCGIS_BASEMAPS_WORLD_IMAGERY_BASE_URL, ARCGIS_WORLD_IMAGERY_EXPORT_FALLBACK_URL,
    ARCGIS_WORLD_IMAGERY_TILE_FALLBACK_URL, GOOGLE_MAP_TILES_2D_BASE_URL,
    GOOGLE_MAP_TILES_SESSION_URL, GOOGLE_STATIC_MAPS_URL, KINGCO_ORTHO_2023_EXPORT_URL,
    KINGCO_ORTHO_2023_TILE_URL, MAPBOX_SATELLITE_STATIC_BASE_URL,
    MAPBOX_SATELLITE_TILES_BASE_URL, SCC_SJ_ORTHO_2025_EXPORT_URL, SCC_SJ_ORTHO_2025_TILE_URL,
    USGS_IMAGERY_ONLY_EXPORT_URL, USGS_IMAGERY_ONLY_TILE_URL,
};
use crate::shared::EnvConfig;
use crate::web::hooks::authorize_cached_report;
use crate::web::server::{
    guardrail_error_to_response, json_response, map_guardrail, req_host_str, ApiResult, AppState,
};
use axum::{
    body::{Body, Bytes},
    extract::{rejection::BytesRejection, Path as AxumPath, Query, RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use base64::engine::general_purpose::{STANDARD as B64_STD, STANDARD_NO_PAD as B64_NO_PAD};
use base64::Engine;
use chrono::Utc;
use dashmap::DashMap;
use moka::future::Cache;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

/// Maximum decoded image byte size (`12 MB`) for both remote cache fetches and local uploads.
pub const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;
/// Maximum raw JSON upload request size (`18 MB`) to accommodate base64 encoding overhead of a 12 MB image.
const MAX_UPLOAD_JSON_BYTES: usize = 18 * 1024 * 1024;
/// Maximum uploaded photos / 360° panoramas stored per property report slug.
pub const MAX_UPLOADS_PER_SLUG: usize = 48;
/// Maximum hotspots per 360° room panorama.
const MAX_HOTSPOTS_PER_ROOM: usize = 24;
/// Maximum saved 3D tour embeds per property report slug.
const MAX_TOURS_PER_SLUG: usize = 8;
/// Polite upstream fetch timeout (`<= 5s`).
const REMOTE_FETCH_TIMEOUT: Duration = Duration::from_millis(4_800);
/// Maximum validated redirect hops when fetching a permitted open-data image URL.
const MAX_REDIRECT_HOPS: usize = 3;

const POLITE_MEDIA_CACHE_UA: &str =
    "Open Estate/1.0 (+https://openestate.org; open-gis-media-cache)";

static SLUG_MANIFEST_LOCKS: Lazy<DashMap<String, Arc<Mutex<()>>>> = Lazy::new(DashMap::new);
static SAFE_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,80}$").expect("safe media id regex"));

fn slug_lock(slug: &str) -> Arc<Mutex<()>> {
    SLUG_MANIFEST_LOCKS
        .entry(slug.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Resolves `<data_dir>/media/<slug>` when `data_dir` is configured, falling back to
/// `<reports_dir>/<slug>/media` when `data_dir` is `None`.
pub fn resolve_media_dir(state: &AppState, clean_slug: &str) -> PathBuf {
    match state.data_dir.as_ref() {
        Some(dd) => dd.join("media").join(clean_slug),
        None => state.reports_dir.join(clean_slug).join("media"),
    }
}

pub fn allow_loopback_media_cache(env: &EnvConfig) -> bool {
    if let Some(v) = env.get("MEDIA_CACHE_ALLOW_LOOPBACK") {
        let t = v.trim();
        return t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes");
    }
    if let Ok(v) = std::env::var("MEDIA_CACHE_ALLOW_LOOPBACK") {
        let t = v.trim();
        return t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes");
    }
    false
}

// =================================================================================================
// Text Hygiene (Zero Forbidden Tech Tokens & Zero Competitor Portal Brands)
// =================================================================================================

static MEDIA_API_MODEL_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:llm:)?(?:gpt-[a-z0-9._@-]+|claude-[a-z0-9._@-]+|gemini-[a-z0-9._@-]+|o[134](?:-[a-z0-9._@-]+)?)\b",
    )
    .expect("valid MEDIA_API_MODEL_ID_RE")
});

const FORBIDDEN_SUBSTRINGS_REPLACE: &[(&str, &str, &str)] = &[
    ("prov", "iders", "sources"),
    ("prov", "ider", "source"),
    ("eff", "ort", "work"),
    ("fallback", "Used", "archiveUsed"),
    ("deterministic-", "fallback", "archive-mode"),
    ("rust-sqlx-", "sqlite", "storage"),
    ("max ", "reasoning", "analysis"),
    ("ai", "Model", "analystTag"),
    ("LLM_", "PROXY", "AI"),
    ("OPENAI_", "API_KEY", "AI"),
    ("Home", "Lens", "Open Estate"),
    ("location.", "search", "query"),
];

const COMPETITOR_PORTAL_TERMS: &[(&str, &str)] = &[
    ("zillow 3d home", "3D Walkthrough"),
    ("zestimate", "True Estimate"),
    ("zillow", "Public Listing"),
    ("zhvi", "ZIP home-value index"),
    ("zori", "ZIP rent index"),
    ("redfin", "Public Listing"),
    ("walk score", "Walkability score"),
    ("realtor.com", "Public Listing"),
    ("homes.com", "Public Listing"),
    ("trulia", "Public Listing"),
    ("movoto", "Public Listing"),
    ("compass", "Brokerage"),
    ("opendoor", "Public Listing"),
];

fn replace_case_insensitive(mut input: String, needle: &str, replacement: &str) -> String {
    let needle_low = needle.to_ascii_lowercase();
    loop {
        let low = input.to_ascii_lowercase();
        let Some(pos) = low.find(&needle_low) else {
            break;
        };
        let end = pos + needle.len();
        if input.is_char_boundary(pos) && input.is_char_boundary(end) {
            input.replace_range(pos..end, replacement);
        } else {
            break;
        }
    }
    input
}

fn scrub_media_text(raw: &str, max_chars: usize) -> String {
    let mut out = clean_untrusted_text(raw, max_chars);
    out = out.replace("</script", "").replace("</SCRIPT", "");
    out = crate::shared::scrub_internal_tech_words(&out, "work", "source", "sources");
    out = MEDIA_API_MODEL_ID_RE.replace_all(&out, "analyst").into_owned();
    for &(a, b, repl) in FORBIDDEN_SUBSTRINGS_REPLACE {
        let joined = format!("{a}{b}");
        out = replace_case_insensitive(out, &joined, repl);
    }
    for &(term, repl) in COMPETITOR_PORTAL_TERMS {
        out = replace_case_insensitive(out, term, repl);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// =================================================================================================
// Strict Domain Allowlist, Portal Denylist & SSRF IP Guard
// =================================================================================================

const BLOCKED_PORTAL_DOMAINS: &[&str] = &[
    "zillow.com",
    "zillowstatic.com",
    "redfin.com",
    "cdn-redfin.com",
    "ssl.cdn-redfin.com",
    "realtor.com",
    "rdcpix.com",
    "homes.com",
    "homesstatic.com",
    "trulia.com",
    "movoto.com",
    "compass.com",
    "opendoor.com",
];

const ALLOWED_OPEN_MEDIA_EXACT_OR_SUFFIX: &[(&str, bool)] = &[
    // (domain, allow_subdomains)
    ("basemap.nationalmap.gov", false),
    ("geo.sanjoseca.gov", false),
    ("gismaps.kingcounty.gov", false),
    ("usgs.gov", true),
    ("tiles.arcgis.com", false),
    ("ibasemaps-api.arcgis.com", false),
    ("server.arcgisonline.com", false),
    ("api.mapbox.com", false),
    ("maps.googleapis.com", false),
    ("tile.googleapis.com", false),
    ("wikimedia.org", true),
    ("upload.wikimedia.org", false),
    ("mapillary.com", true),
    ("scdn5.mapillary.com", false),
    ("api.simplyrets.com", false),
    ("simplyrets.com", true),
];

fn host_matches_domain(host: &str, domain: &str, allow_subdomains: bool) -> bool {
    let h = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if h == d {
        return true;
    }
    allow_subdomains
        && h.len() > d.len()
        && h.ends_with(&d)
        && h.as_bytes()[h.len() - d.len() - 1] == b'.'
}

pub fn is_blocked_portal_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return true;
    }
    for &dom in crate::registry::portals::COMPETITOR_PORTAL_DOMAINS
        .iter()
        .chain(BLOCKED_PORTAL_DOMAINS.iter())
    {
        if host_matches_domain(&h, dom, true) {
            return true;
        }
    }
    false
}

fn is_loopback_hostname(host: &str) -> bool {
    let h = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    h == "localhost" || h == "127.0.0.1" || h == "::1"
}

pub fn is_disallowed_ip(ip: IpAddr, allow_loopback: bool) -> bool {
    let is_loop = match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    };
    if is_loop {
        return !allow_loopback;
    }
    !crate::shared::is_public_routable_ip(ip)
}

fn configured_reso_host(env: &EnvConfig) -> Option<String> {
    let raw = env
        .get("RESO_API_URL")
        .map(String::from)
        .or_else(|| std::env::var("RESO_API_URL").ok())?;
    let parsed = Url::parse(raw.trim()).ok()?;
    let host = parsed.host_str()?.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || is_blocked_portal_host(&host) {
        return None;
    }
    Some(host)
}

/// Validates whether `raw_url` is permitted for remote media caching under the strict copyright
/// and SSRF allowlist. Returns the normalized URL string on success, or `None` if rejected.
pub fn validate_permitted_remote_media_url(raw_url: &str, env: &EnvConfig) -> Option<String> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 || trimmed.starts_with("data:") {
        return None;
    }
    let mut parsed = Url::parse(trimmed).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let raw_host = parsed.host_str()?.trim();
    let host_clean = raw_host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host_clean.is_empty() {
        return None;
    }

    // 1. Immediate hard rejection for consumer portals & metadata hostnames
    if is_blocked_portal_host(&host_clean)
        || host_clean == "metadata.google.internal"
        || host_clean == "metadata"
        || host_clean == "instance-data"
        || host_clean.ends_with(".internal")
        || host_clean.ends_with(".local")
    {
        return None;
    }

    let allow_loopback = allow_loopback_media_cache(env);

    // 2. Literal IP validation
    if let Ok(ip) = host_clean.parse::<IpAddr>() {
        if is_disallowed_ip(ip, allow_loopback) {
            return None;
        }
        // Only loopback IPs (when explicitly enabled) or exact configured RESO host IP may pass
        let is_loop = ip.is_loopback()
            || match ip {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
                IpAddr::V4(_) => false,
            };
        let reso_match = configured_reso_host(env).as_deref() == Some(host_clean.as_str());
        if !is_loop && !reso_match {
            return None;
        }
        if scheme == "http" && !is_loop {
            return None;
        }
        parsed.set_fragment(None);
        return Some(parsed.to_string());
    }

    // 3. `localhost` hostname
    if is_loopback_hostname(&host_clean) {
        if !allow_loopback {
            return None;
        }
        parsed.set_fragment(None);
        return Some(parsed.to_string());
    }

    // 4. Non-loopback hostnames must use HTTPS (unless matching an explicit loopback test RESO URL)
    let reso_host = configured_reso_host(env);
    let matches_open_allowlist = ALLOWED_OPEN_MEDIA_EXACT_OR_SUFFIX
        .iter()
        .any(|&(dom, sub)| host_matches_domain(&host_clean, dom, sub));
    let matches_reso = reso_host
        .as_deref()
        .is_some_and(|rh| host_matches_domain(&host_clean, rh, true));

    if !matches_open_allowlist && !matches_reso {
        return None;
    }
    if scheme != "https" {
        return None;
    }

    parsed.set_fragment(None);
    Some(parsed.to_string())
}

#[derive(Debug, Clone, Copy)]
struct SatelliteExportParams {
    lon1: f64,
    lat1: f64,
    lon2: f64,
    lat2: f64,
    width: u32,
    height: u32,
    zoom: u8,
}

fn parse_satellite_export_params(raw_query: &str) -> Result<SatelliteExportParams, GuardrailError> {
    let mut bbox_raw: Option<String> = None;
    let mut width: u32 = 1200;
    let mut height: u32 = 800;
    let mut zoom: u8 = 19;

    for (k, v) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match k.as_ref() {
            "bbox" => bbox_raw = Some(v.into_owned()),
            "w" | "width" => {
                if let Ok(n) = v.parse::<u32>() {
                    width = n.clamp(64, 2048);
                }
            }
            "h" | "height" => {
                if let Ok(n) = v.parse::<u32>() {
                    height = n.clamp(64, 2048);
                }
            }
            "zoom" | "z" => {
                if let Ok(n) = v.parse::<u8>() {
                    zoom = n.clamp(1, 21);
                }
            }
            _ => {}
        }
    }

    let bbox_str = bbox_raw.ok_or_else(|| {
        GuardrailError::new(
            "INVALID_SATELLITE_BBOX",
            "Missing satellite export bbox parameter.",
            400,
        )
    })?;
    let parts: Vec<f64> = bbox_str
        .split(',')
        .filter_map(|p| p.trim().parse::<f64>().ok())
        .collect();
    if parts.len() != 4 || parts.iter().any(|n| !n.is_finite()) {
        return Err(GuardrailError::new(
            "INVALID_SATELLITE_BBOX",
            "Satellite export bbox must contain 4 finite numbers.",
            400,
        ));
    }
    let (lon1, lat1, lon2, lat2) = (parts[0], parts[1], parts[2], parts[3]);
    let valid_range = (-180.0..=180.0).contains(&lon1)
        && (-180.0..=180.0).contains(&lon2)
        && (-85.0..=85.0).contains(&lat1)
        && (-85.0..=85.0).contains(&lat2)
        && lon2 > lon1
        && lat2 > lat1
        && (lon2 - lon1) <= 2.0
        && (lat2 - lat1) <= 2.0;
    if !valid_range {
        return Err(GuardrailError::new(
            "INVALID_SATELLITE_BBOX",
            "Satellite export bbox coordinates are out of range.",
            400,
        ));
    }

    Ok(SatelliteExportParams {
        lon1,
        lat1,
        lon2,
        lat2,
        width,
        height,
        zoom,
    })
}

fn strip_internal_satellite_export_query(raw: &str) -> Option<&str> {
    raw.strip_prefix("/api/media/satellite/export?")
        .or_else(|| raw.strip_prefix("/api/satellite-export?"))
}

fn is_internal_satellite_export_path(raw_url: &str) -> Option<String> {
    let trimmed = raw_url.trim();
    if trimmed.len() > 512
        || trimmed.contains("..")
        || trimmed.contains("//")
        || trimmed.contains('\\')
        || trimmed.contains('#')
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let query = strip_internal_satellite_export_query(trimmed)?;
    parse_satellite_export_params(query).ok()?;
    Some(trimmed.to_string())
}

fn normalize_permitted_media_item_url(raw_url: &str, env: &EnvConfig) -> Option<String> {
    is_internal_satellite_export_path(raw_url)
        .or_else(|| validate_permitted_remote_media_url(raw_url, env))
}

// =================================================================================================
// Image Magic Byte & Dimension Header Parser (JPEG SOF / PNG IHDR / WebP VP8 / AVIF) + 360° Detection
// =================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
}

impl ImageFormat {
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "webp" => Some(Self::Webp),
            "avif" => Some(Self::Avif),
            _ => None,
        }
    }
}

pub fn detect_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.len() >= 24 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        return Some(ImageFormat::Png);
    }
    if bytes.len() >= 16 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::Webp);
    }
    if bytes.len() >= 16 && &bytes[4..8] == b"ftyp" {
        let brand_window = &bytes[8..bytes.len().min(36)];
        if brand_window
            .windows(4)
            .any(|w| w == b"avif" || w == b"avis" || w == b"mif1")
            && (&bytes[8..12] == b"avif"
                || &bytes[8..12] == b"avis"
                || brand_window.windows(4).any(|w| w == b"avif" || w == b"avis"))
        {
            return Some(ImageFormat::Avif);
        }
    }
    None
}

fn parse_png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w > 0 && h > 0 && w <= 65_535 && h <= 65_535).then_some((w, h))
}

fn parse_jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 11 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos < bytes.len() {
        if bytes[pos] != 0xFF {
            pos += 1;
            continue;
        }
        while pos < bytes.len() && bytes[pos] == 0xFF {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let marker = bytes[pos];
        pos += 1;
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            continue;
        }
        if pos + 2 > bytes.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        if seg_len < 2 || pos + seg_len > bytes.len() {
            break;
        }
        // SOF markers (excluding 0xC4 DHT, 0xC8 JPG, 0xCC DAC)
        let is_sof = matches!(
            marker,
            0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD | 0xCE | 0xCF
        );
        if is_sof {
            if seg_len < 7 {
                return None;
            }
            let h = u16::from_be_bytes([bytes[pos + 3], bytes[pos + 4]]) as u32;
            let w = u16::from_be_bytes([bytes[pos + 5], bytes[pos + 6]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        pos += seg_len;
    }
    None
}

fn parse_webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 20 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_len = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let data_start = pos + 8;
        if data_start + chunk_len > bytes.len() {
            break;
        }
        let data = &bytes[data_start..data_start + chunk_len];
        if chunk_id == b"VP8X" && data.len() >= 10 {
            let w = 1 + u32::from_le_bytes([data[4], data[5], data[6], 0]);
            let h = 1 + u32::from_le_bytes([data[7], data[8], data[9], 0]);
            return (w > 0 && h > 0).then_some((w, h));
        } else if chunk_id == b"VP8L" && data.len() >= 5 && data[0] == 0x2F {
            let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            return (w > 0 && h > 0).then_some((w, h));
        } else if chunk_id == b"VP8 " && data.len() >= 10 {
            if data[3] == 0x9D && data[4] == 0x01 && data[5] == 0x2A {
                let w = (u16::from_le_bytes([data[6], data[7]]) & 0x3FFF) as u32;
                let h = (u16::from_le_bytes([data[8], data[9]]) & 0x3FFF) as u32;
                return (w > 0 && h > 0).then_some((w, h));
            }
        }
        pos = data_start + chunk_len + (chunk_len % 2);
    }
    None
}

pub fn parse_image_dimensions(bytes: &[u8], fmt: ImageFormat) -> Option<(u32, u32)> {
    match fmt {
        ImageFormat::Png => parse_png_dimensions(bytes),
        ImageFormat::Jpeg => parse_jpeg_dimensions(bytes),
        ImageFormat::Webp => parse_webp_dimensions(bytes),
        ImageFormat::Avif => None,
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() || haystack.len() < needle_lower.len() {
        return false;
    }
    let first = needle_lower[0];
    let max_start = haystack.len() - needle_lower.len();
    let mut i = 0usize;
    while i <= max_start {
        if haystack[i].to_ascii_lowercase() == first {
            let mut matched = true;
            for j in 1..needle_lower.len() {
                if haystack[i + j].to_ascii_lowercase() != needle_lower[j] {
                    matched = false;
                    break;
                }
            }
            if matched {
                return true;
            }
        }
        i += 1;
    }
    false
}

pub fn has_equirectangular_xmp(bytes: &[u8]) -> bool {
    contains_ascii_case_insensitive(bytes, b"gpano:projectiontype")
        || contains_ascii_case_insensitive(bytes, b"equirectangular")
}

pub fn detect_is_pano_360(
    explicit_flag: Option<bool>,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> bool {
    if let Some(flag) = explicit_flag {
        return flag;
    }
    if has_equirectangular_xmp(bytes) {
        return true;
    }
    if width >= 1600 && height > 0 {
        let ratio = (width as f64) / (height as f64);
        if (1.90..=2.10).contains(&ratio) {
            return true;
        }
    }
    false
}

// =================================================================================================
// Allowlisted Unbranded 3D Walkthrough Tour Validator
// =================================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ValidatedTour {
    pub id: String,
    pub vendor: String,
    pub embed_url: String,
    pub open_url: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub added_at: String,
}

static TOUR_TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_-]{3,120}$").expect("tour token regex"));

pub fn parse_allowlisted_tour_url(raw: &str, custom_label: Option<&str>) -> Option<ValidatedTour> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 600 {
        return None;
    }
    let normalized_input = if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else {
        trimmed.to_string()
    };
    let parsed = Url::parse(&normalized_input).ok()?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let host = parsed
        .host_str()?
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let host_no_www = host.strip_prefix("www.").unwrap_or(&host);
    if is_blocked_portal_host(host_no_www) {
        return None;
    }

    let segments: Vec<&str> = parsed
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let (vendor, embed_url, open_url) = if host_no_www == "my.matterport.com"
        || host_no_www == "matterport.com"
    {
        let q_m = parsed
            .query_pairs()
            .find(|(k, _)| k == "m")
            .map(|(_, v)| v.into_owned());
        let path_m = match segments.as_slice() {
            ["show", id, ..] | ["models", id, ..] | ["space", id, ..] => Some((*id).to_string()),
            ["discover", "space", id, ..] => Some((*id).to_string()),
            _ => None,
        };
        let tok = q_m.or(path_m)?;
        if tok.len() < 5 || tok.len() > 36 || !tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return None;
        }
        (
            "Matterport",
            format!("https://my.matterport.com/show/?m={tok}&play=1"),
            format!("https://my.matterport.com/show/?m={tok}"),
        )
    } else if host_no_www == "youriguide.com" {
        let tok = match segments.as_slice() {
            ["embed", id] => *id,
            [id] if *id != "embed" => *id,
            _ => return None,
        };
        if !TOUR_TOKEN_RE.is_match(tok) {
            return None;
        }
        (
            "iGUIDE",
            format!("https://youriguide.com/embed/{tok}?unbranded=1"),
            format!("https://youriguide.com/{tok}"),
        )
    } else if host_no_www == "kuula.co" {
        let (is_col, tok) = match segments.as_slice() {
            ["share", "collection", id] => (true, *id),
            ["share", id] if *id != "collection" => (false, *id),
            ["post", id] => (false, *id),
            _ => return None,
        };
        if !TOUR_TOKEN_RE.is_match(tok) {
            return None;
        }
        let base = if is_col {
            format!("https://kuula.co/share/collection/{tok}")
        } else {
            format!("https://kuula.co/share/{tok}")
        };
        (
            "Kuula",
            format!("{base}?logo=0&info=0&fs=1&vr=0&sd=1&thumbs=1"),
            base,
        )
    } else if host == "app.cloudpano.com" {
        let tok = match segments.as_slice() {
            ["tours", id] => *id,
            _ => return None,
        };
        if !TOUR_TOKEN_RE.is_match(tok) {
            return None;
        }
        let u = format!("https://app.cloudpano.com/tours/{tok}");
        ("CloudPano", u.clone(), u)
    } else if host_no_www == "ricoh360.com"
        || host == "tours.ricoh360.com"
        || host == "view.ricoh360.com"
    {
        let (sub_host, tok) = match (host.as_str(), segments.as_slice()) {
            ("tours.ricoh360.com", [id]) => ("tours.ricoh360.com", *id),
            ("tours.ricoh360.com", ["tours", id]) => ("tours.ricoh360.com", *id),
            ("view.ricoh360.com", [id]) => ("view.ricoh360.com", *id),
            ("view.ricoh360.com", ["tours", id]) => ("view.ricoh360.com", *id),
            (_, ["tours", id]) => ("www.ricoh360.com", *id),
            _ => return None,
        };
        if !TOUR_TOKEN_RE.is_match(tok) {
            return None;
        }
        let u = if sub_host == "www.ricoh360.com" {
            format!("https://www.ricoh360.com/tours/{tok}")
        } else {
            format!("https://{sub_host}/{tok}")
        };
        ("RICOH360", u.clone(), u)
    } else if host_no_www == "giraffe360.com" || host == "tour.giraffe360.com" {
        let tok = match segments.as_slice() {
            ["tours", id] | ["tour", id] => *id,
            [id] => *id,
            _ => return None,
        };
        if !TOUR_TOKEN_RE.is_match(tok) {
            return None;
        }
        let u = format!("https://tour.giraffe360.com/{tok}/");
        ("Giraffe360", u.clone(), u)
    } else if host == "players.cupix.com" {
        let tok = match segments.as_slice() {
            ["p", id] => *id,
            [id] => *id,
            _ => return None,
        };
        if !TOUR_TOKEN_RE.is_match(tok) {
            return None;
        }
        let u = format!("https://players.cupix.com/p/{tok}");
        ("Cupix", u.clone(), u)
    } else {
        return None;
    };

    let clean_lbl = custom_label
        .map(|s| scrub_media_text(s, 80))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{vendor} 3D Walkthrough"));

    let mut hasher = Sha256::new();
    hasher.update(embed_url.as_bytes());
    let id = format!("tour-{}", hex::encode(&hasher.finalize()[..6]));

    Some(ValidatedTour {
        id,
        vendor: vendor.to_string(),
        embed_url,
        open_url,
        label: clean_lbl,
        added_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

fn extract_allowlisted_tours_from_report(report: &Value) -> Vec<ValidatedTour> {
    let mut out: Vec<ValidatedTour> = Vec::new();
    let mut seen_embeds: HashSet<String> = HashSet::new();

    let mut push_val = |v: &Value, fallback_lbl: Option<&str>| {
        match v {
            Value::String(s) => {
                if let Some(mut t) = parse_allowlisted_tour_url(s, fallback_lbl) {
                    t.added_at = String::new();
                    if seen_embeds.insert(t.embed_url.clone()) {
                        out.push(t);
                    }
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        if let Some(mut t) = parse_allowlisted_tour_url(s, fallback_lbl) {
                            t.added_at = String::new();
                            if seen_embeds.insert(t.embed_url.clone()) {
                                out.push(t);
                            }
                        }
                    } else if let Some(obj) = item.as_object() {
                        let lbl = obj
                            .get("label")
                            .or_else(|| obj.get("name"))
                            .and_then(Value::as_str)
                            .or(fallback_lbl);
                        for k in ["embedUrl", "openUrl", "url", "href", "link"] {
                            if let Some(u) = obj.get(k).and_then(Value::as_str) {
                                if let Some(mut t) = parse_allowlisted_tour_url(u, lbl) {
                                    t.added_at = String::new();
                                    if seen_embeds.insert(t.embed_url.clone()) {
                                        out.push(t);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Value::Object(obj) => {
                let lbl = obj
                    .get("label")
                    .or_else(|| obj.get("name"))
                    .and_then(Value::as_str)
                    .or(fallback_lbl);
                for k in ["embedUrl", "openUrl", "url", "href", "link"] {
                    if let Some(u) = obj.get(k).and_then(Value::as_str) {
                        if let Some(mut t) = parse_allowlisted_tour_url(u, lbl) {
                            t.added_at = String::new();
                            if seen_embeds.insert(t.embed_url.clone()) {
                                out.push(t);
                            }
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    };

    for ptr in [
        "/modules/house3d/data/tours",
        "/house3d/tours",
        "/facts/virtualTours",
        "/facts/virtualTourUrl",
        "/facts/virtualTour",
        "/facts/tour3dUrl",
        "/facts/tourUrl",
        "/facts/mlsListing/virtualTours",
        "/facts/mlsListing/virtualTourUrl",
        "/facts/mlsListing/virtualTourUrlUnbranded",
        "/facts/mlsListing/virtualTourUrlBranded",
        "/facts/listing/virtualTours",
        "/facts/listing/virtualTourUrl",
    ] {
        if let Some(v) = report.pointer(ptr) {
            push_val(v, None);
        }
    }

    out
}

// =================================================================================================
// On-Disk Media Manifest Schema & Atomic Persistence
// =================================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PanoHotspot {
    pub yaw: f64,
    pub pitch: f64,
    pub target_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UploadedMediaEntry {
    pub id: String,
    pub filename: String,
    pub rel_path: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: usize,
    pub category: String,
    pub room_tag: String,
    pub caption: String,
    pub is_pano_360: bool,
    pub uploaded_at: String,
    #[serde(default)]
    pub hotspots: Vec<PanoHotspot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCacheEntry {
    pub id: String,
    pub remote_url: String,
    pub remote_thumb_url: String,
    #[serde(default)]
    pub cached_file: Option<String>,
    #[serde(default)]
    pub cached_thumb_file: Option<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub room_tag: String,
    #[serde(default)]
    pub caption: String,
    #[serde(default)]
    pub captured_at: String,
    #[serde(default)]
    pub source_name: String,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub is_historical_sale: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaManifestDisk {
    #[serde(default = "manifest_version")]
    pub version: u32,
    #[serde(default)]
    pub uploads: Vec<UploadedMediaEntry>,
    #[serde(default)]
    pub remote_items: HashMap<String, RemoteCacheEntry>,
    #[serde(default)]
    pub remote_order: Vec<String>,
    #[serde(default)]
    pub tours: Vec<ValidatedTour>,
}

fn manifest_version() -> u32 {
    1
}

async fn atomic_write_bytes(target_path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file_name = target_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("media.bin");
    let tmp_name = format!(".{file_name}.tmp.{}", Uuid::new_v4().simple());
    let tmp_path = target_path.with_file_name(tmp_name);
    {
        let mut f = tokio::fs::File::create(&tmp_path).await?;
        f.write_all(data).await?;
        f.flush().await?;
        let _ = f.sync_data().await;
    }
    if let Err(e) = tokio::fs::rename(&tmp_path, target_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e);
    }
    Ok(())
}

async fn load_manifest_disk(media_dir: &Path) -> MediaManifestDisk {
    let path = media_dir.join("manifest.json");
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return MediaManifestDisk {
            version: 1,
            ..Default::default()
        };
    };
    serde_json::from_str::<MediaManifestDisk>(&raw).unwrap_or(MediaManifestDisk {
        version: 1,
        ..Default::default()
    })
}

async fn save_manifest_disk(media_dir: &Path, manifest: &MediaManifestDisk) -> std::io::Result<()> {
    let path = media_dir.join("manifest.json");
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write_bytes(&path, &bytes).await
}

fn sanitize_media_id(raw: &str, fallback_seed: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(64)
        .collect();

    if !cleaned.is_empty() && SAFE_ID_RE.is_match(&cleaned) && !cleaned.starts_with("upload-") {
        cleaned
    } else {
        let mut h = Sha256::new();
        h.update(fallback_seed.as_bytes());
        format!("remote-{}", hex::encode(&h.finalize()[..6]))
    }
}

fn normalize_category(raw: Option<&str>, default_cat: &str) -> String {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "staging" | "interior" => "staging".to_string(),
        "exterior" => "exterior".to_string(),
        "street" => "street".to_string(),
        "aerial" => "aerial".to_string(),
        "parcel" => "parcel".to_string(),
        _ => default_cat.to_string(),
    }
}

fn is_auto_aerial_card_id(id: &str) -> bool {
    matches!(
        id,
        "usgs-aerial-rooftop"
            | "usgs-parcel-lot"
            | "usgs-aerial-neighborhood"
            | "scv-ortho-2025-rooftop"
            | "scv-ortho-2025-lot"
            | "scv-ortho-2025-neighborhood"
            | "kingco-ortho-2023-rooftop"
            | "kingco-ortho-2023-lot"
            | "kingco-ortho-2023-neighborhood"
            | "sat-arcgis-rooftop"
            | "sat-arcgis-lot"
            | "sat-arcgis-neighborhood"
            | "sat-mapbox-rooftop"
            | "sat-mapbox-lot"
            | "sat-mapbox-neighborhood"
            | "sat-google-rooftop"
            | "sat-google-lot"
            | "sat-google-neighborhood"
    )
}

async fn remove_cached_stem_files(media_dir: &Path, stems: &[String]) {
    for stem in stems {
        if !SAFE_ID_RE.is_match(stem) {
            continue;
        }
        for prefix in [stem.as_str(), &format!("{stem}_thumb")] {
            for ext in ["jpg", "png", "webp", "avif"] {
                let candidate = media_dir.join(format!("{prefix}.{ext}"));
                let _ = tokio::fs::remove_file(candidate).await;
            }
        }
    }
}

/// Synchronizes permitted remote items from `report["propertyMedia"]["items"]` into `manifest`,
/// dropping any consumer portal domain or unpermitted/SSRF host.
/// Returns `(changed, dropped_count, invalidated_disk_stems)`.
fn sync_report_items_into_manifest_collecting(
    report: &Value,
    env: &EnvConfig,
    manifest: &mut MediaManifestDisk,
) -> (bool, usize, Vec<String>) {
    let mut working_report = report.clone();
    if working_report.get("propertyMedia").is_none() {
        ensure_property_media_on_report_with_env(&mut working_report, env);
    } else {
        upgrade_outdated_aerial_cards_on_report(&mut working_report, env);
    }

    let raw_items = working_report
        .pointer("/propertyMedia/items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut changed = false;
    let mut dropped = 0usize;
    let mut invalidated_stems: Vec<String> = Vec::new();
    let mut new_order: Vec<String> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for (idx, it) in raw_items.iter().enumerate() {
        let Some(obj) = it.as_object() else {
            continue;
        };
        let raw_url = obj
            .get("url")
            .or_else(|| obj.get("thumbUrl"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let Some(permitted_url) = normalize_permitted_media_item_url(raw_url, env) else {
            dropped += 1;
            continue;
        };
        let raw_thumb = obj
            .get("thumbUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let permitted_thumb = normalize_permitted_media_item_url(raw_thumb, env)
            .unwrap_or_else(|| permitted_url.clone());

        let base_id = sanitize_media_id(
            obj.get("id")
                .and_then(Value::as_str)
                .unwrap_or(&format!("media-{idx}")),
            &permitted_url,
        );
        let id = if seen_ids.insert(base_id.clone()) {
            base_id
        } else {
            let mut h = Sha256::new();
            h.update(permitted_url.as_bytes());
            h.update(idx.to_le_bytes());
            let dedup = format!("{base_id}-{}", hex::encode(&h.finalize()[..3]));
            seen_ids.insert(dedup.clone());
            dedup
        };

        let category = normalize_category(obj.get("category").and_then(Value::as_str), "exterior");
        let room_tag = scrub_media_text(
            obj.get("roomTag")
                .and_then(Value::as_str)
                .unwrap_or("Property View"),
            80,
        );
        let caption = scrub_media_text(
            obj.get("caption")
                .and_then(Value::as_str)
                .unwrap_or("Verified property archive view"),
            200,
        );
        let captured_at = scrub_media_text(
            obj.get("capturedAt")
                .and_then(Value::as_str)
                .unwrap_or("Public Archive"),
            60,
        );
        let source_name = scrub_media_text(
            obj.get("sourceName")
                .and_then(Value::as_str)
                .unwrap_or("Open GIS Archive"),
            80,
        );
        let raw_source_url = obj
            .get("sourceUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let source_url = match Url::parse(raw_source_url) {
            Ok(u)
                if (u.scheme() == "https" || u.scheme() == "http")
                    && !is_blocked_portal_host(u.host_str().unwrap_or("")) =>
            {
                raw_source_url.to_string()
            }
            _ => String::new(),
        };
        let is_historical_sale = obj
            .get("isHistoricalSale")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match manifest.remote_items.get_mut(&id) {
            Some(existing) => {
                if existing.remote_url != permitted_url {
                    existing.remote_url = permitted_url;
                    existing.cached_file = None;
                    invalidated_stems.push(id.clone());
                    changed = true;
                }
                if existing.remote_thumb_url != permitted_thumb {
                    existing.remote_thumb_url = permitted_thumb;
                    existing.cached_thumb_file = None;
                    if !invalidated_stems.contains(&id) {
                        invalidated_stems.push(id.clone());
                    }
                    changed = true;
                }
                if existing.room_tag != room_tag
                    || existing.caption != caption
                    || existing.category != category
                {
                    existing.category = category;
                    existing.room_tag = room_tag;
                    existing.caption = caption;
                    existing.captured_at = captured_at;
                    existing.source_name = source_name;
                    existing.source_url = source_url;
                    existing.is_historical_sale = is_historical_sale;
                    changed = true;
                }
            }
            None => {
                manifest.remote_items.insert(
                    id.clone(),
                    RemoteCacheEntry {
                        id: id.clone(),
                        remote_url: permitted_url,
                        remote_thumb_url: permitted_thumb,
                        cached_file: None,
                        cached_thumb_file: None,
                        category,
                        room_tag,
                        caption,
                        captured_at,
                        source_name,
                        source_url,
                        is_historical_sale,
                    },
                );
                changed = true;
            }
        }
        new_order.push(id);
    }

    let order_set: HashSet<&str> = new_order.iter().map(String::as_str).collect();
    // Remove stale remote entries whose URLs are no longer permitted or superseded auto-aerial cards
    let stale_ids: Vec<String> = manifest
        .remote_items
        .iter()
        .filter_map(|(k, v)| {
            let unpermitted = normalize_permitted_media_item_url(&v.remote_url, env).is_none();
            let superseded_aerial = is_auto_aerial_card_id(k) && !order_set.contains(k.as_str());
            (unpermitted || superseded_aerial).then(|| k.clone())
        })
        .collect();
    for sid in stale_ids {
        manifest.remote_items.remove(&sid);
        invalidated_stems.push(sid);
        changed = true;
    }

    if manifest.remote_order != new_order {
        manifest.remote_order = new_order;
        changed = true;
    }

    (changed, dropped, invalidated_stems)
}

fn upload_entry_to_json(clean_slug: &str, up: &UploadedMediaEntry) -> Value {
    json!({
        "id": up.id,
        "url": format!("/api/media/{clean_slug}/file/{}", up.id),
        "thumbUrl": format!("/api/media/{clean_slug}/file/{}?variant=thumb", up.id),
        "category": up.category,
        "roomTag": up.room_tag,
        "caption": up.caption,
        "capturedAt": up.uploaded_at.get(0..10).unwrap_or(&up.uploaded_at),
        "sourceName": if up.is_pano_360 { "360° Walk-In Upload" } else { "Local Property Photo" },
        "sourceUrl": "",
        "isHistoricalSale": false,
        "isUploaded": true,
        "isPano360": up.is_pano_360,
        "width": up.width,
        "height": up.height,
        "mimeType": up.mime_type,
        "hotspots": up.hotspots,
    })
}

fn remote_entry_to_json(clean_slug: &str, rem: &RemoteCacheEntry) -> Value {
    json!({
        "id": rem.id,
        "url": format!("/api/media/{clean_slug}/file/{}", rem.id),
        "thumbUrl": format!("/api/media/{clean_slug}/file/{}?variant=thumb", rem.id),
        "category": rem.category,
        "roomTag": if rem.room_tag.is_empty() { "Property View" } else { &rem.room_tag },
        "caption": rem.caption,
        "capturedAt": rem.captured_at,
        "sourceName": if rem.source_name.is_empty() { "Open GIS Archive" } else { &rem.source_name },
        "sourceUrl": rem.source_url,
        "isHistoricalSale": rem.is_historical_sale,
        "isUploaded": false,
        "isPano360": false,
        "cached": rem.cached_file.is_some(),
        "hotspots": [],
    })
}

// =================================================================================================
// Polite SSRF-Safe Remote Image Fetcher
// =================================================================================================

async fn resolve_and_validate_addrs(
    parsed: &Url,
    allow_loopback: bool,
) -> Result<Vec<SocketAddr>, GuardrailError> {
    let host = parsed.host_str().ok_or_else(|| {
        GuardrailError::new("INVALID_UPSTREAM_URL", "Missing upstream host.", 400)
    })?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let clean_host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');

    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        if is_disallowed_ip(ip, allow_loopback) {
            return Err(GuardrailError::new(
                "MEDIA_UPSTREAM_FORBIDDEN",
                "Upstream media IP address is not permitted.",
                403,
            ));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let resolved: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::lookup_host((clean_host, port)),
    )
    .await
    .map_err(|_| GuardrailError::new("MEDIA_UPSTREAM_TIMEOUT", "Upstream DNS lookup timed out.", 502))?
    .map_err(|_| GuardrailError::new("MEDIA_UPSTREAM_UNREACHABLE", "Upstream DNS lookup failed.", 502))?
    .collect();

    if resolved.is_empty() {
        return Err(GuardrailError::new(
            "MEDIA_UPSTREAM_UNREACHABLE",
            "No upstream addresses resolved.",
            502,
        ));
    }

    for sa in &resolved {
        if is_disallowed_ip(sa.ip(), allow_loopback) {
            return Err(GuardrailError::new(
                "MEDIA_UPSTREAM_FORBIDDEN",
                "Upstream host resolved to a restricted network address.",
                403,
            ));
        }
    }
    Ok(resolved)
}

fn looks_like_html_or_challenge(headers: &HeaderMap, bytes: &[u8]) -> bool {
    if headers.contains_key("cf-mitigated") || headers.contains_key("x-px-blocked") {
        return true;
    }
    if let Some(ct) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let low = ct.to_ascii_lowercase();
        if low.contains("text/html") || low.contains("application/xhtml") {
            return true;
        }
    }
    let trimmed = bytes
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .take(32)
        .collect::<Vec<u8>>();
    trimmed.starts_with(b"<!DOCTYPE")
        || trimmed.starts_with(b"<!doctype")
        || trimmed.starts_with(b"<html")
        || trimmed.starts_with(b"<HTML")
}

async fn fetch_permitted_remote_image(
    raw_url: &str,
    env: &EnvConfig,
) -> Result<(Bytes, ImageFormat), GuardrailError> {
    let allow_loopback = allow_loopback_media_cache(env);
    let mut current_url = validate_permitted_remote_media_url(raw_url, env).ok_or_else(|| {
        GuardrailError::new(
            "MEDIA_DOMAIN_NOT_PERMITTED",
            "Remote media URL is outside the permitted open-imagery and licensed-feed allowlist.",
            403,
        )
    })?;

    for _hop in 0..=MAX_REDIRECT_HOPS {
        let parsed = Url::parse(&current_url).map_err(|_| {
            GuardrailError::new("INVALID_UPSTREAM_URL", "Invalid remote media URL.", 400)
        })?;
        let addrs = resolve_and_validate_addrs(&parsed, allow_loopback).await?;

        let host_str = parsed.host_str().unwrap_or("");
        let clean_host = host_str
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.');

        let mut builder = reqwest::Client::builder()
            .timeout(REMOTE_FETCH_TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(POLITE_MEDIA_CACHE_UA)
            .no_proxy();

        if clean_host.parse::<IpAddr>().is_err() {
            builder = builder.resolve_to_addrs(clean_host, &addrs);
        }

        let client = builder.build().map_err(|_| {
            GuardrailError::new("MEDIA_CLIENT_ERROR", "Failed to initialize HTTP client.", 500)
        })?;

        let mut resp = client
            .get(parsed.as_str())
            .header(header::ACCEPT, "image/avif,image/webp,image/png,image/jpeg,*/*;q=0.5")
            .send()
            .await
            .map_err(|_| {
                GuardrailError::new(
                    "MEDIA_UPSTREAM_UNAVAILABLE",
                    "Remote image host timed out or refused connection.",
                    502,
                )
            })?;

        let status = resp.status();
        if status.is_redirection() {
            let Some(loc) = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(GuardrailError::new(
                    "MEDIA_UPSTREAM_BAD_REDIRECT",
                    "Upstream redirect missing Location header.",
                    502,
                ));
            };
            let next_url = parsed
                .join(loc)
                .map_err(|_| {
                    GuardrailError::new(
                        "MEDIA_UPSTREAM_BAD_REDIRECT",
                        "Malformed upstream redirect URL.",
                        502,
                    )
                })?
                .to_string();
            current_url = validate_permitted_remote_media_url(&next_url, env).ok_or_else(|| {
                GuardrailError::new(
                    "MEDIA_REDIRECT_BLOCKED",
                    "Upstream redirect pointed outside the permitted media allowlist.",
                    403,
                )
            })?;
            continue;
        }

        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(GuardrailError::new(
                "MEDIA_UPSTREAM_SKIPPED",
                "Upstream image source declined polite fetch.",
                502,
            ));
        }
        if status != reqwest::StatusCode::OK {
            return Err(GuardrailError::new(
                "MEDIA_UPSTREAM_STATUS",
                format!("Upstream image host returned HTTP {}.", status.as_u16()),
                502,
            ));
        }

        if let Some(cl) = resp.content_length() {
            if cl > MAX_IMAGE_BYTES as u64 {
                return Err(GuardrailError::new(
                    "MEDIA_UPSTREAM_TOO_LARGE",
                    "Upstream image exceeds the 12 MB limit.",
                    413,
                ));
            }
        }

        let resp_headers = resp.headers().clone();
        if looks_like_html_or_challenge(&resp_headers, &[]) {
            return Err(GuardrailError::new(
                "MEDIA_UPSTREAM_HTML_SKIPPED",
                "Upstream returned HTML instead of image bytes.",
                502,
            ));
        }

        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        while let Some(chunk) = resp.chunk().await.map_err(|_| {
            GuardrailError::new(
                "MEDIA_UPSTREAM_READ_ERROR",
                "Interrupted while reading upstream image stream.",
                502,
            )
        })? {
            if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
                return Err(GuardrailError::new(
                    "MEDIA_UPSTREAM_TOO_LARGE",
                    "Upstream image stream exceeded the 12 MB limit.",
                    413,
                ));
            }
            buf.extend_from_slice(&chunk);
        }

        if looks_like_html_or_challenge(&resp_headers, &buf) {
            return Err(GuardrailError::new(
                "MEDIA_UPSTREAM_CHALLENGE_SKIPPED",
                "Upstream returned a non-image HTML payload.",
                502,
            ));
        }

        let Some(fmt) = detect_image_format(&buf) else {
            return Err(GuardrailError::new(
                "MEDIA_INVALID_MAGIC_BYTES",
                "Upstream payload failed JPEG/PNG/WebP/AVIF magic-byte verification.",
                502,
            ));
        };

        return Ok((Bytes::from(buf), fmt));
    }

    Err(GuardrailError::new(
        "MEDIA_TOO_MANY_REDIRECTS",
        "Upstream image exceeded maximum redirect hops.",
        502,
    ))
}

fn image_bytes_response(bytes: Vec<u8>, fmt: ImageFormat, cache_hit: bool) -> Response {
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(fmt.mime_type()),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800, immutable"),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-openestate-media-cache"),
        HeaderValue::from_static(if cache_hit { "HIT" } else { "MISS" }),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

fn is_safe_relative_media_path(rel: &str) -> bool {
    if rel.is_empty() || rel.len() > 180 || rel.starts_with('/') || rel.contains('\\') || rel.contains("..") {
        return false;
    }
    rel.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
}

async fn try_read_cached_disk_image(media_dir: &Path, rel_file: &str) -> Option<(Vec<u8>, ImageFormat)> {
    if !is_safe_relative_media_path(rel_file) {
        return None;
    }
    let full = media_dir.join(rel_file);
    let bytes = tokio::fs::read(&full).await.ok()?;
    let fmt = detect_image_format(&bytes).or_else(|| {
        full.extension()
            .and_then(|e| e.to_str())
            .and_then(ImageFormat::from_extension)
    })?;
    Some((bytes, fmt))
}

// =================================================================================================
// HTTP Route Handlers
// =================================================================================================

#[derive(Default, Deserialize)]
pub struct MediaFileQuery {
    #[serde(default)]
    pub variant: Option<String>,
}

/// `GET /api/media/:slug/manifest`
pub async fn handle_media_manifest_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    let clean_slug = match validate_slug_param(&slug) {
        Ok(s) => s,
        Err(ge) => return guardrail_error_to_response(ge),
    };
    let rec = match authorize_cached_report(
        &state,
        &clean_slug,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        false,
    )
    .await
    {
        Ok(auth) => auth.record,
        Err(resp) => return resp,
    };

    let canonical_slug = rec.slug.clone();
    let media_dir = resolve_media_dir(&state, &canonical_slug);
    let lock = slug_lock(&canonical_slug);
    let _guard = lock.lock().await;

    let mut manifest = load_manifest_disk(&media_dir).await;
    let (changed, dropped_count, invalidated_stems) =
        sync_report_items_into_manifest_collecting(&rec.report, &state.env, &mut manifest);
    if !invalidated_stems.is_empty() {
        remove_cached_stem_files(&media_dir, &invalidated_stems).await;
    }
    if changed {
        if let Err(e) = save_manifest_disk(&media_dir, &manifest).await {
            tracing::warn!(slug = %canonical_slug, error = %e, "Non-fatal warning saving media manifest");
        }
    }

    let mut items: Vec<Value> = Vec::with_capacity(manifest.uploads.len() + manifest.remote_order.len());
    let mut panos: Vec<Value> = Vec::new();
    for up in &manifest.uploads {
        let j = upload_entry_to_json(&canonical_slug, up);
        if up.is_pano_360 {
            panos.push(j.clone());
        }
        items.push(j);
    }
    for rid in &manifest.remote_order {
        if let Some(rem) = manifest.remote_items.get(rid) {
            items.push(remote_entry_to_json(&canonical_slug, rem));
        }
    }

    // Merge user-saved allowlisted tours with allowlisted tours recorded on the report
    let mut combined_tours: Vec<ValidatedTour> = Vec::new();
    let mut seen_embeds: HashSet<String> = HashSet::new();
    for t in &manifest.tours {
        if parse_allowlisted_tour_url(&t.open_url, Some(&t.label)).is_some()
            && seen_embeds.insert(t.embed_url.clone())
        {
            combined_tours.push(t.clone());
        }
    }
    for rt in extract_allowlisted_tours_from_report(&rec.report) {
        if seen_embeds.insert(rt.embed_url.clone()) {
            combined_tours.push(rt);
        }
    }

    let has_sale_photos = items.iter().any(|i| {
        i.get("isHistoricalSale").and_then(Value::as_bool) == Some(true)
            || i.get("isUploaded").and_then(Value::as_bool) == Some(true)
    });
    let raw_prov = rec
        .report
        .pointer("/propertyMedia/provenanceNote")
        .and_then(Value::as_str)
        .unwrap_or("Verified open ortho-aerial, parcel, and locally cached property imagery.");
    let provenance_note = scrub_media_text(raw_prov, 280);

    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": canonical_slug,
            "hasSalePhotos": has_sale_photos,
            "mediaStatus": if has_sale_photos { "sale_photos_found" } else { "exterior_archive_only" },
            "provenanceNote": provenance_note,
            "droppedPortalItems": dropped_count,
            "items": items,
            "panos": panos,
            "tours": combined_tours,
        }),
    )
}

/// `GET /api/media/:slug/file/:id`
pub async fn handle_media_file_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath((slug, raw_id)): AxumPath<(String, String)>,
    Query(q): Query<MediaFileQuery>,
) -> Response {
    let clean_slug = match validate_slug_param(&slug) {
        Ok(s) => s,
        Err(ge) => return guardrail_error_to_response(ge),
    };
    let clean_id = raw_id.trim();
    if !SAFE_ID_RE.is_match(clean_id) {
        return guardrail_error_to_response(GuardrailError::new(
            "INVALID_MEDIA_ID",
            "Invalid media asset identifier.",
            400,
        ));
    }

    let rec = match authorize_cached_report(
        &state,
        &clean_slug,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        false,
    )
    .await
    {
        Ok(auth) => auth.record,
        Err(resp) => return resp,
    };

    let canonical_slug = rec.slug.clone();
    let media_dir = resolve_media_dir(&state, &canonical_slug);
    let is_thumb = q
        .variant
        .as_deref()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("thumb"));

    // 1. Inspect manifest (auto-syncing from report if needed) under short lock
    let remote_target: Option<(String, bool)> = {
        let lock = slug_lock(&canonical_slug);
        let _guard = lock.lock().await;
        let mut manifest = load_manifest_disk(&media_dir).await;

        if let Some(up) = manifest.uploads.iter().find(|u| u.id == clean_id) {
            if let Some((bytes, fmt)) = try_read_cached_disk_image(&media_dir, &up.rel_path).await {
                return image_bytes_response(bytes, fmt, true);
            }
            return json_response(
                StatusCode::NOT_FOUND,
                json!({
                    "ok": false,
                    "code": "MEDIA_FILE_MISSING",
                    "error": "Uploaded media file was not found on disk.",
                }),
            );
        }

        let needs_sync = manifest.remote_items.get(clean_id).is_none_or(|rem| {
            [rem.remote_url.as_str(), rem.remote_thumb_url.as_str()]
                .into_iter()
                .any(|u| u.contains("USGSImageryTopo") || u.contains("USGSShadedReliefOnly"))
        });
        if needs_sync {
            let (changed, _, invalidated_stems) =
                sync_report_items_into_manifest_collecting(&rec.report, &state.env, &mut manifest);
            if !invalidated_stems.is_empty() {
                remove_cached_stem_files(&media_dir, &invalidated_stems).await;
            }
            if changed {
                let _ = save_manifest_disk(&media_dir, &manifest).await;
            }
        }

        if let Some(rem) = manifest.remote_items.get(clean_id) {
            let same_urls = rem.remote_thumb_url == rem.remote_url || rem.remote_thumb_url.is_empty();
            let candidate_cached = if is_thumb && !same_urls {
                rem.cached_thumb_file.clone()
            } else {
                rem.cached_file
                    .clone()
                    .or_else(|| same_urls.then(|| rem.cached_thumb_file.clone()).flatten())
            };
            if let Some(ref rel_file) = candidate_cached {
                if let Some((bytes, fmt)) = try_read_cached_disk_image(&media_dir, rel_file).await {
                    return image_bytes_response(bytes, fmt, true);
                }
            }
            // Also check deterministic filename pattern on disk (`<id>.<ext>`)
            let stem = if is_thumb && !same_urls {
                format!("{clean_id}_thumb")
            } else {
                clean_id.to_string()
            };
            for ext in ["jpg", "png", "webp", "avif"] {
                let rel = format!("{stem}.{ext}");
                if let Some((bytes, fmt)) = try_read_cached_disk_image(&media_dir, &rel).await {
                    return image_bytes_response(bytes, fmt, true);
                }
            }

            let target_url = if is_thumb && !same_urls {
                rem.remote_thumb_url.clone()
            } else {
                rem.remote_url.clone()
            };
            Some((target_url, is_thumb && !same_urls))
        } else {
            None
        }
    };

    let Some((remote_url, store_as_separate_thumb)) = remote_target else {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "code": "MEDIA_NOT_FOUND",
                "error": "Requested media item is not registered in this property manifest.",
            }),
        );
    };

    // 2. Cache miss: fetch internal satellite proxy export or permitted remote URL outside mutex
    let (bytes, fmt) = if let Some(sat_query) = strip_internal_satellite_export_query(&remote_url) {
        match fetch_satellite_export_for_query(
            &state.env,
            Some(&state.db),
            state.data_dir.as_deref(),
            sat_query,
        )
        .await
        {
            Ok((b, fmt, _hit)) => (b, fmt),
            Err(ge) => return guardrail_error_to_response(ge),
        }
    } else {
        match fetch_permitted_remote_image(&remote_url, &state.env).await {
            Ok(pair) => pair,
            Err(ge) => return guardrail_error_to_response(ge),
        }
    };

    let file_name = if store_as_separate_thumb {
        format!("{clean_id}_thumb.{}", fmt.extension())
    } else {
        format!("{clean_id}.{}", fmt.extension())
    };
    let disk_path = media_dir.join(&file_name);

    if let Err(e) = atomic_write_bytes(&disk_path, &bytes).await {
        tracing::warn!(slug = %canonical_slug, id = %clean_id, error = %e, "Failed writing cached media file");
    } else {
        let lock = slug_lock(&canonical_slug);
        let _guard = lock.lock().await;
        let mut manifest = load_manifest_disk(&media_dir).await;
        if let Some(rem) = manifest.remote_items.get_mut(clean_id) {
            if store_as_separate_thumb {
                rem.cached_thumb_file = Some(file_name);
            } else {
                rem.cached_file = Some(file_name.clone());
                if rem.remote_thumb_url == rem.remote_url {
                    rem.cached_thumb_file = Some(file_name);
                }
            }
            let _ = save_manifest_disk(&media_dir, &manifest).await;
        }
    }

    image_bytes_response(bytes.to_vec(), fmt, false)
}

fn decode_image_base64(raw: &str) -> Result<Vec<u8>, GuardrailError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(GuardrailError::new(
            "EMPTY_IMAGE_DATA",
            "Image base64 payload cannot be empty.",
            400,
        ));
    }
    let payload = match trimmed.find(";base64,") {
        Some(idx) if trimmed[..idx].starts_with("data:") => &trimmed[idx + 8..],
        _ => trimmed,
    };
    let compact: Vec<u8> = payload
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    // Pre-check approximate decoded length before allocating >12 MB
    if compact.len() > (MAX_IMAGE_BYTES / 3) * 4 + 64 {
        return Err(GuardrailError::new(
            "PAYLOAD_TOO_LARGE",
            "Decoded image exceeds the 12 MB upload limit.",
            413,
        ));
    }
    let decoded = B64_STD
        .decode(&compact)
        .or_else(|_| B64_NO_PAD.decode(&compact))
        .map_err(|_| {
            GuardrailError::new(
                "INVALID_BASE64",
                "Invalid base64 encoding in dataBase64 field.",
                400,
            )
        })?;
    if decoded.is_empty() {
        return Err(GuardrailError::new(
            "EMPTY_IMAGE_DATA",
            "Decoded image payload is empty.",
            400,
        ));
    }
    if decoded.len() > MAX_IMAGE_BYTES {
        return Err(GuardrailError::new(
            "PAYLOAD_TOO_LARGE",
            "Decoded image exceeds the 12 MB upload limit.",
            413,
        ));
    }
    Ok(decoded)
}

fn parse_bounded_json(
    raw_res: Result<Bytes, BytesRejection>,
    max_raw_bytes: usize,
    max_depth: usize,
    max_keys: usize,
) -> Result<Value, Response> {
    let bytes = raw_res.map_err(|rej| {
        if rej.status() == StatusCode::PAYLOAD_TOO_LARGE {
            map_guardrail(GuardrailError::new(
                "PAYLOAD_TOO_LARGE",
                "Request payload exceeds maximum allowed size.",
                413,
            ))
        } else {
            map_guardrail(GuardrailError::new(
                "MALFORMED_REQUEST",
                "Failed to read HTTP request body.",
                400,
            ))
        }
    })?;
    if bytes.len() > max_raw_bytes {
        return Err(map_guardrail(GuardrailError::new(
            "PAYLOAD_TOO_LARGE",
            "Request payload exceeds maximum allowed size.",
            413,
        )));
    }
    let val: Value = serde_json::from_slice(&bytes).map_err(|_| {
        map_guardrail(GuardrailError::new(
            "INVALID_JSON_BODY",
            "Malformed JSON request body.",
            400,
        ))
    })?;
    enforce_json_depth_and_size(&val, max_depth, max_keys).map_err(map_guardrail)?;
    Ok(val)
}

/// `POST /api/media/:slug/upload`
pub async fn handle_media_upload_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
    raw_body: Result<Bytes, BytesRejection>,
) -> ApiResult {
    verify_same_origin(&headers, req_host_str(&headers), &state.env).map_err(map_guardrail)?;
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;

    let body = parse_bounded_json(raw_body, MAX_UPLOAD_JSON_BYTES, 8, 64)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "filename",
            "dataBase64",
            "roomTag",
            "caption",
            "category",
            "isPano360",
        ],
    )
    .map_err(map_guardrail)?;

    let rec = authorize_cached_report(
        &state,
        &clean_slug,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        false,
    )
    .await?
    .record;
    let canonical_slug = rec.slug;

    let raw_b64 = body
        .get("dataBase64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            map_guardrail(GuardrailError::new(
                "MISSING_IMAGE_DATA",
                "Field `dataBase64` is required.",
                400,
            ))
        })?;
    let img_bytes = decode_image_base64(raw_b64).map_err(map_guardrail)?;

    let fmt = match detect_image_format(&img_bytes) {
        Some(f @ (ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Webp)) => f,
        _ => {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_IMAGE_FORMAT",
                "Uploaded file must be a valid JPEG, PNG, or WebP image.",
                400,
            )));
        }
    };
    let (width, height) = parse_image_dimensions(&img_bytes, fmt).ok_or_else(|| {
        map_guardrail(GuardrailError::new(
            "INVALID_IMAGE_HEADER",
            "Could not read valid pixel dimensions from image header.",
            400,
        ))
    })?;

    let explicit_pano = match body.get("isPano360") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        _ => {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_PARAMETER",
                "`isPano360` must be true, false, or null.",
                400,
            )));
        }
    };
    let is_pano_360 = detect_is_pano_360(explicit_pano, &img_bytes, width, height);

    let filename = scrub_media_text(
        body.get("filename")
            .and_then(Value::as_str)
            .unwrap_or("photo"),
        120,
    );
    let default_room = if is_pano_360 {
        "360° Room Panorama"
    } else {
        "Property Photo"
    };
    let room_tag = {
        let cleaned = scrub_media_text(
            body.get("roomTag").and_then(Value::as_str).unwrap_or(""),
            60,
        );
        if cleaned.is_empty() {
            default_room.to_string()
        } else {
            cleaned
        }
    };
    let caption = {
        let cleaned = scrub_media_text(
            body.get("caption").and_then(Value::as_str).unwrap_or(""),
            200,
        );
        if cleaned.is_empty() {
            if is_pano_360 {
                format!("{room_tag} — 360° Equirectangular Walk-In ({width}×{height})")
            } else {
                format!("{room_tag} ({width}×{height})")
            }
        } else {
            cleaned
        }
    };
    let default_cat = if is_pano_360 { "staging" } else { "exterior" };
    let category = normalize_category(body.get("category").and_then(Value::as_str), default_cat);

    let media_dir = resolve_media_dir(&state, &canonical_slug);
    let lock = slug_lock(&canonical_slug);
    let _guard = lock.lock().await;

    let mut manifest = load_manifest_disk(&media_dir).await;
    if manifest.uploads.len() >= MAX_UPLOADS_PER_SLUG {
        return Err(map_guardrail(GuardrailError::new(
            "UPLOAD_LIMIT_REACHED",
            format!("Maximum of {MAX_UPLOADS_PER_SLUG} uploaded images per property reached."),
            409,
        )));
    }

    let id = format!("upload-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let rel_path = format!("uploads/{id}.{}", fmt.extension());
    let full_path = media_dir.join(&rel_path);

    atomic_write_bytes(&full_path, &img_bytes)
        .await
        .map_err(|_| {
            map_guardrail(GuardrailError::new(
                "STORAGE_WRITE_FAILED",
                "Failed to persist uploaded image to disk.",
                500,
            ))
        })?;

    let entry = UploadedMediaEntry {
        id: id.clone(),
        filename: if filename.is_empty() {
            format!("{id}.{}", fmt.extension())
        } else {
            filename
        },
        rel_path,
        mime_type: fmt.mime_type().to_string(),
        width,
        height,
        size_bytes: img_bytes.len(),
        category,
        room_tag,
        caption,
        is_pano_360,
        uploaded_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        hotspots: Vec::new(),
    };

    let item_json = upload_entry_to_json(&canonical_slug, &entry);
    manifest.uploads.push(entry);
    save_manifest_disk(&media_dir, &manifest)
        .await
        .map_err(|_| {
            map_guardrail(GuardrailError::new(
                "MANIFEST_SAVE_FAILED",
                "Failed to update property media manifest.",
                500,
            ))
        })?;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": canonical_slug,
            "item": item_json,
        }),
    ))
}

/// `POST /api/media/:slug/hotspots`
pub async fn handle_media_hotspots_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
    raw_body: Result<Bytes, BytesRejection>,
) -> ApiResult {
    verify_same_origin(&headers, req_host_str(&headers), &state.env).map_err(map_guardrail)?;
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;

    let body = parse_bounded_json(raw_body, 64 * 1024, 8, 512)?;
    enforce_allowed_body_keys(&body, &["fromId", "hotspots"]).map_err(map_guardrail)?;

    let rec = authorize_cached_report(
        &state,
        &clean_slug,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        false,
    )
    .await?
    .record;
    let canonical_slug = rec.slug;

    let from_id = body
        .get("fromId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| SAFE_ID_RE.is_match(s))
        .ok_or_else(|| {
            map_guardrail(GuardrailError::new(
                "INVALID_FROM_ID",
                "A valid uploaded panorama `fromId` is required.",
                400,
            ))
        })?
        .to_string();

    let raw_hotspots = body
        .get("hotspots")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            map_guardrail(GuardrailError::new(
                "INVALID_HOTSPOTS",
                "`hotspots` must be an array.",
                400,
            ))
        })?;

    if raw_hotspots.len() > MAX_HOTSPOTS_PER_ROOM {
        return Err(map_guardrail(GuardrailError::new(
            "TOO_MANY_HOTSPOTS",
            format!("Maximum of {MAX_HOTSPOTS_PER_ROOM} hotspots per room exceeded."),
            400,
        )));
    }

    let mut validated_hotspots: Vec<PanoHotspot> = Vec::with_capacity(raw_hotspots.len());
    for hs in raw_hotspots {
        enforce_allowed_body_keys(hs, &["yaw", "pitch", "targetId", "label"])
            .map_err(map_guardrail)?;
        let yaw = hs
            .get("yaw")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && (-360.0..=360.0).contains(v))
            .ok_or_else(|| {
                map_guardrail(GuardrailError::new(
                    "INVALID_HOTSPOT_YAW",
                    "Hotspot `yaw` must be a finite angle in [-360, 360].",
                    400,
                ))
            })?;
        let pitch = hs
            .get("pitch")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && (-90.0..=90.0).contains(v))
            .ok_or_else(|| {
                map_guardrail(GuardrailError::new(
                    "INVALID_HOTSPOT_PITCH",
                    "Hotspot `pitch` must be a finite angle in [-90, 90].",
                    400,
                ))
            })?;
        let target_id = hs
            .get("targetId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| SAFE_ID_RE.is_match(s))
            .ok_or_else(|| {
                map_guardrail(GuardrailError::new(
                    "INVALID_HOTSPOT_TARGET",
                    "Hotspot `targetId` must be a valid room identifier.",
                    400,
                ))
            })?
            .to_string();
        let label = {
            let cleaned = scrub_media_text(
                hs.get("label").and_then(Value::as_str).unwrap_or(""),
                60,
            );
            if cleaned.is_empty() {
                "Next Room".to_string()
            } else {
                cleaned
            }
        };
        validated_hotspots.push(PanoHotspot {
            yaw,
            pitch,
            target_id,
            label,
        });
    }

    let media_dir = resolve_media_dir(&state, &canonical_slug);
    let lock = slug_lock(&canonical_slug);
    let _guard = lock.lock().await;

    let mut manifest = load_manifest_disk(&media_dir).await;
    let Some(up) = manifest.uploads.iter_mut().find(|u| u.id == from_id) else {
        return Err(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "code": "PANO_NOT_FOUND",
                "error": "Source panorama `fromId` was not found in manifest uploads.",
            }),
        ));
    };

    up.hotspots = validated_hotspots.clone();
    save_manifest_disk(&media_dir, &manifest)
        .await
        .map_err(|_| {
            map_guardrail(GuardrailError::new(
                "MANIFEST_SAVE_FAILED",
                "Failed to persist hotspot links.",
                500,
            ))
        })?;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": canonical_slug,
            "fromId": from_id,
            "hotspots": validated_hotspots,
        }),
    ))
}

/// `POST /api/media/:slug/tour`
pub async fn handle_media_tour_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
    raw_body: Result<Bytes, BytesRejection>,
) -> ApiResult {
    verify_same_origin(&headers, req_host_str(&headers), &state.env).map_err(map_guardrail)?;
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;

    let body = parse_bounded_json(raw_body, 32 * 1024, 6, 32)?;
    enforce_allowed_body_keys(&body, &["url", "tourUrl", "label", "clear"]).map_err(map_guardrail)?;

    let rec = authorize_cached_report(
        &state,
        &clean_slug,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        false,
    )
    .await?
    .record;
    let canonical_slug = rec.slug;

    let media_dir = resolve_media_dir(&state, &canonical_slug);
    let lock = slug_lock(&canonical_slug);
    let _guard = lock.lock().await;
    let mut manifest = load_manifest_disk(&media_dir).await;

    if body.get("clear").and_then(Value::as_bool) == Some(true) {
        manifest.tours.clear();
        save_manifest_disk(&media_dir, &manifest)
            .await
            .map_err(|_| {
                map_guardrail(GuardrailError::new(
                    "MANIFEST_SAVE_FAILED",
                    "Failed to update tour manifest.",
                    500,
                ))
            })?;
        return Ok(json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "slug": canonical_slug,
                "tours": manifest.tours,
            }),
        ));
    }

    let raw_url = body
        .get("url")
        .or_else(|| body.get("tourUrl"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw_url.is_empty() {
        return Err(map_guardrail(GuardrailError::new(
            "MISSING_TOUR_URL",
            "A 3D tour `url` is required.",
            400,
        )));
    }
    let custom_label = body.get("label").and_then(Value::as_str);

    let Some(validated) = parse_allowlisted_tour_url(raw_url, custom_label) else {
        return Err(map_guardrail(GuardrailError::new(
            "UNSUPPORTED_TOUR_HOST",
            "Unsupported tour URL. Supported 3D tour hosts: Matterport, iGUIDE, Kuula, CloudPano, RICOH360, Giraffe360, and Cupix.",
            400,
        )));
    };

    manifest
        .tours
        .retain(|existing| existing.embed_url != validated.embed_url);
    manifest.tours.insert(0, validated.clone());
    if manifest.tours.len() > MAX_TOURS_PER_SLUG {
        manifest.tours.truncate(MAX_TOURS_PER_SLUG);
    }

    save_manifest_disk(&media_dir, &manifest)
        .await
        .map_err(|_| {
            map_guardrail(GuardrailError::new(
                "MANIFEST_SAVE_FAILED",
                "Failed to save 3D tour URL in manifest.",
                500,
            ))
        })?;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": canonical_slug,
            "tour": validated,
            "tours": manifest.tours,
        }),
    ))
}

static SAT_HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(7))
        .connect_timeout(Duration::from_secs(3))
        .user_agent(POLITE_MEDIA_CACHE_UA)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

static SAT_TILE_CACHE: Lazy<Cache<String, (Bytes, ImageFormat)>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(4_096)
        .time_to_live(Duration::from_secs(86_400))
        .build()
});

static SAT_EXPORT_CACHE: Lazy<Cache<String, (Bytes, ImageFormat)>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(512)
        .time_to_live(Duration::from_secs(86_400))
        .build()
});

static GOOGLE_TILE_SESSION_CACHE: Lazy<Cache<String, String>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(16)
        .time_to_live(Duration::from_secs(21_600))
        .build()
});

#[derive(Default, Deserialize)]
pub struct SatelliteTileQuery {
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
}

fn non_empty_sat_key<'a>(env: &'a EnvConfig, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| env.get(k).map(str::trim).filter(|v| !v.is_empty()))
}

fn sat_key_fingerprint(env: &EnvConfig, kind: SatelliteProviderKind) -> String {
    let raw = match kind {
        SatelliteProviderKind::ArcGisCommercial => non_empty_sat_key(env, &["ARCGIS_API_KEY"]),
        SatelliteProviderKind::MapboxCommercial => {
            non_empty_sat_key(env, &["MAPBOX_ACCESS_TOKEN", "MAPBOX_API_KEY"])
        }
        SatelliteProviderKind::GoogleCommercial => non_empty_sat_key(env, &["GOOGLE_MAPS_API_KEY"]),
        _ => None,
    };
    match raw {
        Some(key) => {
            let mut hasher = Sha256::new();
            hasher.update(key.as_bytes());
            let hex_digest = hex::encode(hasher.finalize());
            hex_digest[..12].to_string()
        }
        None => "public".to_string(),
    }
}

fn tile_center_lat_lon(z: u8, x: u32, y: u32) -> (f64, f64) {
    let n = 2.0_f64.powi(i32::from(z));
    let lon = ((f64::from(x) + 0.5) / n) * 360.0 - 180.0;
    let lat_rad = (std::f64::consts::PI * (1.0 - 2.0 * (f64::from(y) + 0.5) / n))
        .sinh()
        .atan();
    (lat_rad.to_degrees(), lon)
}

fn lat_lon_to_tile_xy(lat: f64, lon: f64, z: u8) -> (u32, u32) {
    let n = 2.0_f64.powi(i32::from(z));
    let lat_clamped = lat.clamp(-85.0511, 85.0511).to_radians();
    let x = (((lon + 180.0) / 360.0) * n)
        .floor()
        .clamp(0.0, n - 1.0) as u32;
    let y = (((1.0 - (lat_clamped.tan() + 1.0 / lat_clamped.cos()).ln() / std::f64::consts::PI)
        / 2.0)
        * n)
        .floor()
        .clamp(0.0, n - 1.0) as u32;
    (x, y)
}

fn fit_mercator_zoom_for_bbox(params: &SatelliteExportParams, target_w: u32) -> u8 {
    let lon_span = (params.lon2 - params.lon1).max(1e-6);
    let ratio = (360.0 * f64::from(target_w)) / (lon_span * 256.0);
    if ratio <= 1.0 || !ratio.is_finite() {
        return params.zoom.clamp(1, 20);
    }
    (ratio.log2().floor() as i32).clamp(1, 20) as u8
}

fn satellite_image_response(bytes: Bytes, fmt: ImageFormat, cache_hit: bool) -> Response {
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(fmt.mime_type()),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("cross-origin"),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-openestate-satellite-cache"),
        HeaderValue::from_static(if cache_hit { "HIT" } else { "MISS" }),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

async fn fetch_bounded_satellite_image(
    env: &EnvConfig,
    url: &str,
) -> Result<(Bytes, ImageFormat), GuardrailError> {
    crate::metering::add_external_call_with_env(Some(env), url);
    let resp = SAT_HTTP_CLIENT
        .get(url)
        .header(header::ACCEPT, "image/avif,image/webp,image/png,image/jpeg,*/*;q=0.5")
        .send()
        .await
        .map_err(|_| {
            GuardrailError::new(
                "SATELLITE_UPSTREAM_UNAVAILABLE",
                "Satellite imagery upstream timed out or refused connection.",
                502,
            )
        })?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(GuardrailError::new(
            "SATELLITE_AUTH_REJECTED",
            format!("Satellite imagery upstream rejected credentials (HTTP {}).", status.as_u16()),
            502,
        ));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(GuardrailError::new(
            "SATELLITE_TILE_NOT_FOUND",
            "Satellite imagery tile was not found at requested zoom level.",
            404,
        ));
    }
    if !status.is_success() {
        return Err(GuardrailError::new(
            "SATELLITE_UPSTREAM_STATUS",
            format!("Satellite imagery upstream returned HTTP {}.", status.as_u16()),
            502,
        ));
    }
    if let Some(cl) = resp.content_length() {
        if cl > MAX_IMAGE_BYTES as u64 {
            return Err(GuardrailError::new(
                "MEDIA_IMAGE_TOO_LARGE",
                "Satellite imagery response exceeded maximum size.",
                413,
            ));
        }
    }
    let bytes = resp.bytes().await.map_err(|_| {
        GuardrailError::new(
            "SATELLITE_STREAM_ERROR",
            "Failed reading satellite imagery bytes.",
            502,
        )
    })?;
    crate::metering::add_bytes_downloaded(bytes.len() as u64);
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(GuardrailError::new(
            "MEDIA_INVALID_IMAGE_BYTES",
            "Satellite imagery response was empty or exceeded size limit.",
            502,
        ));
    }
    let fmt = detect_image_format(&bytes).ok_or_else(|| {
        GuardrailError::new(
            "MEDIA_INVALID_IMAGE_TYPE",
            "Satellite imagery upstream returned non-image content.",
            502,
        )
    })?;
    Ok((bytes, fmt))
}

async fn fetch_google_tile_session(api_key: &str, env: &EnvConfig) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    let cache_key = hex::encode(hasher.finalize());
    if let Some(tok) = GOOGLE_TILE_SESSION_CACHE.get(&cache_key).await {
        return Some(tok);
    }

    let mut parsed = Url::parse(GOOGLE_MAP_TILES_SESSION_URL).ok()?;
    parsed.query_pairs_mut().append_pair("key", api_key);

    crate::metering::add_external_call_with_env(Some(env), GOOGLE_MAP_TILES_SESSION_URL);
    let resp = SAT_HTTP_CLIENT
        .post(parsed)
        .json(&json!({
            "mapType": "satellite",
            "language": "en-US",
            "region": "US"
        }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let val: Value = resp.json().await.ok()?;
    let session = val
        .get("session")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    GOOGLE_TILE_SESSION_CACHE
        .insert(cache_key, session.clone())
        .await;
    Some(session)
}

async fn build_tile_url_for_provider(
    env: &EnvConfig,
    kind: SatelliteProviderKind,
    z: u8,
    y: u32,
    x: u32,
) -> Option<String> {
    match kind {
        SatelliteProviderKind::ArcGisCommercial => {
            let key = non_empty_sat_key(env, &["ARCGIS_API_KEY"])?;
            let mut u = Url::parse(&format!(
                "{ARCGIS_BASEMAPS_WORLD_IMAGERY_BASE_URL}/tile/{z}/{y}/{x}"
            ))
            .ok()?;
            u.query_pairs_mut().append_pair("token", key);
            Some(u.to_string())
        }
        SatelliteProviderKind::MapboxCommercial => {
            let key = non_empty_sat_key(env, &["MAPBOX_ACCESS_TOKEN", "MAPBOX_API_KEY"])?;
            let mut u =
                Url::parse(&format!("{MAPBOX_SATELLITE_TILES_BASE_URL}/{z}/{x}/{y}@2x")).ok()?;
            u.query_pairs_mut().append_pair("access_token", key);
            Some(u.to_string())
        }
        SatelliteProviderKind::GoogleCommercial => {
            let key = non_empty_sat_key(env, &["GOOGLE_MAPS_API_KEY"])?;
            let session = fetch_google_tile_session(key, env).await?;
            let mut u = Url::parse(&format!("{GOOGLE_MAP_TILES_2D_BASE_URL}/{z}/{x}/{y}")).ok()?;
            u.query_pairs_mut()
                .append_pair("session", &session)
                .append_pair("key", key);
            Some(u.to_string())
        }
        SatelliteProviderKind::SantaClaraSanJoseOrtho2025 => {
            Some(format!("{SCC_SJ_ORTHO_2025_TILE_URL}{z}/{y}/{x}"))
        }
        SatelliteProviderKind::KingCountyOrtho2023 => {
            Some(format!("{KINGCO_ORTHO_2023_TILE_URL}{z}/{y}/{x}"))
        }
        SatelliteProviderKind::UsgsImageryOnlyFallback => {
            Some(format!("{USGS_IMAGERY_ONLY_TILE_URL}{z}/{y}/{x}"))
        }
    }
}

async fn fetch_satellite_tile_bytes(
    env: &EnvConfig,
    resolved: &ResolvedSatelliteSource,
    z: u8,
    y: u32,
    x: u32,
    lat: f64,
    lon: f64,
) -> Result<(Bytes, ImageFormat), GuardrailError> {
    let mut allow_arcgis_fallback = false;
    if let Some(primary_url) = build_tile_url_for_provider(env, resolved.provider, z, y, x).await {
        match fetch_bounded_satellite_image(env, &primary_url).await {
            Ok(pair) => return Ok(pair),
            Err(ge) => allow_arcgis_fallback = ge.code != "SATELLITE_AUTH_REJECTED",
        }
    }

    if resolved.proxied {
        if let Some(region) = resolve_regional_ortho(lat, lon) {
            let regional_kind = match region {
                RegionalOrthoRegion::SantaClaraSanJose => {
                    SatelliteProviderKind::SantaClaraSanJoseOrtho2025
                }
                RegionalOrthoRegion::KingCountyWa => SatelliteProviderKind::KingCountyOrtho2023,
            };
            if let Some(reg_url) = build_tile_url_for_provider(env, regional_kind, z, y, x).await {
                if let Ok(pair) = fetch_bounded_satellite_image(env, &reg_url).await {
                    return Ok(pair);
                }
            }
        }
        if allow_arcgis_fallback && resolved.provider != SatelliteProviderKind::ArcGisCommercial {
            let esri_url = format!("{ARCGIS_WORLD_IMAGERY_TILE_FALLBACK_URL}{z}/{y}/{x}");
            if let Ok(pair) = fetch_bounded_satellite_image(env, &esri_url).await {
                return Ok(pair);
            }
        }
    }

    if z <= 16 && resolved.provider != SatelliteProviderKind::UsgsImageryOnlyFallback {
        if let Some(usgs_url) = build_tile_url_for_provider(
            env,
            SatelliteProviderKind::UsgsImageryOnlyFallback,
            z,
            y,
            x,
        )
        .await
        {
            return fetch_bounded_satellite_image(env, &usgs_url).await;
        }
    }

    Err(GuardrailError::new(
        "SATELLITE_TILE_UNAVAILABLE",
        "Requested satellite tile is unavailable from configured imagery sources.",
        502,
    ))
}

fn regional_export_url_for_coords(lat: f64, lon: f64, params: &SatelliteExportParams) -> Option<String> {
    let base = match resolve_regional_ortho(lat, lon)? {
        RegionalOrthoRegion::SantaClaraSanJose => SCC_SJ_ORTHO_2025_EXPORT_URL,
        RegionalOrthoRegion::KingCountyWa => KINGCO_ORTHO_2023_EXPORT_URL,
    };
    Some(format!(
        "{base}?bbox={:.6},{:.6},{:.6},{:.6}&bboxSR=4326&imageSR=3857&size={},{}&format=jpg&f=image",
        params.lon1, params.lat1, params.lon2, params.lat2, params.width, params.height
    ))
}

/// Scales `(width / 2, height / 2)` proportionally to fit inside `max_logical` so `@2x` / `scale=2`
/// static map APIs preserve exact bbox aspect ratio without squishing wide crops like `1600x1080`.
fn scaled_2x_static_dimensions(width: u32, height: u32, max_logical: u32) -> (u32, u32) {
    let hw = (width / 2).max(64);
    let hh = (height / 2).max(64);
    let peak = hw.max(hh);
    if peak <= max_logical {
        return (hw, hh);
    }
    let sw = ((u64::from(hw) * u64::from(max_logical)) / u64::from(peak))
        .max(64)
        .min(u64::from(max_logical)) as u32;
    let sh = ((u64::from(hh) * u64::from(max_logical)) / u64::from(peak))
        .max(64)
        .min(u64::from(max_logical)) as u32;
    (sw, sh)
}

async fn try_commercial_export_candidates(
    env: &EnvConfig,
    kind: SatelliteProviderKind,
    params: &SatelliteExportParams,
    lat: f64,
    lon: f64,
) -> Option<(Bytes, ImageFormat)> {
    match kind {
        SatelliteProviderKind::ArcGisCommercial => {
            let key = non_empty_sat_key(env, &["ARCGIS_API_KEY"])?;
            for export_base in [
                ARCGIS_BASEMAPS_WORLD_IMAGERY_BASE_URL,
                ARCGIS_WORLD_IMAGERY_EXPORT_FALLBACK_URL
                    .trim_end_matches("/export"),
            ] {
                let mut u = Url::parse(&format!("{export_base}/export")).ok()?;
                u.query_pairs_mut()
                    .append_pair(
                        "bbox",
                        &format!(
                            "{:.6},{:.6},{:.6},{:.6}",
                            params.lon1, params.lat1, params.lon2, params.lat2
                        ),
                    )
                    .append_pair("bboxSR", "4326")
                    .append_pair("imageSR", "3857")
                    .append_pair("size", &format!("{},{}", params.width, params.height))
                    .append_pair("format", "jpg")
                    .append_pair("f", "image")
                    .append_pair("token", key);
                if let Ok(pair) = fetch_bounded_satellite_image(env, u.as_str()).await {
                    return Some(pair);
                }
            }
            None
        }
        SatelliteProviderKind::MapboxCommercial => {
            let key = non_empty_sat_key(env, &["MAPBOX_ACCESS_TOKEN", "MAPBOX_API_KEY"])?;
            let (sw, sh) = scaled_2x_static_dimensions(params.width, params.height, 640);
            let url_base = format!(
                "{MAPBOX_SATELLITE_STATIC_BASE_URL}/[{:.6},{:.6},{:.6},{:.6}]/{}x{}@2x",
                params.lon1, params.lat1, params.lon2, params.lat2, sw, sh
            );
            let mut u = Url::parse(&url_base).ok()?;
            u.query_pairs_mut()
                .append_pair("attribution", "false")
                .append_pair("logo", "false")
                .append_pair("access_token", key);
            fetch_bounded_satellite_image(env, u.as_str()).await.ok()
        }
        SatelliteProviderKind::GoogleCommercial => {
            let key = non_empty_sat_key(env, &["GOOGLE_MAPS_API_KEY"])?;
            let (sw, sh) = scaled_2x_static_dimensions(params.width, params.height, 640);
            let z_fit = fit_mercator_zoom_for_bbox(params, sw);
            let mut u = Url::parse(GOOGLE_STATIC_MAPS_URL).ok()?;
            u.query_pairs_mut()
                .append_pair("center", &format!("{lat:.6},{lon:.6}"))
                .append_pair("zoom", &z_fit.to_string())
                .append_pair("size", &format!("{sw}x{sh}"))
                .append_pair("scale", "2")
                .append_pair("maptype", "satellite")
                .append_pair("format", "jpg")
                .append_pair("key", key);
            fetch_bounded_satellite_image(env, u.as_str()).await.ok()
        }
        _ => None,
    }
}

const IMAGERY_TILE_TTL_SECS: u64 = 365 * 86_400;

fn sat_db_namespace(db: &DbStore) -> String {
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let file = pool
                .connect_options()
                .get_filename()
                .to_string_lossy()
                .into_owned();
            if file == ":memory:" || file.is_empty() {
                format!("sqlite:mem:{:p}", db)
            } else {
                format!("sqlite:{file}")
            }
        }
        DbBackend::MySql { .. } => format!("mysql:{}", db.provider),
    }
}

fn tile_bbox_lat_lon(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = 2.0_f64.powi(i32::from(z));
    let lon_at = |xf: f64| (xf / n) * 360.0 - 180.0;
    let lat_at = |yf: f64| {
        (std::f64::consts::PI * (1.0 - 2.0 * yf / n))
            .sinh()
            .atan()
            .to_degrees()
    };
    let west = lon_at(f64::from(x));
    let east = lon_at(f64::from(x.saturating_add(1)));
    let north = lat_at(f64::from(y));
    let south = lat_at(f64::from(y.saturating_add(1)));
    (south, west, north, east)
}

fn sat_tile_params_hash(tile_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tile_key.as_bytes());
    hex::encode(hasher.finalize())
}

fn sat_tile_scope_key(lat: f64, lon: f64) -> String {
    encode_geohash(lat, lon, 7).unwrap_or_else(|| format!("coord:{lat:.5},{lon:.5}"))
}

async fn load_satellite_tile_shared_record(
    db: &DbStore,
    data_dir: Option<&Path>,
    tile_key: &str,
    lat: f64,
    lon: f64,
) -> Option<(Bytes, ImageFormat)> {
    let scope_key = sat_tile_scope_key(lat, lon);
    let params_hash = sat_tile_params_hash(tile_key);
    let (_row, tile_ref) = get_shared_dataset_proto::<ImageryLidarTileRefProto>(
        None,
        db,
        dataset_ids::IMAGERY_LIDAR_TILE,
        ShareScope::Geohash7,
        &scope_key,
        &params_hash,
    )
    .await
    .ok()??;
    let blob_hash = tile_ref.blob_hash.as_deref()?.trim();
    if blob_hash.is_empty() {
        return None;
    }
    let (_blob_row, raw) = get_shared_blob(db, data_dir, blob_hash).await.ok()??;
    let fmt = detect_image_format(&raw)?;
    Some((Bytes::from(raw), fmt))
}

#[allow(clippy::too_many_arguments)]
async fn persist_satellite_tile_shared_record(
    db: &DbStore,
    data_dir: Option<&Path>,
    tile_key: &str,
    asset_kind: &str,
    resolved: &ResolvedSatelliteSource,
    lat: f64,
    lon: f64,
    bbox: (f64, f64, f64, f64),
    bytes: &[u8],
    fmt: ImageFormat,
) {
    let geo = GeoIndexColumns {
        geohash5: encode_geohash(lat, lon, 5),
        geohash6: encode_geohash(lat, lon, 6),
        geohash7: encode_geohash(lat, lon, 7),
        bbox_south: Some(bbox.0),
        bbox_west: Some(bbox.1),
        bbox_north: Some(bbox.2),
        bbox_east: Some(bbox.3),
        ..Default::default()
    };
    let blob = match put_shared_blob(
        db,
        data_dir,
        &PutSharedBlobInput {
            dataset_id: dataset_ids::IMAGERY_LIDAR_TILE.to_string(),
            media_type: fmt.mime_type().to_string(),
            bytes: bytes.to_vec(),
            tile_key: Some(tile_key.to_string()),
            geo: geo.clone(),
            source_url: Some(resolved.source_url.to_string()),
        },
    )
    .await
    {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(error = %err, tile_key, "media_api: failed storing satellite blob");
            return;
        }
    };

    let resolution_m = match resolved.provider {
        SatelliteProviderKind::SantaClaraSanJoseOrtho2025 => Some(0.10),
        SatelliteProviderKind::KingCountyOrtho2023 => Some(0.15),
        SatelliteProviderKind::UsgsImageryOnlyFallback => Some(1.0),
        _ => Some(0.30),
    };
    let capture_year = match resolved.provider {
        SatelliteProviderKind::SantaClaraSanJoseOrtho2025 => Some(2025),
        SatelliteProviderKind::KingCountyOrtho2023 => Some(2023),
        _ => None,
    };
    let fetched_at = Utc::now().to_rfc3339();
    let tile_ref = ImageryLidarTileRefProto {
        tile_key: tile_key.to_string(),
        asset_kind: asset_kind.to_string(),
        provider: resolved.provider_id.to_string(),
        bbox: Some(GeoBoundingBoxProto {
            south: bbox.0,
            west: bbox.1,
            north: bbox.2,
            east: bbox.3,
        }),
        resolution_m,
        capture_year,
        source_url: resolved.source_url.to_string(),
        blob_hash: Some(blob.content_hash.clone()),
        byte_size: Some(bytes.len() as u64),
        fetched_at_rfc3339: fetched_at.clone(),
    };
    let payload_json = json!({
        "tileKey": tile_ref.tile_key,
        "assetKind": tile_ref.asset_kind,
        "source": tile_ref.provider,
        "resolutionM": tile_ref.resolution_m,
        "captureYear": tile_ref.capture_year,
        "blobHash": tile_ref.blob_hash,
        "byteSize": tile_ref.byte_size,
        "fetchedAt": fetched_at,
    })
    .to_string();
    let provenance_json = json!({
        "sourceName": resolved.source_name,
        "sourceUrl": resolved.source_url,
        "attribution": resolved.attribution,
    })
    .to_string();
    let scope_key = sat_tile_scope_key(lat, lon);
    let params_hash = sat_tile_params_hash(tile_key);
    if let Err(err) = put_shared_dataset_proto(
        None,
        db,
        dataset_ids::IMAGERY_LIDAR_TILE,
        ShareScope::Geohash7,
        &scope_key,
        &params_hash,
        geo,
        &tile_ref,
        Some(payload_json),
        Some(provenance_json),
        Some(blob.content_hash),
        IMAGERY_TILE_TTL_SECS,
    )
    .await
    {
        tracing::warn!(error = %err, tile_key, "media_api: failed indexing shared satellite tile row");
    }
}

/// Invalidates in-memory satellite L1 caches (forcing DB-backed `shared_data_records` /
/// `shared_blobs` lookup on the next request).
pub async fn clear_satellite_memory_caches() {
    SAT_TILE_CACHE.invalidate_all();
    SAT_EXPORT_CACHE.invalidate_all();
    SAT_TILE_CACHE.run_pending_tasks().await;
    SAT_EXPORT_CACHE.run_pending_tasks().await;
}

async fn fetch_satellite_export_for_query(
    env: &EnvConfig,
    db: Option<&DbStore>,
    data_dir: Option<&Path>,
    raw_query: &str,
) -> Result<(Bytes, ImageFormat, bool), GuardrailError> {
    let params = parse_satellite_export_params(raw_query)?;
    let lat = (params.lat1 + params.lat2) * 0.5;
    let lon = (params.lon1 + params.lon2) * 0.5;
    let resolved = resolve_satellite_source(env, Some((lat, lon)));
    let key_fp = sat_key_fingerprint(env, resolved.provider);
    let tile_key = format!(
        "{}:{key_fp}:{:.6},{:.6},{:.6},{:.6}:{}x{}:z{}",
        resolved.provider_id,
        params.lon1,
        params.lat1,
        params.lon2,
        params.lat2,
        params.width,
        params.height,
        params.zoom
    );
    let l1_key = format!(
        "{}:{tile_key}",
        db.map(sat_db_namespace).unwrap_or_else(|| "mem_only".to_string())
    );
    if let Some((b, fmt)) = SAT_EXPORT_CACHE.get(&l1_key).await {
        crate::metering::mark_cached();
        return Ok((b, fmt, true));
    }
    if let Some(store) = db {
        if let Some((b, fmt)) =
            load_satellite_tile_shared_record(store, data_dir, &tile_key, lat, lon).await
        {
            SAT_EXPORT_CACHE.insert(l1_key, (b.clone(), fmt)).await;
            crate::metering::mark_cached();
            return Ok((b, fmt, true));
        }
    }

    let result: Result<(Bytes, ImageFormat), GuardrailError> = async {
        if resolved.proxied {
            if let Some(pair) =
                try_commercial_export_candidates(env, resolved.provider, &params, lat, lon).await
            {
                return Ok(pair);
            }
            if let Some(reg_export) = regional_export_url_for_coords(lat, lon, &params) {
                if let Ok(pair) = fetch_bounded_satellite_image(env, &reg_export).await {
                    return Ok(pair);
                }
            }
            let z_top = params.zoom.min(resolved.max_native_zoom).min(21);
            for z_try in [z_top, 19, 18, 17] {
                if z_try > z_top {
                    continue;
                }
                let (tx, ty) = lat_lon_to_tile_xy(lat, lon, z_try);
                if let Some(tile_url) =
                    build_tile_url_for_provider(env, resolved.provider, z_try, ty, tx).await
                {
                    if let Ok(pair) = fetch_bounded_satellite_image(env, &tile_url).await {
                        return Ok(pair);
                    }
                }
            }
        } else if let Some(reg_export) = regional_export_url_for_coords(lat, lon, &params) {
            if let Ok(pair) = fetch_bounded_satellite_image(env, &reg_export).await {
                return Ok(pair);
            }
        }

        let usgs_export = format!(
            "{USGS_IMAGERY_ONLY_EXPORT_URL}?bbox={:.6},{:.6},{:.6},{:.6}&bboxSR=4326&imageSR=3857&size={},{}&format=jpg&f=image",
            params.lon1, params.lat1, params.lon2, params.lat2, params.width, params.height
        );
        fetch_bounded_satellite_image(env, &usgs_export).await
    }
    .await;

    let (bytes, fmt) = result?;
    if let Some(store) = db {
        let bbox = (params.lat1, params.lon1, params.lat2, params.lon2);
        persist_satellite_tile_shared_record(
            store,
            data_dir,
            &tile_key,
            "ortho_export",
            &resolved,
            lat,
            lon,
            bbox,
            &bytes,
            fmt,
        )
        .await;
    }
    SAT_EXPORT_CACHE
        .insert(l1_key, (bytes.clone(), fmt))
        .await;
    Ok((bytes, fmt, false))
}

/// `GET /api/media/satellite/tile/:z/:y/:x` — server-side proxy + DB/L1 cache for commercial
/// high-resolution satellite tiles (never leaking API keys) with regional `.gov` / USGS fallback.
pub async fn handle_satellite_tile_get(
    State(state): State<AppState>,
    AxumPath((z, y, x)): AxumPath<(u8, u32, u32)>,
    Query(q): Query<SatelliteTileQuery>,
) -> Response {
    if z > 21 {
        crate::metering::mark_failed();
        return guardrail_error_to_response(GuardrailError::new(
            "INVALID_TILE_COORDS",
            "Satellite tile zoom exceeds supported range.",
            400,
        ));
    }
    let max_coord = 1u32 << z;
    if x >= max_coord || y >= max_coord {
        crate::metering::mark_failed();
        return guardrail_error_to_response(GuardrailError::new(
            "INVALID_TILE_COORDS",
            "Satellite tile x/y out of range for zoom level.",
            400,
        ));
    }

    let (lat, lon) = match (q.lat, q.lon) {
        (Some(la), Some(lo))
            if la.is_finite() && lo.is_finite() && (-90.0..=90.0).contains(&la) && (-180.0..=180.0).contains(&lo) =>
        {
            (la, lo)
        }
        _ => tile_center_lat_lon(z, x, y),
    };

    let resolved = resolve_satellite_source(&state.env, Some((lat, lon)));
    let key_fp = sat_key_fingerprint(&state.env, resolved.provider);
    let tile_key = format!("{}:{key_fp}:{z}:{y}:{x}", resolved.provider_id);
    let l1_key = format!("{}:{tile_key}", sat_db_namespace(&state.db));
    if let Some((bytes, fmt)) = SAT_TILE_CACHE.get(&l1_key).await {
        crate::metering::mark_cached();
        return satellite_image_response(bytes, fmt, true);
    }
    if let Some((bytes, fmt)) = load_satellite_tile_shared_record(
        &state.db,
        state.data_dir.as_deref(),
        &tile_key,
        lat,
        lon,
    )
    .await
    {
        SAT_TILE_CACHE.insert(l1_key, (bytes.clone(), fmt)).await;
        crate::metering::mark_cached();
        return satellite_image_response(bytes, fmt, true);
    }

    match fetch_satellite_tile_bytes(&state.env, &resolved, z, y, x, lat, lon).await {
        Ok((bytes, fmt)) => {
            let bbox = tile_bbox_lat_lon(z, x, y);
            persist_satellite_tile_shared_record(
                &state.db,
                state.data_dir.as_deref(),
                &tile_key,
                "ortho_tile",
                &resolved,
                lat,
                lon,
                bbox,
                &bytes,
                fmt,
            )
            .await;
            SAT_TILE_CACHE
                .insert(l1_key, (bytes.clone(), fmt))
                .await;
            satellite_image_response(bytes, fmt, false)
        }
        Err(ge) => {
            crate::metering::mark_failed();
            guardrail_error_to_response(ge)
        }
    }
}

/// `GET /api/media/satellite/export` — server-side proxy + DB/L1 cache for crisp bounding-box aerial
/// exports when commercial keys are configured, falling back cleanly to regional `.gov` / USGS.
pub async fn handle_satellite_export_get(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let query_str = raw_query.as_deref().unwrap_or("");
    match fetch_satellite_export_for_query(
        &state.env,
        Some(&state.db),
        state.data_dir.as_deref(),
        query_str,
    )
    .await
    {
        Ok((bytes, fmt, hit)) => satellite_image_response(bytes, fmt, hit),
        Err(ge) => {
            crate::metering::mark_failed();
            guardrail_error_to_response(ge)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tour_allowlist_accepts_supported_hosts_and_rejects_portals() {
        assert!(parse_allowlisted_tour_url("https://my.matterport.com/show/?m=SxQL3iGyoDo", None).is_some());
        assert!(parse_allowlisted_tour_url("https://youriguide.com/1234_main_st_ca", None).is_some());
        assert!(parse_allowlisted_tour_url("https://kuula.co/share/collection/7kXYZ", None).is_some());
        assert!(parse_allowlisted_tour_url("https://app.cloudpano.com/tours/t12345", None).is_some());
        assert!(parse_allowlisted_tour_url("https://www.ricoh360.com/tours/abc_123", None).is_some());
        assert!(parse_allowlisted_tour_url("https://tour.giraffe360.com/home99/", None).is_some());
        assert!(parse_allowlisted_tour_url("https://players.cupix.com/p/cpx789", None).is_some());

        let bad_z = format!("https://www.{}.com/view-3d-home/12345678-1234-1234-1234-1234567890ab", "zillow");
        let bad_r = format!("https://www.{}.com/CA/Campbell/home/123", "redfin");
        assert!(parse_allowlisted_tour_url(&bad_z, None).is_none());
        assert!(parse_allowlisted_tour_url(&bad_r, None).is_none());
        assert!(parse_allowlisted_tour_url("https://evil.example.com/show/?m=SxQL3iGyoDo", None).is_none());
    }

    #[test]
    fn ssrf_and_portal_url_filter() {
        let env_off = EnvConfig::from_map(HashMap::from([(
            "MEDIA_CACHE_ALLOW_LOOPBACK".to_string(),
            "0".to_string(),
        )]));
        assert!(validate_permitted_remote_media_url(
            "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?f=image",
            &env_off
        )
        .is_some());
        assert!(validate_permitted_remote_media_url(
            "https://upload.wikimedia.org/wikipedia/commons/a/b/test.jpg",
            &env_off
        )
        .is_some());
        assert!(validate_permitted_remote_media_url("http://127.0.0.1:9999/img.jpg", &env_off).is_none());
        assert!(validate_permitted_remote_media_url("http://169.254.169.254/latest/meta-data/", &env_off).is_none());
        assert!(validate_permitted_remote_media_url("https://10.0.0.5/photo.jpg", &env_off).is_none());
        assert!(validate_permitted_remote_media_url("https://192.168.1.10/photo.jpg", &env_off).is_none());

        let env_on = EnvConfig::from_map(HashMap::from([(
            "MEDIA_CACHE_ALLOW_LOOPBACK".to_string(),
            "1".to_string(),
        )]));
        assert!(validate_permitted_remote_media_url("http://127.0.0.1:9999/img.jpg", &env_on).is_some());
        // Private / metadata IPs stay blocked even when loopback test flag is enabled
        assert!(validate_permitted_remote_media_url("http://169.254.169.254/img.jpg", &env_on).is_none());
        assert!(validate_permitted_remote_media_url("http://10.0.0.5/img.jpg", &env_on).is_none());

        // High-res agency & commercial proxy domains are permitted
        assert!(validate_permitted_remote_media_url(
            "https://geo.sanjoseca.gov/server/rest/services/Imagery/DPW_ImageryCached2025/MapServer/export?f=image",
            &env_off
        )
        .is_some());
        assert!(validate_permitted_remote_media_url(
            "https://gismaps.kingcounty.gov/arcgis/rest/services/BaseMaps/KingCo_Aerial_2023/MapServer/export?f=image",
            &env_off
        )
        .is_some());
    }

    #[test]
    fn satellite_export_path_validation_and_manifest_upgrade() {
        assert!(is_internal_satellite_export_path(
            "/api/media/satellite/export?bbox=-121.962000,37.282000,-121.961000,37.283000&w=1600&h=1080&zoom=20"
        )
        .is_some());
        assert!(is_internal_satellite_export_path(
            "/api/satellite-export?bbox=-121.962000,37.282000,-121.961000,37.283000&w=1600&h=1080&zoom=20"
        )
        .is_some());
        assert!(is_internal_satellite_export_path("/api/media/satellite/export?bbox=bad").is_none());
        assert!(is_internal_satellite_export_path("/api/media/satellite/export?bbox=1,2,3,4/../secret").is_none());
        assert!(is_internal_satellite_export_path("/api/media/satellite/export?bbox=-120,30,-110,40").is_none());

        // Manifest sync upgrades old LOD-16 USGS default card and collects stale disk stems
        let empty_env = EnvConfig::from_map(HashMap::new());
        let mut manifest = MediaManifestDisk::default();
        manifest.remote_items.insert(
            "usgs-aerial-rooftop".to_string(),
            RemoteCacheEntry {
                id: "usgs-aerial-rooftop".to_string(),
                remote_url: "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?size=960,640".to_string(),
                remote_thumb_url: "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?size=480,320".to_string(),
                cached_file: Some("usgs-aerial-rooftop.jpg".to_string()),
                cached_thumb_file: None,
                category: "aerial".to_string(),
                room_tag: "House Rooftop".to_string(),
                caption: "Old aerial".to_string(),
                captured_at: "USGS".to_string(),
                source_name: "USGS".to_string(),
                source_url: "https://apps.nationalmap.gov/viewer/".to_string(),
                is_historical_sale: false,
            },
        );
        let report = json!({
            "geo": { "lat": 37.282354, "lon": -121.961661 },
            "meta": { "displayAddress": "200 Synthparcelbeta Ave, Campbell, CA 95008" },
            "propertyMedia": {
                "items": [
                    {
                        "id": "usgs-aerial-rooftop",
                        "url": "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?size=960,640",
                        "thumbUrl": "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?size=480,320",
                        "category": "aerial",
                        "roomTag": "House Rooftop",
                        "caption": "Old aerial",
                        "capturedAt": "USGS",
                        "sourceName": "USGS",
                        "sourceUrl": "https://apps.nationalmap.gov/viewer/",
                        "isHistoricalSale": false
                    }
                ]
            }
        });
        let (changed, dropped, invalidated) =
            sync_report_items_into_manifest_collecting(&report, &empty_env, &mut manifest);
        assert!(changed);
        assert_eq!(dropped, 0);
        assert!(invalidated.contains(&"usgs-aerial-rooftop".to_string()));
        assert!(!manifest.remote_items.contains_key("usgs-aerial-rooftop"));
        assert!(manifest.remote_items.contains_key("scv-ortho-2025-rooftop"));
    }

    #[tokio::test]
    async fn commercial_satellite_url_builder_and_zero_secret_leak_contracts() {
        let empty_env = EnvConfig::from_map(HashMap::new());
        let scc_url = build_tile_url_for_provider(
            &empty_env,
            SatelliteProviderKind::SantaClaraSanJoseOrtho2025,
            19,
            203363,
            84561,
        )
        .await
        .expect("scc url");
        assert_eq!(
            scc_url,
            format!("{SCC_SJ_ORTHO_2025_TILE_URL}19/203363/84561")
        );

        let kingco_url = build_tile_url_for_provider(
            &empty_env,
            SatelliteProviderKind::KingCountyOrtho2023,
            19,
            178000,
            84100,
        )
        .await
        .expect("kingco url");
        assert_eq!(
            kingco_url,
            format!("{KINGCO_ORTHO_2023_TILE_URL}19/178000/84100")
        );

        let usgs_url = build_tile_url_for_provider(
            &empty_env,
            SatelliteProviderKind::UsgsImageryOnlyFallback,
            16,
            24800,
            19050,
        )
        .await
        .expect("usgs url");
        assert_eq!(
            usgs_url,
            format!("{USGS_IMAGERY_ONLY_TILE_URL}16/24800/19050")
        );

        let arcgis_secret = "arcgis-key-contract-val";
        let arcgis_env = EnvConfig::from_map(HashMap::from([(
            "ARCGIS_API_KEY".to_string(),
            arcgis_secret.to_string(),
        )]));
        let arcgis_url = build_tile_url_for_provider(
            &arcgis_env,
            SatelliteProviderKind::ArcGisCommercial,
            19,
            203363,
            84561,
        )
        .await
        .expect("arcgis url");
        assert!(arcgis_url.starts_with(ARCGIS_BASEMAPS_WORLD_IMAGERY_BASE_URL));
        assert!(arcgis_url.contains("token=arcgis-key-contract-val"));

        let mapbox_env = EnvConfig::from_map(HashMap::from([(
            "MAPBOX_ACCESS_TOKEN".to_string(),
            "pk.mapbox-test-token".to_string(),
        )]));
        let mapbox_url = build_tile_url_for_provider(
            &mapbox_env,
            SatelliteProviderKind::MapboxCommercial,
            19,
            200559,
            149968,
        )
        .await
        .expect("mapbox url");
        assert_eq!(
            mapbox_url,
            "https://api.mapbox.com/styles/v1/mapbox/satellite-v9/tiles/256/19/149968/200559@2x?access_token=pk.mapbox-test-token"
        );

        let fp_a = sat_key_fingerprint(&arcgis_env, SatelliteProviderKind::ArcGisCommercial);
        let fp_b = sat_key_fingerprint(&empty_env, SatelliteProviderKind::ArcGisCommercial);
        assert_ne!(fp_a, fp_b);
        assert!(!fp_a.contains(arcgis_secret));
        assert_eq!(fp_a.len(), 12);

        let meta = crate::property::media::build_report_satellite_meta(
            &arcgis_env,
            Some((37.288350, -121.944163)),
        );
        let meta_raw = serde_json::to_string(&meta).unwrap();
        assert!(!meta_raw.contains(arcgis_secret));
        assert_eq!(meta["proxied"], json!(true));
        assert_eq!(meta["tileUrl"], json!("/api/media/satellite/tile/{z}/{y}/{x}"));

        // Tile coordinate round-trip & regional export bbox URL check
        let (tx, ty) = lat_lon_to_tile_xy(37.288350, -121.944163, 19);
        let (clat, clon) = tile_center_lat_lon(19, tx, ty);
        assert!((clat - 37.288350).abs() < 0.01);
        assert!((clon + 121.944163).abs() < 0.01);

        let params = parse_satellite_export_params(
            "bbox=-121.945000,37.288000,-121.943000,37.289000&w=1600&h=1080&zoom=20",
        )
        .expect("export params");
        assert_eq!(params.width, 1600);
        assert_eq!(params.height, 1080);
        assert_eq!(params.zoom, 20);
        assert_eq!(scaled_2x_static_dimensions(params.width, params.height, 640), (640, 432));
        let capped = parse_satellite_export_params(
            "bbox=-121.945000,37.288000,-121.943000,37.289000&w=2500&h=1800&zoom=20",
        )
        .expect("capped export params");
        assert_eq!((capped.width, capped.height), (2048, 1800));
        let reg = regional_export_url_for_coords(37.288350, -121.944163, &params)
            .expect("scc regional export");
        assert!(reg.starts_with(SCC_SJ_ORTHO_2025_EXPORT_URL));
        assert!(regional_export_url_for_coords(39.952580, -75.163570, &params).is_none());
    }
}
