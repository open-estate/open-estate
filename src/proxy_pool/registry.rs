//! Source registry + per-source state: conditional, size-capped downloads with adaptive polling.

use super::candidates::CandKey;
use super::extract::{parse_registry, SourceSpec};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Default registries: empty by default so Open Estate never harvests third-party public free
/// proxy lists unless an operator explicitly configures `FREE_PROXY_REGISTRY_URLS`.
pub const DEFAULT_REGISTRY_URLS: &[&str] = &[];

pub(crate) const REGISTRY_CACHE_FILE: &str = "free_proxy_registry.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceOrigin {
    Registry,
    Operator,
}

#[derive(Debug)]
pub struct SourceState {
    pub id: u16,
    pub spec: SourceSpec,
    pub label: String,
    pub origin: SourceOrigin,
    pub active: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub next_fetch_at: i64,
    pub consecutive_failures: u32,
    pub last_fetch_at: i64,
    pub last_status: Option<u16>,
    pub last_error: Option<String>,
    pub last_count: usize,
    pub last_bytes: usize,
    pub fetches: u64,
    pub not_modified: u64,
    pub failures: u64,
    pub checked: u64,
    pub verified: u64,
    pub keys: Vec<CandKey>,
}

impl SourceState {
    pub fn new(id: u16, spec: SourceSpec, origin: SourceOrigin) -> Self {
        let label = source_label(&spec.url);
        Self {
            id,
            spec,
            label,
            origin,
            active: true,
            etag: None,
            last_modified: None,
            next_fetch_at: 0,
            consecutive_failures: 0,
            last_fetch_at: 0,
            last_status: None,
            last_error: None,
            last_count: 0,
            last_bytes: 0,
            fetches: 0,
            not_modified: 0,
            failures: 0,
            checked: 0,
            verified: 0,
            keys: Vec::new(),
        }
    }

    /// Smoothed yield (verified / checked) with a 5 % prior, used to weight candidates.
    pub fn weight(&self) -> f32 {
        (self.verified as f32 + 1.0) / (self.checked as f32 + 20.0)
    }

    /// Polling multiplier: sources that produced nothing after many checks are polled 4× less.
    pub fn interval_multiplier(&self) -> i64 {
        if self.checked >= 300 && self.verified == 0 {
            4
        } else {
            1
        }
    }
}

/// Short, stable label for logs/status (`owner/repo/file` for GitHub raw and jsDelivr URLs).
pub fn source_label(url: &str) -> String {
    let Ok(u) = url::Url::parse(url) else {
        return url.chars().take(60).collect();
    };
    let host = u.host_str().unwrap_or("");
    let segs: Vec<&str> = u.path_segments().map(|s| s.collect()).unwrap_or_default();
    let file = segs.last().copied().unwrap_or("");
    match host {
        "raw.githubusercontent.com" if segs.len() >= 2 => format!("{}/{}/{}", segs[0], segs[1], file),
        "cdn.jsdelivr.net" if segs.len() >= 3 && segs[0] == "gh" => {
            let repo = segs[2].split('@').next().unwrap_or(segs[2]);
            format!("{}/{}/{}", segs[1], repo, file)
        }
        _ if file.is_empty() => host.to_string(),
        _ => format!("{host}/{file}"),
    }
}

