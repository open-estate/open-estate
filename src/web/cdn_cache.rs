//! Public Report CDN Edge Caching (`ETag`, `Cache-Control`, `CDN-Cache-Control`, `Vary`, `304 Not Modified`)
//! and Optional Bounded Static Mirror Exporter (`REPORT_STATIC_MIRROR_DIR`) for Cloudflare Pages / GitHub Pages.

use crate::extensions::RuntimeRegistrySnapshot;
use crate::guardrails::apply_security_headers;
use crate::shared::{is_truthy_flag, now_iso, EnvConfig};
use anyhow::{anyhow, Context, Result};
use axum::{
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Public finalized report CDN caching header value:
/// - Browser fresh for 5 minutes (`max-age=300`)
/// - Shared CDN / Cloudflare edge fresh for 24 hours (`s-maxage=86400`)
/// - Serve stale up to 7 days while revalidating asynchronously (`stale-while-revalidate=604800`)
pub const PUBLIC_REPORT_CACHE_CONTROL: &str =
    "public, max-age=300, s-maxage=86400, stale-while-revalidate=604800";

/// Cloudflare / CDN-specific cache directive for edge nodes.
pub const PUBLIC_REPORT_CDN_CACHE_CONTROL: &str = "public, max-age=86400";

/// Private / unfinalized (`aiPending`) / shared-visit cache directive.
pub const PRIVATE_REPORT_CACHE_CONTROL: &str = "private, no-store";

/// Default file cap for static mirrors (leaves headroom below Cloudflare Pages' 20,000-file limit).
pub const DEFAULT_REPORT_STATIC_MAX_FILES: usize = 18_000;

/// Default disk size cap in MiB for static mirrors (leaves headroom below GitHub Pages' 1 GB limit).
pub const DEFAULT_REPORT_STATIC_MAX_MB: u64 = 900;

static STATIC_EXPORT_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static TMP_FILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportCachePolicy {
    pub public_cacheable: bool,
    pub etag: Option<String>,
}

/// Computes a deterministic hash of currently enabled modules and registry generation.
pub fn compute_enabled_modules_hash(snap: &RuntimeRegistrySnapshot) -> String {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for id in snap.enabled_module_ids() {
        ids.insert(id);
    }
    let mut hasher = Sha256::new();
    hasher.update(snap.generation_version.to_le_bytes());
    for id in ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    hex::encode(&digest[..12])
}

/// Computes a deterministic RFC 9110 strong `ETag` (`"<hex>"`) from report `slug`, `updated_at`,
/// enabled modules hash, and representation variant (`"html"` or `"json"`).
pub fn compute_report_etag(
    slug: &str,
    updated_at: &str,
    modules_hash: &str,
    variant: &str,
) -> String {
    let mut hasher = Sha256::new();
    for field in [slug, updated_at, modules_hash, variant] {
        let bytes = field.as_bytes();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let digest = hasher.finalize();
    format!("\"{}\"", hex::encode(&digest[..16]))
}

/// Convenience wrapper computing the strong `ETag` directly from a `RuntimeRegistrySnapshot`.
pub fn compute_report_etag_with_snap(
    slug: &str,
    updated_at: &str,
    snap: &RuntimeRegistrySnapshot,
    variant: &str,
) -> String {
    let mod_hash = compute_enabled_modules_hash(snap);
    compute_report_etag(slug, updated_at, &mod_hash, variant)
}

/// Returns `true` when the report is still awaiting background AI synthesis (`meta.aiPending == true`).
pub fn report_ai_pending(report: &Value) -> bool {
    report
        .pointer("/meta/aiPending")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Returns `true` when the raw query string contains a share token or API key query parameter.
pub fn raw_query_has_share_or_api_key(raw_query: &str) -> bool {
    if raw_query.trim().is_empty() {
        return false;
    }
    for pair in raw_query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim();
        if !v.is_empty()
            && matches!(
                k,
                "share_token" | "shareToken" | "api_key" | "apiKey"
            )
        {
            return true;
        }
    }
    false
}

/// Returns `true` ONLY when a report may be publicly cached by Cloudflare / shared CDNs.
pub fn is_public_finalized_report(
    private_reports: bool,
    is_shared_visit: bool,
    ai_pending: bool,
) -> bool {
    !private_reports && !is_shared_visit && !ai_pending
}

/// Resolves the HTTP cache policy (and strong `ETag` when public & finalized) for `/r/:slug` or `/r/:slug/report.json`.
pub fn evaluate_report_cache_policy(
    slug: &str,
    updated_at: &str,
    snap: &RuntimeRegistrySnapshot,
    variant: &str,
    private_reports: bool,
    is_shared_visit: bool,
    ai_pending: bool,
) -> ReportCachePolicy {
    let public_cacheable = is_public_finalized_report(private_reports, is_shared_visit, ai_pending);
    let etag = if public_cacheable {
        Some(compute_report_etag_with_snap(slug, updated_at, snap, variant))
    } else {
        None
    };
    ReportCachePolicy {
        public_cacheable,
        etag,
    }
}

fn strip_weak_etag_prefix(raw: &str) -> &str {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("W/").or_else(|| t.strip_prefix("w/")) {
        rest.trim()
    } else {
        t
    }
}

/// Splits an `If-None-Match` header value on commas outside double quotes and performs
/// RFC 9110 weak comparison for `GET`/`HEAD` (handling CDNs that weaken ETags on compression).
pub fn if_none_match_matches(headers: &HeaderMap, current_etag: &str) -> bool {
    let norm_current = strip_weak_etag_prefix(current_etag);
    for hv in headers.get_all(header::IF_NONE_MATCH) {
        let Ok(raw) = hv.to_str() else {
            continue;
        };
        let mut start = 0usize;
        let mut in_quotes = false;
        let bytes = raw.as_bytes();
        for i in 0..=bytes.len() {
            let at_end = i == bytes.len();
            if !at_end {
                if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                    in_quotes = !in_quotes;
                }
                if bytes[i] != b',' || in_quotes {
                    continue;
                }
            }
            let token = raw[start..i].trim();
            start = i + 1;
            if token == "*" {
                return true;
            }
            if !token.is_empty() && strip_weak_etag_prefix(token) == norm_current {
                return true;
            }
        }
    }
    false
}

/// Applies `Cache-Control`, `CDN-Cache-Control`, `ETag`, and `Vary` headers according to `policy`.
pub fn apply_report_cache_headers(headers: &mut HeaderMap, policy: &ReportCachePolicy) {
    if policy.public_cacheable {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(PUBLIC_REPORT_CACHE_CONTROL),
        );
        headers.insert(
            HeaderName::from_static("cdn-cache-control"),
            HeaderValue::from_static(PUBLIC_REPORT_CDN_CACHE_CONTROL),
        );
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        if let Some(ref etag) = policy.etag {
            if let Ok(hv) = HeaderValue::from_str(etag) {
                headers.insert(header::ETAG, hv);
            }
        }
    } else {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(PRIVATE_REPORT_CACHE_CONTROL),
        );
        headers.remove(HeaderName::from_static("cdn-cache-control"));
        headers.remove(header::ETAG);
    }
}

/// Builds a `304 Not Modified` response preserving cache headers and security headers without `Set-Cookie`.
pub fn not_modified_response(policy: &ReportCachePolicy) -> Response {
    let mut resp = StatusCode::NOT_MODIFIED.into_response();
    apply_report_cache_headers(resp.headers_mut(), policy);
    apply_security_headers(resp.headers_mut());
    resp
}

// =================================================================================================
// Static Mirror Bundle Exporter (`REPORT_STATIC_MIRROR_DIR`)
// =================================================================================================

#[derive(Debug, Clone)]
pub struct StaticMirrorConfig {
    pub root_dir: PathBuf,
    pub max_files: usize,
    pub max_mb: u64,
}

pub fn static_mirror_config_from_env(env: &EnvConfig) -> Option<StaticMirrorConfig> {
    let raw_dir = env
        .get("REPORT_STATIC_MIRROR_DIR")
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let max_files = env
        .get("REPORT_STATIC_MAX_FILES")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n >= 3)
        .unwrap_or(DEFAULT_REPORT_STATIC_MAX_FILES);
    let max_mb = env
        .get("REPORT_STATIC_MAX_MB")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_REPORT_STATIC_MAX_MB);
    Some(StaticMirrorConfig {
        root_dir: PathBuf::from(raw_dir),
        max_files,
        max_mb,
    })
}