#[derive(Debug, Default, Clone)]
pub struct RegistryUrlState {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub entries: Vec<SourceSpec>,
    pub last_ok_at: i64,
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
pub struct RegistryState {
    pub per_url: HashMap<String, RegistryUrlState>,
    pub last_refresh_at: i64,
    pub next_refresh_at: i64,
    pub consecutive_failures: u32,
    pub loaded_from_cache: bool,
}

impl RegistryState {
    /// Union of the latest good entries of every registry URL (mirrors dedupe naturally).
    pub fn union(&self, urls: &[String]) -> Vec<SourceSpec> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for u in urls {
            if let Some(st) = self.per_url.get(u) {
                for e in &st.entries {
                    if seen.insert((e.url.clone(), e.protocol)) {
                        out.push(e.clone());
                    }
                }
            }
        }
        out
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistryCacheFile {
    saved_at: i64,
    sources: Vec<SourceSpec>,
}

pub fn load_registry_cache(dir: &Path) -> Option<Vec<SourceSpec>> {
    let raw = std::fs::read_to_string(dir.join(REGISTRY_CACHE_FILE)).ok()?;
    let parsed: RegistryCacheFile = serde_json::from_str(&raw).ok()?;
    (!parsed.sources.is_empty()).then_some(parsed.sources)
}

pub async fn save_registry_cache(dir: &Path, sources: &[SourceSpec], now: i64) {
    let file = RegistryCacheFile {
        saved_at: now,
        sources: sources.to_vec(),
    };
    if let Ok(json) = serde_json::to_vec_pretty(&file) {
        write_atomic(&dir.join(REGISTRY_CACHE_FILE), &json).await;
    }
}

pub async fn write_atomic(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if tokio::fs::write(&tmp, bytes).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, path).await;
    } else {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

#[derive(Debug)]
pub enum FetchOutcome {
    NotModified,
    Fresh {
        body: String,
        bytes: usize,
        etag: Option<String>,
        last_modified: Option<String>,
        truncated: bool,
    },
    Failed {
        status: Option<u16>,
        error: String,
    },
}

/// GET with `If-None-Match` / `If-Modified-Since`, streaming the body and stopping at
/// `max_bytes` (lists are line-oriented, so a truncated body still parses).
pub async fn conditional_get(
    client: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    timeout: Duration,
    max_bytes: usize,
) -> FetchOutcome {
    let mut req = client.get(url).timeout(timeout);
    if let Some(e) = etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, e);
    }
    if let Some(lm) = last_modified {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
    }
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let kind = if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "connect failed"
            } else {
                "request failed"
            };
            return FetchOutcome::Failed {
                status: None,
                error: kind.to_string(),
            };
        }
    };
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return FetchOutcome::NotModified;
    }
    if !status.is_success() {
        return FetchOutcome::Failed {
            status: Some(status.as_u16()),
            error: format!("HTTP {}", status.as_u16()),
        };
    }
    let header = |name: reqwest::header::HeaderName| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let new_etag = header(reqwest::header::ETAG);
    let new_lm = header(reqwest::header::LAST_MODIFIED);

    let mut buf: Vec<u8> = Vec::with_capacity(
        resp.content_length()
            .map(|l| (l as usize).min(max_bytes))
            .unwrap_or(64 * 1024),
    );
    let mut truncated = false;
    let mut read_error = false;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = max_bytes.saturating_sub(buf.len());
                if chunk.len() > room {
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                if buf.is_empty() {
                    return FetchOutcome::Failed {
                        status: Some(status.as_u16()),
                        error: "body read failed".to_string(),
                    };
                }
                truncated = true;
                read_error = true;
                break;
            }
        }
    }
    if truncated {
        // Drop the partial last line so a cut entry (e.g. `1.2.3.4:80` → `1.2.3.4:8`) is not
        // parsed as a different proxy.
        if let Some(nl) = buf.iter().rposition(|&b| b == b'\n') {
            buf.truncate(nl + 1);
        }
    }
    let bytes = buf.len();
    FetchOutcome::Fresh {
        body: String::from_utf8_lossy(&buf).into_owned(),
        bytes,
        // A body cut short by a network error must be downloaded again in full next time; one
        // cut at the size cap keeps its validators (a re-download would yield the same prefix).
        etag: if read_error { None } else { new_etag },
        last_modified: if read_error { None } else { new_lm },
        truncated,
    }
}

/// Outcome of downloading every registry URL (registry entries are kept per URL so a failing
/// mirror never wipes the sources learned from a healthy one).
pub async fn fetch_registries(
    client: &reqwest::Client,
    urls: &[String],
    state: &[(String, Option<String>, Option<String>)],
    timeout: Duration,
) -> Vec<(String, FetchOutcome, Option<Vec<SourceSpec>>)> {
    let lookup: HashMap<&str, (Option<&str>, Option<&str>)> = state
        .iter()
        .map(|(u, e, l)| (u.as_str(), (e.as_deref(), l.as_deref())))
        .collect();
    futures::stream::iter(urls.iter().cloned())
        .map(|url| {
            let (etag, lm) = lookup.get(url.as_str()).copied().unwrap_or((None, None));
            let etag = etag.map(str::to_string);
            let lm = lm.map(str::to_string);
            async move {
                let out = conditional_get(
                    client,
                    &url,
                    etag.as_deref(),
                    lm.as_deref(),
                    timeout,
                    2 * 1024 * 1024,
                )
                .await;
                let parsed = match &out {
                    FetchOutcome::Fresh { body, .. } => Some(parse_registry(body)),
                    _ => None,
                };
                (url, out, parsed)
            }
        })
        .buffer_unordered(4)
        .collect()
        .await
}