pub fn validate_export_slug(raw: &str) -> Result<String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() || s.len() > 160 {
        return Err(anyhow!("Invalid report slug length for static export"));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(anyhow!("Report slug contains invalid path characters"));
    }
    Ok(s)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticManifestEntry {
    pub slug: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub last_accessed_epoch_ms: u128,
    #[serde(default)]
    pub access_seq: u64,
    #[serde(default)]
    pub html_bytes: u64,
    #[serde(default)]
    pub json_bytes: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub file_count: usize,
    #[serde(default)]
    pub html_path: String,
    #[serde(default)]
    pub json_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticMirrorManifest {
    #[serde(default = "default_manifest_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    #[serde(default = "default_max_mb")]
    pub max_mb: u64,
    #[serde(default)]
    pub total_files: usize,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default = "default_next_seq")]
    pub next_seq: u64,
    #[serde(default)]
    pub reports: BTreeMap<String, StaticManifestEntry>,
}

fn default_manifest_version() -> u32 {
    1
}
fn default_max_files() -> usize {
    DEFAULT_REPORT_STATIC_MAX_FILES
}
fn default_max_mb() -> u64 {
    DEFAULT_REPORT_STATIC_MAX_MB
}
fn default_next_seq() -> u64 {
    1
}

impl Default for StaticMirrorManifest {
    fn default() -> Self {
        Self {
            schema_version: 1,
            updated_at: now_iso(),
            max_files: DEFAULT_REPORT_STATIC_MAX_FILES,
            max_mb: DEFAULT_REPORT_STATIC_MAX_MB,
            total_files: 1,
            total_bytes: 0,
            next_seq: 1,
            reports: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticExportResult {
    pub slug: String,
    pub html_path: PathBuf,
    pub json_path: PathBuf,
    pub manifest_path: PathBuf,
    pub evicted_slugs: Vec<String>,
    pub total_files: usize,
    pub total_bytes: u64,
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn atomic_write_file(dest: &Path, bytes: &[u8]) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("Destination has no parent directory: {}", dest.display()))?;
    std::fs::create_dir_all(parent)?;
    let fname = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let seq = TMP_FILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{fname}.{}.{}.{seq}.tmp",
        std::process::id(),
        now_epoch_ms()
    ));
    std::fs::write(&tmp, bytes)?;
    if let Err(err) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

/// Exports `<root>/r/<slug>/index.html`, `<root>/r/<slug>/report.json`, and updates `<root>/manifest.json`
/// while enforcing `REPORT_STATIC_MAX_FILES` (default `18000`) and `max_mb` (default `900` MiB) via LRU eviction.
pub fn export_static_report_bundle(
    root_dir: &Path,
    slug: &str,
    report_json: &Value,
    html: &str,
    max_mb: u64,
) -> Result<StaticExportResult> {
    export_static_report_bundle_with_limits(
        root_dir,
        slug,
        report_json,
        html,
        DEFAULT_REPORT_STATIC_MAX_FILES,
        max_mb,
    )
}

pub fn export_static_report_bundle_with_limits(
    root_dir: &Path,
    slug: &str,
    report_json: &Value,
    html: &str,
    max_files: usize,
    max_mb: u64,
) -> Result<StaticExportResult> {
    let clean_slug = validate_export_slug(slug)?;
    if max_files < 3 {
        return Err(anyhow!(
            "Static mirror max_files ({max_files}) must be >= 3 (manifest + index.html + report.json)"
        ));
    }
    let max_bytes = max_mb.saturating_mul(1024 * 1024);
    let json_bytes = serde_json::to_vec(report_json).context("Serialize report JSON for static mirror")?;
    let html_bytes = html.as_bytes();
    let new_bundle_files = 2usize;
    let new_bundle_bytes = (html_bytes.len() as u64).saturating_add(json_bytes.len() as u64);

    if new_bundle_bytes > max_bytes {
        return Err(anyhow!(
            "Single report bundle ({new_bundle_bytes} bytes) exceeds static mirror max_mb ({max_mb} MiB)"
        ));
    }

    let _guard = STATIC_EXPORT_LOCK
        .lock()
        .map_err(|_| anyhow!("Static mirror export mutex poisoned"))?;

    std::fs::create_dir_all(root_dir)?;
    let manifest_path = root_dir.join("manifest.json");
    let existing_manifest_bytes = std::fs::metadata(&manifest_path)
        .map(|m| m.len())
        .unwrap_or(512);
    let mut manifest: StaticMirrorManifest = if manifest_path.is_file() {
        std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|s| serde_json::from_str::<StaticMirrorManifest>(&s).ok())
            .unwrap_or_default()
    } else {
        StaticMirrorManifest::default()
    };

    // Prune stale manifest rows whose files were removed externally.
    manifest.reports.retain(|k, _| {
        let dir = root_dir.join("r").join(k);
        dir.join("index.html").is_file() && dir.join("report.json").is_file()
    });

    // Exclude the target slug itself before sizing eviction so re-exporting updates in place.
    manifest.reports.remove(&clean_slug);

    // Reserve realistic headroom for `manifest.json` itself (never below 512 bytes).
    let manifest_reserve = existing_manifest_bytes.max(512).min(max_bytes / 4 + 512);
    let mut evicted_slugs = Vec::new();

    loop {
        let cur_files: usize = manifest.reports.values().map(|e| e.file_count).sum();
        let cur_bytes: u64 = manifest.reports.values().map(|e| e.total_bytes).sum();
        let projected_files = cur_files.saturating_add(new_bundle_files).saturating_add(1);
        let projected_bytes = cur_bytes
            .saturating_add(new_bundle_bytes)
            .saturating_add(manifest_reserve);

        if projected_files <= max_files && projected_bytes <= max_bytes {
            break;
        }
        let Some(lru_slug) = manifest
            .reports
            .iter()
            .min_by_key(|(k, e)| (e.last_accessed_epoch_ms, e.access_seq, (*k).clone()))
            .map(|(k, _)| k.clone())
        else {
            if projected_files > max_files || cur_bytes.saturating_add(new_bundle_bytes) > max_bytes {
                return Err(anyhow!(
                    "Static mirror bundle cannot fit within max_files={max_files}, max_mb={max_mb}"
                ));
            }
            break;
        };
        manifest.reports.remove(&lru_slug);
        let _ = std::fs::remove_dir_all(root_dir.join("r").join(&lru_slug));
        evicted_slugs.push(lru_slug);
    }

    let slug_dir = root_dir.join("r").join(&clean_slug);
    let html_path = slug_dir.join("index.html");
    let json_path = slug_dir.join("report.json");
    atomic_write_file(&html_path, html_bytes)?;
    atomic_write_file(&json_path, &json_bytes)?;

    let seq = manifest.next_seq;
    manifest.next_seq = manifest.next_seq.saturating_add(1);
    let access_ms = now_epoch_ms();
    let updated_at = report_json
        .pointer("/meta/generatedAt")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(now_iso);
    let address = report_json
        .pointer("/meta/displayAddress")
        .or_else(|| report_json.pointer("/property/address"))
        .or_else(|| report_json.pointer("/input/address"))
        .and_then(Value::as_str)
        .map(String::from);

    manifest.reports.insert(
        clean_slug.clone(),
        StaticManifestEntry {
            slug: clean_slug.clone(),
            address,
            updated_at: updated_at.clone(),
            last_accessed_epoch_ms: access_ms,
            access_seq: seq,
            html_bytes: html_bytes.len() as u64,
            json_bytes: json_bytes.len() as u64,
            total_bytes: new_bundle_bytes,
            file_count: new_bundle_files,
            html_path: format!("r/{clean_slug}/index.html"),
            json_path: format!("r/{clean_slug}/report.json"),
        },
    );

    manifest.updated_at = now_iso();
    manifest.max_files = max_files;
    manifest.max_mb = max_mb;
    manifest.total_files = manifest
        .reports
        .values()
        .map(|e| e.file_count)
        .sum::<usize>()
        .saturating_add(1);
    let bundles_bytes: u64 = manifest.reports.values().map(|e| e.total_bytes).sum();
    manifest.total_bytes = bundles_bytes;
    let first_pass = serde_json::to_vec_pretty(&manifest)?;
    manifest.total_bytes = bundles_bytes.saturating_add(first_pass.len() as u64);
    let manifest_payload = serde_json::to_vec_pretty(&manifest)?;
    atomic_write_file(&manifest_path, &manifest_payload)?;

    Ok(StaticExportResult {
        slug: clean_slug,
        html_path,
        json_path,
        manifest_path,
        evicted_slugs,
        total_files: manifest.total_files,
        total_bytes: manifest.total_bytes,
    })
}

/// Best-effort background trigger invoked when a public finalized report is produced or updated.
pub fn spawn_static_export_if_configured(
    env: &EnvConfig,
    slug: &str,
    report: &Value,
    html: String,
) {
    let private_reports = env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    if private_reports || report_ai_pending(report) {
        return;
    }
    let Some(cfg) = static_mirror_config_from_env(env) else {
        return;
    };
    let slug_owned = slug.to_string();
    let report_owned = report.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = export_static_report_bundle_with_limits(
            &cfg.root_dir,
            &slug_owned,
            &report_owned,
            &html,
            cfg.max_files,
            cfg.max_mb,
        ) {
            tracing::warn!(
                slug = %slug_owned,
                dir = %cfg.root_dir.display(),
                error = %e,
                "Best-effort static report mirror export failed"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::RuntimeRegistrySnapshot;
    use serde_json::json;
    use uuid::Uuid;

    fn sample_snap(gen: u64) -> RuntimeRegistrySnapshot {
        RuntimeRegistrySnapshot {
            generation_version: gen,
            modules: std::collections::HashMap::new(),
            enabled_extension_ids_by_domain: std::collections::HashMap::new(),
            manifests: Vec::new(),
        }
    }

    #[test]
    fn strong_etag_is_deterministic_and_sensitive_to_inputs() {
        let s1 = sample_snap(1);
        let s2 = sample_snap(2);
        let h1 = compute_enabled_modules_hash(&s1);
        let h2 = compute_enabled_modules_hash(&s2);
        assert_ne!(h1, h2, "registry generation must affect modules hash");

        let e1 = compute_report_etag("synth-report-beta", "2026-11-15T00:00:00Z", &h1, "html");
        let e1_again = compute_report_etag("synth-report-beta", "2026-11-15T00:00:00Z", &h1, "html");
        let e_json = compute_report_etag("synth-report-beta", "2026-11-15T00:00:00Z", &h1, "json");
        let e_updated = compute_report_etag("synth-report-beta", "2026-11-15T01:00:00Z", &h1, "html");
        let e_other_slug = compute_report_etag("synth-report-alpha", "2026-11-15T00:00:00Z", &h1, "html");

        assert!(e1.starts_with('"') && e1.ends_with('"') && !e1.starts_with("W/"));
        assert_eq!(e1, e1_again);
        assert_ne!(e1, e_json);
        assert_ne!(e1, e_updated);
        assert_ne!(e1, e_other_slug);
    }

    #[test]
    fn cache_headers_distinguish_public_finalized_vs_private_pending_and_shared() {
        let snap = sample_snap(1);

        let pub_pol = evaluate_report_cache_policy(
            "synth-report-beta",
            "2026-11-15T00:00:00Z",
            &snap,
            "html",
            false,
            false,
            false,
        );
        assert!(pub_pol.public_cacheable);
        assert!(pub_pol.etag.is_some());

        let mut hdrs = HeaderMap::new();
        apply_report_cache_headers(&mut hdrs, &pub_pol);
        assert_eq!(
            hdrs.get(header::CACHE_CONTROL).unwrap().to_str().unwrap(),
            PUBLIC_REPORT_CACHE_CONTROL
        );
        assert_eq!(
            hdrs.get("cdn-cache-control").unwrap().to_str().unwrap(),
            PUBLIC_REPORT_CDN_CACHE_CONTROL
        );
        assert_eq!(
            hdrs.get(header::VARY).unwrap().to_str().unwrap(),
            "Accept-Encoding"
        );
        assert!(hdrs.get(header::ETAG).is_some());

        for (priv_r, shared, pending) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let pol = evaluate_report_cache_policy(
                "synth-report-beta",
                "2026-11-15T00:00:00Z",
                &snap,
                "html",
                priv_r,
                shared,
                pending,
            );
            assert!(!pol.public_cacheable);
            assert!(pol.etag.is_none());
            let mut h = hdrs.clone();
            apply_report_cache_headers(&mut h, &pol);
            assert_eq!(
                h.get(header::CACHE_CONTROL).unwrap().to_str().unwrap(),
                PRIVATE_REPORT_CACHE_CONTROL
            );
            assert!(h.get("cdn-cache-control").is_none());
            assert!(h.get(header::ETAG).is_none());
        }
    }

    #[test]
    fn if_none_match_handles_strong_weak_comma_list_and_wildcard() {
        let etag = "\"0123456789abcdef0123456789abcdef\"";
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_str(etag).unwrap());
        assert!(if_none_match_matches(&h, etag));

        let mut h_weak = HeaderMap::new();
        h_weak.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("\"other\", W/{etag}")).unwrap(),
        );
        assert!(if_none_match_matches(&h_weak, etag));

        let mut h_star = HeaderMap::new();
        h_star.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match_matches(&h_star, etag));

        let mut h_miss = HeaderMap::new();
        h_miss.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"deadbeef\""));
        assert!(!if_none_match_matches(&h_miss, etag));
    }

    #[test]
    fn static_mirror_export_writes_bundle_and_evicts_lru_on_file_and_size_caps() {
        let root = std::env::temp_dir().join(format!("oe-static-mirror-{}", Uuid::new_v4()));
        // 5 files max => at most 2 reports (2 * 2 files + 1 manifest = 5 files).
        let rep1 = json!({ "meta": { "slug": "home-1", "displayAddress": "101 Synthmirror01 St", "generatedAt": "2026-11-15T00:00:00Z" } });
        let rep2 = json!({ "meta": { "slug": "home-2", "displayAddress": "102 Synthmirror02 St", "generatedAt": "2026-11-15T00:01:00Z" } });
        let rep3 = json!({ "meta": { "slug": "home-3", "displayAddress": "103 Synthmirror03 St", "generatedAt": "2026-11-15T00:02:00Z" } });

        let r1 = export_static_report_bundle_with_limits(&root, "home-1", &rep1, "<html>1</html>", 5, 10).unwrap();
        assert!(r1.html_path.is_file());
        assert!(r1.json_path.is_file());
        assert_eq!(r1.total_files, 3);
        assert!(r1.evicted_slugs.is_empty());

        let r2 = export_static_report_bundle_with_limits(&root, "home-2", &rep2, "<html>2</html>", 5, 10).unwrap();
        assert_eq!(r2.total_files, 5);
        assert!(r2.evicted_slugs.is_empty());

        // Touch home-1 again so home-2 becomes the LRU!
        let _ = export_static_report_bundle_with_limits(&root, "home-1", &rep1, "<html>1b</html>", 5, 10).unwrap();

        // Exporting home-3 must evict home-2 (LRU), keeping total_files <= 5.
        let r3 = export_static_report_bundle_with_limits(&root, "home-3", &rep3, "<html>3</html>", 5, 10).unwrap();
        assert_eq!(r3.evicted_slugs, vec!["home-2".to_string()]);
        assert_eq!(r3.total_files, 5);
        assert!(!root.join("r/home-2").exists());
        assert!(root.join("r/home-1/index.html").is_file());
        assert!(root.join("r/home-3/report.json").is_file());

        // Now test 1 MiB size cap with ~600 KB HTML payloads: 2nd report must evict 1st.
        let big_html = "x".repeat(620 * 1024);
        let r_big1 = export_static_report_bundle_with_limits(&root, "big-1", &rep1, &big_html, 100, 1).unwrap();
        assert!(r_big1.total_bytes <= 1024 * 1024);
        let r_big2 = export_static_report_bundle_with_limits(&root, "big-2", &rep2, &big_html, 100, 1).unwrap();
        assert!(r_big2.total_bytes <= 1024 * 1024);
        assert!(!root.join("r/big-1").exists(), "big-1 must be LRU-evicted under 1 MiB cap");
        assert!(root.join("r/big-2/index.html").is_file());

        let manifest_raw = std::fs::read_to_string(root.join("manifest.json")).unwrap();
        let manifest: StaticMirrorManifest = serde_json::from_str(&manifest_raw).unwrap();
        assert!(manifest.reports.contains_key("big-2"));
        assert!(!manifest.reports.contains_key("big-1"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
