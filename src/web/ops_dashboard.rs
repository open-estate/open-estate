// Open Estate Platform Ops, Cache & Connector Telemetry Control Panel (`/ops` & `GET /api/ops/telemetry`).
// Zero-overhead lock-free request counters, bounded per-route latency ring buffers (N=256),
// dialect-portable SQLite WAL / MySQL HeatWave operational aggregates, Linux `/proc/self` RSS memory
// reporting, sanitized connector posture, and self-contained responsive ECharts dashboard UI.

use crate::db::{DbBackend, DbStore};
use crate::guardrails::{apply_security_headers, constant_time_eq};
use crate::shared::{
    effective_brand_name, format_brand_wordmark_html, normalize_retired_brand, now_iso,
    now_unix_secs, round_n, EnvConfig,
};
use crate::web::favicon::{logo_mark_img, FAVICON_HEAD_TAGS};
use crate::web::render::safe_inline_json;
use crate::web::server::{resolve_client_ip_from_req, AppState};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, Extensions, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
    Json,
};
use html_escape::{encode_double_quoted_attribute, encode_text};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const OPS_ROUTE_LATENCY_RING_CAP: usize = 256;
pub const OPS_RECENT_EVENTS_CAP: usize = 30;
pub const OPS_TIME_SERIES_CAP: usize = 30;
const TIME_BUCKET_SECS: i64 = 10;

/// Canonical route buckets monitored by the telemetry middleware and rendered on `/ops`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum RouteBucket {
    ReportPage = 0,
    ApiReport = 1,
    ApiAsk = 2,
    ApiSlides = 3,
    ApiArtifact = 4,
    ApiSearch = 5,
    DealRoom = 6,
    Ops = 7,
    HealthProbes = 8,
    Other = 9,
}

impl RouteBucket {
    pub const ALL: [RouteBucket; 10] = [
        RouteBucket::ReportPage,
        RouteBucket::ApiReport,
        RouteBucket::ApiAsk,
        RouteBucket::ApiSlides,
        RouteBucket::ApiArtifact,
        RouteBucket::ApiSearch,
        RouteBucket::DealRoom,
        RouteBucket::Ops,
        RouteBucket::HealthProbes,
        RouteBucket::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RouteBucket::ReportPage => "/r/:slug",
            RouteBucket::ApiReport => "/api/report",
            RouteBucket::ApiAsk => "/api/ask",
            RouteBucket::ApiSlides => "/api/report/:slug/slides",
            RouteBucket::ApiArtifact => "/api/report/:slug/artifact",
            RouteBucket::ApiSearch => "/api/search",
            RouteBucket::DealRoom => "/deal-room",
            RouteBucket::Ops => "/ops",
            RouteBucket::HealthProbes => "/healthz",
            RouteBucket::Other => "other",
        }
    }
}

/// Maps a sanitized URI path onto a fixed telemetry bucket without dynamic allocations.
/// Order matters: `/api/report/:slug/slides` and `/api/report/:slug/artifact` must match
/// before the broader `/api/report` prefix.
pub fn classify_route_bucket(sanitized_path: &str) -> RouteBucket {
    let p = sanitized_path.trim_end_matches('/');
    let path = if p.is_empty() { "/" } else { p };

    if path.starts_with("/api/report/") && path.ends_with("/slides") {
        RouteBucket::ApiSlides
    } else if path.starts_with("/api/report/") && path.ends_with("/artifact") {
        RouteBucket::ApiArtifact
    } else if path == "/r" || path.starts_with("/r/") {
        RouteBucket::ReportPage
    } else if path == "/api/report"
        || path.starts_with("/api/report/")
        || path == "/report"
        || path == "/api/job"
        || path.starts_with("/api/job/")
    {
        RouteBucket::ApiReport
    } else if path == "/api/ask" || path == "/api/translate" {
        RouteBucket::ApiAsk
    } else if path == "/api/search"
        || path.starts_with("/api/search/")
        || path == "/api/match"
        || path.starts_with("/api/match/")
        || path == "/api/listings"
        || path.starts_with("/api/listings/")
        || path == "/api/suggest"
        || path == "/match"
        || path == "/shop"
        || path == "/api/geo/ip"
    {
        RouteBucket::ApiSearch
    } else if path == "/deal-room"
        || path.starts_with("/deal-room/")
        || path == "/api/deal-room"
        || path.starts_with("/api/deal-room/")
        || path.starts_with("/api/extension/")
    {
        RouteBucket::DealRoom
    } else if path == "/ops"
        || path == "/api/ops/telemetry"
        || path == "/admin"
        || path.starts_with("/admin/")
        || path == "/api/admin"
        || path.starts_with("/api/admin/")
    {
        RouteBucket::Ops
    } else if path == "/healthz" || path == "/readyz" || path == "/api/health" {
        RouteBucket::HealthProbes
    } else {
        RouteBucket::Other
    }
}

fn looks_like_secret_or_pii_segment(seg: &str) -> bool {
    if seg.is_empty() {
        return false;
    }
    if seg.contains('@') || seg.contains("%40") {
        return true;
    }
    if seg.parse::<IpAddr>().is_ok() {
        return true;
    }
    if seg.starts_with("eyJ")
        || seg.starts_with("dbk_")
        || seg.starts_with("sk_")
        || seg.starts_with("pk_")
        || seg.starts_with("rk_")
        || seg.starts_with("whsec_")
    {
        return true;
    }
    // Long unhyphenated opaque tokens (hex/base64/random IDs >= 24 chars); regular property slugs use hyphens.
    if !seg.contains('-') && seg.len() >= 24 {
        return true;
    }
    false
}

/// Strips query strings, fragments, IPs, emails, API keys, and opaque auth/share tokens from
/// request paths before storing them in the in-memory telemetry ring buffer.
pub fn sanitize_telemetry_path(raw_path: &str) -> String {
    let base = raw_path.split(['?', '#']).next().unwrap_or("/").trim();

    let mut segs: Vec<String> = Vec::new();
    for raw_seg in base.split('/').filter(|s| !s.is_empty()) {
        let clean: String = raw_seg
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@'))
            .take(64)
            .collect();
        if clean.is_empty() {
            continue;
        }
        if clean.parse::<IpAddr>().is_ok() {
            segs.push("[redacted-ip]".to_string());
            continue;
        }
        if looks_like_secret_or_pii_segment(&clean) {
            segs.push("[redacted]".to_string());
            continue;
        }
        segs.push(clean);
    }

    // Collapse dynamic ID/token segments on sensitive endpoints cleanly.
    if segs.len() >= 3 && segs[0] == "api" && segs[1] == "keys" {
        segs.truncate(3);
        segs[2] = ":id".to_string();
    } else if segs.len() >= 3 && segs[0] == "api" && segs[1] == "job" {
        segs[2] = ":id".to_string();
    } else if segs.len() >= 4 && segs[0] == "api" && segs[1] == "scan" && segs[2] == "batch" {
        segs[3] = ":id".to_string();
    } else if segs.len() >= 4 && segs[0] == "api" && segs[1] == "match" && segs[2] == "rank" {
        segs[3] = ":id".to_string();
    } else if segs.len() >= 3 && segs[0] == "api" && segs[1] == "auth" {
        segs.truncate(3);
    } else if segs.len() >= 2 && segs[0] == "auth" && segs[1] == "verify" {
        segs.truncate(2);
    } else if segs.len() >= 2 && segs[0] == "share" {
        segs.truncate(2);
        segs[1] = ":token".to_string();
    } else if segs.len() >= 4 && segs[0] == "api" && segs[1] == "reports" && segs[3] == "share" {
        segs[2] = ":slug".to_string();
    } else if segs.len() >= 4
        && segs[0] == "api"
        && segs[1] == "admin"
        && (segs[2] == "users" || segs[2] == "subscriptions")
    {
        segs[3] = ":id".to_string();
    }

    let joined = if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    };

    if joined.len() <= 96 {
        joined
    } else {
        format!("{}...", &joined[..93])
    }
}

/// Fixed-capacity ring buffer keeping the last 256 latency samples (in microseconds) per route.
#[derive(Debug)]
struct LatencyRing256 {
    samples_us: [u64; OPS_ROUTE_LATENCY_RING_CAP],
    head: usize,
    len: usize,
}

impl Default for LatencyRing256 {
    fn default() -> Self {
        Self {
            samples_us: [0; OPS_ROUTE_LATENCY_RING_CAP],
            head: 0,
            len: 0,
        }
    }
}

impl LatencyRing256 {
    fn push(&mut self, duration_us: u64) {
        self.samples_us[self.head] = duration_us;
        self.head = (self.head + 1) % OPS_ROUTE_LATENCY_RING_CAP;
        if self.len < OPS_ROUTE_LATENCY_RING_CAP {
            self.len += 1;
        }
    }

    fn percentiles_ms(&self) -> (f64, f64, usize) {
        if self.len == 0 {
            return (0.0, 0.0, 0);
        }
        let mut buf = [0_u64; OPS_ROUTE_LATENCY_RING_CAP];
        let n = self.len;
        if n == OPS_ROUTE_LATENCY_RING_CAP {
            buf.copy_from_slice(&self.samples_us);
        } else {
            buf[..n].copy_from_slice(&self.samples_us[..n]);
        }
        let slice = &mut buf[..n];
        slice.sort_unstable();

        let p50_idx = ((n as f64 * 0.50).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        let p95_idx = ((n as f64 * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);

        let p50_ms = round_n(slice[p50_idx] as f64 / 1000.0, 2);
        let p95_ms = round_n(slice[p95_idx] as f64 / 1000.0, 2);
        (p50_ms, p95_ms, n)
    }
}

#[derive(Debug)]
struct RouteMetricsSlot {
    bucket: RouteBucket,
    requests: AtomicU64,
    client_errors: AtomicU64,
    server_errors: AtomicU64,
    total_duration_us: AtomicU64,
    max_duration_us: AtomicU64,
    latencies: Mutex<LatencyRing256>,
}

impl RouteMetricsSlot {
    fn new(bucket: RouteBucket) -> Self {
        Self {
            bucket,
            requests: AtomicU64::new(0),
            client_errors: AtomicU64::new(0),
            server_errors: AtomicU64::new(0),
            total_duration_us: AtomicU64::new(0),
            max_duration_us: AtomicU64::new(0),
            latencies: Mutex::new(LatencyRing256::default()),
        }
    }

    fn record(&self, status: u16, duration_us: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if (400..500).contains(&status) {
            self.client_errors.fetch_add(1, Ordering::Relaxed);
        } else if status >= 500 {
            self.server_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.total_duration_us
            .fetch_add(duration_us, Ordering::Relaxed);
        // Atomic max update loop
        let mut cur = self.max_duration_us.load(Ordering::Relaxed);
        while duration_us > cur {
            match self.max_duration_us.compare_exchange_weak(
                cur,
                duration_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        if let Ok(mut ring) = self.latencies.lock() {
            ring.push(duration_us);
        }
    }

    fn snapshot(&self) -> RouteMetricsSnapshot {
        let requests = self.requests.load(Ordering::Relaxed);
        let client_errors = self.client_errors.load(Ordering::Relaxed);
        let server_errors = self.server_errors.load(Ordering::Relaxed);
        let total_us = self.total_duration_us.load(Ordering::Relaxed);
        let max_us = self.max_duration_us.load(Ordering::Relaxed);
        let mean_ms = if requests > 0 {
            round_n((total_us as f64 / requests as f64) / 1000.0, 2)
        } else {
            0.0
        };
        let max_ms = round_n(max_us as f64 / 1000.0, 2);
        let (p50_ms, p95_ms, sample_count) = self
            .latencies
            .lock()
            .map(|ring| ring.percentiles_ms())
            .unwrap_or((0.0, 0.0, 0));

        RouteMetricsSnapshot {
            route: self.bucket.label().to_string(),
            requests,
            errors: client_errors + server_errors,
            client_errors,
            server_errors,
            mean_ms,
            p50_ms,
            p95_ms,
            max_ms,
            sample_count,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteMetricsSnapshot {
    pub route: String,
    pub requests: u64,
    pub errors: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub sample_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentRequestEvent {
    pub method: String,
    pub path: String,
    pub route_bucket: String,
    pub status: u16,
    pub duration_ms: f64,
    pub timestamp: String,
}

#[derive(Debug, Clone)]
struct TimeSeriesEntry {
    unix_bucket: i64,
    label: String,
    requests: u64,
    errors: u64,
    total_duration_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSeriesPoint {
    pub label: String,
    pub unix_bucket: i64,
    pub requests: u64,
    pub errors: u64,
    pub avg_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpsHttpSnapshot {
    pub started_at: String,
    pub uptime_secs: u64,
    pub total_requests: u64,
    pub in_flight_requests: u64,
    pub status_counts: StatusClassCounts,
    pub error_rate_pct: f64,
    pub routes: Vec<RouteMetricsSnapshot>,
    pub time_series: Vec<TimeSeriesPoint>,
    pub recent_events: Vec<RecentRequestEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusClassCounts {
    #[serde(rename = "2xx")]
    pub c2xx: u64,
    #[serde(rename = "3xx")]
    pub c3xx: u64,
    #[serde(rename = "4xx")]
    pub c4xx: u64,
    #[serde(rename = "5xx")]
    pub c5xx: u64,
}

/// Global/shared HTTP & route latency telemetry registry.
pub struct OpsTelemetryRegistry {
    started_at: String,
    started_instant: Instant,
    total_requests: AtomicU64,
    in_flight_requests: AtomicU64,
    status_2xx: AtomicU64,
    status_3xx: AtomicU64,
    status_4xx: AtomicU64,
    status_5xx: AtomicU64,
    routes: Vec<RouteMetricsSlot>,
    recent_events: Mutex<VecDeque<RecentRequestEvent>>,
    time_series: Mutex<VecDeque<TimeSeriesEntry>>,
}

static GLOBAL_OPS_TELEMETRY: OnceLock<Arc<OpsTelemetryRegistry>> = OnceLock::new();

impl Default for OpsTelemetryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OpsTelemetryRegistry {
    pub fn new() -> Self {
        let routes = RouteBucket::ALL
            .iter()
            .copied()
            .map(RouteMetricsSlot::new)
            .collect();
        Self {
            started_at: now_iso(),
            started_instant: Instant::now(),
            total_requests: AtomicU64::new(0),
            in_flight_requests: AtomicU64::new(0),
            status_2xx: AtomicU64::new(0),
            status_3xx: AtomicU64::new(0),
            status_4xx: AtomicU64::new(0),
            status_5xx: AtomicU64::new(0),
            routes,
            recent_events: Mutex::new(VecDeque::with_capacity(OPS_RECENT_EVENTS_CAP)),
            time_series: Mutex::new(VecDeque::with_capacity(OPS_TIME_SERIES_CAP)),
        }
    }

    pub fn global() -> &'static Arc<OpsTelemetryRegistry> {
        GLOBAL_OPS_TELEMETRY.get_or_init(|| Arc::new(Self::new()))
    }

    fn record_with_poll_flag(
        &self,
        method: &str,
        raw_path: &str,
        status: u16,
        duration_us: u64,
        is_background_poll: bool,
    ) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        match status {
            200..=299 => {
                self.status_2xx.fetch_add(1, Ordering::Relaxed);
            }
            300..=399 => {
                self.status_3xx.fetch_add(1, Ordering::Relaxed);
            }
            400..=499 => {
                self.status_4xx.fetch_add(1, Ordering::Relaxed);
            }
            500..=599 => {
                self.status_5xx.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }

        let sanitized_path = sanitize_telemetry_path(raw_path);
        let bucket = classify_route_bucket(&sanitized_path);
        if let Some(slot) = self.routes.get(bucket as usize) {
            slot.record(status, duration_us);
        }

        // Update 10-second rolling throughput/latency buckets.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let bucket_ts = (now_secs / TIME_BUCKET_SECS) * TIME_BUCKET_SECS;
        let is_err = status >= 400;
        if let Ok(mut ts) = self.time_series.lock() {
            if let Some(last) = ts.back_mut().filter(|e| e.unix_bucket == bucket_ts) {
                last.requests += 1;
                if is_err {
                    last.errors += 1;
                }
                last.total_duration_us += duration_us;
            } else {
                let label = chrono::DateTime::<chrono::Utc>::from_timestamp(bucket_ts, 0)
                    .map(|dt| dt.format("%H:%M:%S").to_string())
                    .unwrap_or_else(|| "00:00:00".to_string());
                if ts.len() >= OPS_TIME_SERIES_CAP {
                    ts.pop_front();
                }
                ts.push_back(TimeSeriesEntry {
                    unix_bucket: bucket_ts,
                    label,
                    requests: 1,
                    errors: u64::from(is_err),
                    total_duration_us: duration_us,
                });
            }
        }

        // Keep `recent_events` free of background 5s dashboard polling noise so live app traffic stays visible.
        if !is_background_poll {
            let safe_method: String = method
                .chars()
                .filter(|c| c.is_ascii_alphabetic())
                .take(10)
                .collect::<String>()
                .to_ascii_uppercase();
            let ev = RecentRequestEvent {
                method: if safe_method.is_empty() {
                    "GET".to_string()
                } else {
                    safe_method
                },
                path: sanitized_path,
                route_bucket: bucket.label().to_string(),
                status,
                duration_ms: round_n(duration_us as f64 / 1000.0, 2),
                timestamp: now_iso(),
            };
            if let Ok(mut recent) = self.recent_events.lock() {
                if recent.len() >= OPS_RECENT_EVENTS_CAP {
                    recent.pop_front();
                }
                recent.push_back(ev);
            }
        }
    }

    pub fn record_request(&self, method: &str, raw_path: &str, status: u16, duration_us: u64) {
        self.record_with_poll_flag(method, raw_path, status, duration_us, false);
    }

    pub fn snapshot(&self) -> OpsHttpSnapshot {
        let total = self.total_requests.load(Ordering::Relaxed);
        let c2 = self.status_2xx.load(Ordering::Relaxed);
        let c3 = self.status_3xx.load(Ordering::Relaxed);
        let c4 = self.status_4xx.load(Ordering::Relaxed);
        let c5 = self.status_5xx.load(Ordering::Relaxed);
        let error_rate_pct = if total > 0 {
            round_n(((c4 + c5) as f64 / total as f64) * 100.0, 2)
        } else {
            0.0
        };

        let routes = self.routes.iter().map(RouteMetricsSlot::snapshot).collect();
        let time_series = self
            .time_series
            .lock()
            .map(|ts| {
                ts.iter()
                    .map(|e| TimeSeriesPoint {
                        label: e.label.clone(),
                        unix_bucket: e.unix_bucket,
                        requests: e.requests,
                        errors: e.errors,
                        avg_ms: if e.requests > 0 {
                            round_n((e.total_duration_us as f64 / e.requests as f64) / 1000.0, 2)
                        } else {
                            0.0
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Newest events first for operator scanability
        let recent_events = self
            .recent_events
            .lock()
            .map(|recent| recent.iter().rev().cloned().collect())
            .unwrap_or_default();

        OpsHttpSnapshot {
            started_at: self.started_at.clone(),
            uptime_secs: self.started_instant.elapsed().as_secs(),
            total_requests: total,
            in_flight_requests: self.in_flight_requests.load(Ordering::Relaxed),
            status_counts: StatusClassCounts {
                c2xx: c2,
                c3xx: c3,
                c4xx: c4,
                c5xx: c5,
            },
            error_rate_pct,
            routes,
            time_series,
            recent_events,
        }
    }
}

/// RAII guard so `in_flight_requests` always decrements even if Axum drops the future on client disconnect.
struct InFlightGuard<'a>(&'a AtomicU64);

impl<'a> InFlightGuard<'a> {
    fn enter(counter: &'a AtomicU64) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Axum middleware recording request counts, status classes, sanitized paths, and per-route P50/P95 latency.
pub async fn ops_telemetry_middleware(req: Request, next: Next) -> Response {
    let reg = OpsTelemetryRegistry::global();
    let _in_flight = InFlightGuard::enter(&reg.in_flight_requests);
    let method = req.method().as_str().to_string();
    let raw_path = req.uri().path().to_string();
    let is_poll = (raw_path == "/api/ops/telemetry" || raw_path == "/api/admin/overview")
        && req
            .uri()
            .query()
            .map(|q| q.split('&').any(|pair| pair == "poll=1"))
            .unwrap_or(false);

    let started = Instant::now();
    let resp = next.run(req).await;
    let duration_us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
    let status = resp.status().as_u16();

    reg.record_with_poll_flag(&method, &raw_path, status, duration_us, is_poll);
    resp
}

// =================================================================================================
// Database, Storage, Memory & Connector Telemetry Aggregators
// =================================================================================================

async fn query_scalar_i64(db: &DbStore, sql: &str) -> anyhow::Result<i64> {
    let val = match &db.backend {
        DbBackend::Sqlite(pool) => sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await?,
        DbBackend::MySql { pool, .. } => sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await?,
    };
    Ok(val)
}

async fn safe_table_count(db: &DbStore, table: &str) -> i64 {
    debug_assert!(
        table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "unsafe table identifier"
    );
    let sql = format!("SELECT COUNT(*) FROM {table}");
    query_scalar_i64(db, &sql).await.unwrap_or(0)
}

async fn safe_int_status_breakdown<F>(db: &DbStore, sql: &str, decode_fn: F) -> HashMap<String, i64>
where
    F: Fn(i32) -> &'static str,
{
    let mut map = HashMap::new();
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            if let Ok(rows) = sqlx::query(sql).fetch_all(pool).await {
                for r in rows {
                    let cnt = crate::db::sqlite_opt_i64(&r, "cnt").unwrap_or(0);
                    if let Some(code) = crate::db::sqlite_opt_i64(&r, "status") {
                        let key = decode_fn(code as i32).to_ascii_lowercase();
                        *map.entry(key).or_insert(0) += cnt;
                    } else if let Some(st) = crate::db::sqlite_opt_str(&r, "status") {
                        *map.entry(st.to_ascii_lowercase()).or_insert(0) += cnt;
                    }
                }
            }
        }
        DbBackend::MySql { pool, .. } => {
            if let Ok(rows) = sqlx::query(sql).fetch_all(pool).await {
                for r in rows {
                    let cnt = crate::db::mysql_opt_i64(&r, "cnt").unwrap_or(0);
                    if let Some(code) = crate::db::mysql_opt_i64(&r, "status") {
                        let key = decode_fn(code as i32).to_ascii_lowercase();
                        *map.entry(key).or_insert(0) += cnt;
                    } else if let Some(st) = crate::db::mysql_opt_str(&r, "status") {
                        *map.entry(st.to_ascii_lowercase()).or_insert(0) += cnt;
                    }
                }
            }
        }
    }
    map
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecentReportSummary {
    slug: String,
    display_address: String,
    zip: Option<String>,
    updated_at: String,
    overall_score: Option<f64>,
    estimated_value: Option<i64>,
    property_type: Option<String>,
    ai_ready: bool,
    runtime_ssr: bool,
    compressed: bool,
    has_html: bool,
    report_url: String,
    command_center_url: String,
}

async fn fetch_recent_reports(db: &DbStore, limit: i64) -> Vec<RecentReportSummary> {
    let safe_limit = limit.clamp(1, 50) as usize;
    let Ok((rows, _)) = db.list_admin_reports(None, safe_limit, 0).await else {
        return Vec::new();
    };
    rows.into_iter()
        .filter(|r| !r.slug.is_empty())
        .map(|r| {
            let slug = r.slug;
            let clean_addr = normalize_retired_brand(if r.display_address.trim().is_empty() {
                &slug
            } else {
                &r.display_address
            });
            RecentReportSummary {
                report_url: format!("/r/{slug}"),
                command_center_url: format!("/r/{slug}?view=dashboard"),
                slug,
                display_address: clean_addr,
                zip: r.zip,
                updated_at: r.updated_at,
                overall_score: r.overall_score.map(|x| round_n(x, 1)),
                estimated_value: r.estimated_value,
                property_type: r
                    .property_type
                    .map(|s| normalize_retired_brand(&s))
                    .filter(|s| !s.trim().is_empty()),
                ai_ready: r.ai_ready,
                runtime_ssr: true,
                compressed: r.compressed,
                has_html: r.has_rendered_html,
            }
        })
        .collect()
}

async fn collect_database_storage_info(db: &DbStore) -> Value {
    let connected = db.ping().await.is_ok();
    let dialect = db.dialect();
    let heatwave = false;
    let engine = match &db.backend {
        DbBackend::Sqlite(_) => "SQLite WAL",
        DbBackend::MySql { .. } => "MySQL 8/9",
    };

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut db_file_path: Option<String> = None;
            if let Ok(rows) = sqlx::query("PRAGMA database_list").fetch_all(pool).await {
                for r in rows {
                    let name = r.try_get::<String, _>(1).unwrap_or_default();
                    if name == "main" {
                        let f = r.try_get::<String, _>(2).unwrap_or_default();
                        if !f.is_empty() {
                            db_file_path = Some(f);
                        }
                        break;
                    }
                }
            }
            let page_count = sqlx::query_scalar::<_, i64>("PRAGMA page_count")
                .fetch_one(pool)
                .await
                .unwrap_or(0)
                .max(0) as u64;
            let page_size = sqlx::query_scalar::<_, i64>("PRAGMA page_size")
                .fetch_one(pool)
                .await
                .unwrap_or(4096)
                .max(0) as u64;
            let logical_bytes = page_count.saturating_mul(page_size);

            if let Some(ref path_str) = db_file_path {
                let db_bytes = std::fs::metadata(path_str)
                    .map(|m| m.len())
                    .unwrap_or(logical_bytes);
                let wal_bytes = std::fs::metadata(format!("{path_str}-wal"))
                    .map(|m| m.len())
                    .unwrap_or(0);
                let shm_bytes = std::fs::metadata(format!("{path_str}-shm"))
                    .map(|m| m.len())
                    .unwrap_or(0);
                let total_disk_bytes = db_bytes + wal_bytes + shm_bytes;
                let file_name = Path::new(path_str)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("openestate.sqlite")
                    .to_string();
                json!({
                    "connected": connected,
                    "dialect": dialect,
                    "engine": engine,
                    "heatwave": heatwave,
                    "inMemory": false,
                    "fileName": file_name,
                    "dbBytes": db_bytes,
                    "walBytes": wal_bytes,
                    "totalDiskBytes": total_disk_bytes,
                    "logicalBytes": logical_bytes,
                    "totalMb": round_n(total_disk_bytes as f64 / (1024.0 * 1024.0), 3),
                })
            } else {
                json!({
                    "connected": connected,
                    "dialect": dialect,
                    "engine": engine,
                    "heatwave": heatwave,
                    "inMemory": true,
                    "fileName": ":memory:",
                    "dbBytes": logical_bytes,
                    "walBytes": 0,
                    "totalDiskBytes": logical_bytes,
                    "logicalBytes": logical_bytes,
                    "totalMb": round_n(logical_bytes as f64 / (1024.0 * 1024.0), 3),
                })
            }
        }
        DbBackend::MySql { pool, .. } => {
            let total_bytes = sqlx::query_scalar::<_, i64>(
                "SELECT CAST(COALESCE(SUM(data_length + index_length), 0) AS SIGNED) FROM information_schema.TABLES WHERE table_schema = DATABASE()",
            )
            .fetch_one(pool)
            .await
            .unwrap_or(0)
            .max(0) as u64;
            json!({
                "connected": connected,
                "dialect": dialect,
                "engine": engine,
                "heatwave": heatwave,
                "inMemory": false,
                "fileName": Value::Null,
                "dbBytes": total_bytes,
                "walBytes": 0,
                "totalDiskBytes": total_bytes,
                "logicalBytes": total_bytes,
                "totalMb": round_n(total_bytes as f64 / (1024.0 * 1024.0), 3),
            })
        }
    }
}

/// Reads Linux `/proc/self/status` (`VmRSS`, `VmHWM`, `Threads`) with `/proc/self/statm` fallback.
/// Returns `null` fields cleanly on non-Linux hosts or restricted containers.
pub fn read_process_memory_info() -> Value {
    let mut rss_bytes: Option<u64> = None;
    let mut peak_rss_bytes: Option<u64> = None;
    let mut threads: Option<u64> = None;

    if let Ok(status_txt) = std::fs::read_to_string("/proc/self/status") {
        for line in status_txt.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    rss_bytes = Some(kb.saturating_mul(1024));
                }
            } else if let Some(rest) = line.strip_prefix("VmHWM:") {
                if let Some(kb) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    peak_rss_bytes = Some(kb.saturating_mul(1024));
                }
            } else if let Some(rest) = line.strip_prefix("Threads:") {
                if let Some(t) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    threads = Some(t);
                }
            }
        }
    }

    if rss_bytes.is_none() {
        if let Ok(statm_txt) = std::fs::read_to_string("/proc/self/statm") {
            if let Some(pages) = statm_txt
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u64>().ok())
            {
                rss_bytes = Some(pages.saturating_mul(4096));
            }
        }
    }

    let rss_mb = rss_bytes.map(|b| round_n(b as f64 / (1024.0 * 1024.0), 2));
    let peak_rss_mb = peak_rss_bytes.map(|b| round_n(b as f64 / (1024.0 * 1024.0), 2));

    json!({
        "rssBytes": rss_bytes,
        "rssMb": rss_mb,
        "peakRssMb": peak_rss_mb,
        "threads": threads,
        "os": std::env::consts::OS,
    })
}

fn measure_disk_cache_bounded(dir: Option<&Path>) -> (bool, u64, f64, bool) {
    let Some(path) = dir else {
        return (false, 0, 0.0, false);
    };
    if !path.is_dir() {
        return (true, 0, 0.0, false);
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return (true, 0, 0.0, false);
    };
    const MAX_SCAN: u64 = 5_000;
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    let mut capped = false;
    for entry in entries.flatten() {
        if count >= MAX_SCAN {
            capped = true;
            break;
        }
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                count += 1;
                bytes = bytes.saturating_add(meta.len());
            }
        }
    }
    let mb = round_n(bytes as f64 / (1024.0 * 1024.0), 2);
    (true, count, mb, capped)
}

/// Builds a sanitized upstream connector status catalog (never exposes keys, URLs with tokens, or proxy IPs).
pub fn build_connector_catalog(env: &EnvConfig) -> Vec<Value> {
    let strict = env.is_commercial_strict();
    let items = [
        (
            "census_geocoder",
            "U.S. Census Geocoder",
            "geocoding",
            "keyless",
            true,
        ),
        (
            "county_arcgis_assessor",
            "County ArcGIS & Assessor Roll",
            "parcels",
            "keyless",
            true,
        ),
        (
            "fema_nfip_flood",
            "FEMA NFIP Flood & NRI",
            "hazards",
            "keyless",
            true,
        ),
        (
            "usgs_seismic_lidar",
            "USGS Seismic & 3DEP LiDAR",
            "hazards",
            "keyless",
            true,
        ),
        (
            "epa_envirofacts",
            "EPA Envirofacts & Air Quality",
            "environment",
            "keyless",
            true,
        ),
        (
            "nces_schools_fbi",
            "NCES Schools & FBI Crime",
            "civic",
            "keyless",
            true,
        ),
        (
            "zillow_freddie_market",
            "ZIP Home-Value Index & Freddie Mac PMMS",
            "market",
            if strict { "clean_room" } else { "keyless" },
            !strict,
        ),
        (
            "osm_overpass_osrm",
            "OSM Overpass & OSRM Routing",
            "livability",
            "keyless",
            true,
        ),
        (
            "ai_synthesis_engine",
            "AI Synthesis Engine (Purpose-Locked)",
            "ai",
            "token",
            env.llm_configured(),
        ),
        (
            "reso_idx",
            "RESO Web API 2.0 (IDX)",
            "mls",
            "token",
            env.get("RESO_API_URL").is_some() && env.get("RESO_BEARER_TOKEN").is_some(),
        ),
        (
            "attom_data",
            "ATTOM Property & AVM",
            "mls",
            "api_key",
            env.get("ATTOM_API_KEY").is_some(),
        ),
        (
            "zoneomics_zoning",
            "Zoneomics Zoning & ADU",
            "zoning",
            "api_key",
            env.zoneomics_configured(),
        ),
        (
            "smtp_alerts",
            "SMTP Alert Mailer",
            "alerts",
            "credentials",
            env.smtp_configured(),
        ),
    ];

    items
        .into_iter()
        .map(|(id, name, category, auth_mode, configured)| {
            let status = if configured {
                "ready"
            } else if id == "zillow_freddie_market" && strict {
                "clean_room_restricted"
            } else {
                "standby"
            };
            json!({
                "id": id,
                "name": name,
                "category": category,
                "authMode": auth_mode,
                "configured": configured,
                "status": status,
            })
        })
        .collect()
}

fn build_sanitized_egress_summary(state: &AppState) -> Value {
    let raw = state.http.egress_status_json();
    let mode = raw.get("mode").and_then(Value::as_str).unwrap_or("direct");
    let split = raw
        .get("splitTrafficEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dynamic_active = raw
        .get("dynamicProxiesActive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let strict_egress = raw
        .get("strictProxyEgress")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Aggregate slot metrics only — NEVER expose slot labels/URLs or sample proxy IPs.
    let mut total_slots = 0_u64;
    let mut healthy_slots = 0_u64;
    let mut egress_requests = 0_u64;
    let mut egress_failovers = 0_u64;
    if let Some(slots) = raw.get("slots").and_then(Value::as_array) {
        total_slots = slots.len() as u64;
        for s in slots {
            if s.get("healthy").and_then(Value::as_bool).unwrap_or(false) {
                healthy_slots += 1;
            }
            egress_requests += s.get("requests").and_then(Value::as_u64).unwrap_or(0);
            egress_failovers += s.get("failovers").and_then(Value::as_u64).unwrap_or(0);
        }
    }

    let pool = raw.get("freeProxyPool");
    let pool_healthy = pool
        .and_then(|p| p.get("healthy_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let pool_working = pool
        .and_then(|p| p.get("working_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let pool_avg_lat = pool
        .and_then(|p| p.get("avg_latency_ms"))
        .and_then(Value::as_f64);

    json!({
        "guardrailsActive": true,
        "shield": state.shield.metrics_json(),
        "egressMode": mode,
        "splitTrafficEnabled": split,
        "dynamicProxiesActive": dynamic_active,
        "strictProxyEgress": strict_egress,
        "healthySlots": healthy_slots,
        "totalSlots": total_slots,
        "egressRequests": egress_requests,
        "egressFailovers": egress_failovers,
        "proxyPoolHealthy": pool_healthy,
        "proxyPoolWorking": pool_working,
        "proxyPoolAvgLatencyMs": pool_avg_lat,
    })
}

/// Builds the full non-sensitive `/api/ops/telemetry` JSON snapshot.
pub async fn build_ops_telemetry_snapshot(state: &AppState) -> Value {
    let http_snap = OpsTelemetryRegistry::global().snapshot();
    let now_unix = now_unix_secs();

    // Run independent DB & filesystem probes concurrently.
    let listings_status_sql =
        "SELECT status, COUNT(*) AS cnt FROM listings GROUP BY status".to_string();
    let (
        db_info,
        reports_total,
        recent_reports,
        job_status_map,
        llm_cache_entries,
        llm_cache_active,
        doc_reviews_cached,
        indexed_homes_total,
        listing_status_map,
        watched_zips_active,
        listing_factors_count,
        school_metrics_count,
        deal_rooms_count,
        search_history_entries,
        subscriptions_total,
        subscriptions_active,
        notifications_sent_total,
        property_likes_total,
        osrm_degraded_reports,
        osrm_health,
    ) = tokio::join!(
        collect_database_storage_info(&state.db),
        safe_table_count(&state.db, "cached_reports"),
        fetch_recent_reports(&state.db, 10),
        safe_int_status_breakdown(
            &state.db,
            "SELECT status, COUNT(*) AS cnt FROM report_build_queue GROUP BY status",
            crate::db::proto::build_job_status_from_code,
        ),
        safe_table_count(&state.db, "llm_cache"),
        async {
            let sql = format!("SELECT COUNT(*) FROM llm_cache WHERE expires_ts > {now_unix}");
            query_scalar_i64(&state.db, &sql).await.unwrap_or(0)
        },
        safe_table_count(&state.db, "client_doc_reviews"),
        safe_table_count(&state.db, "listings"),
        safe_int_status_breakdown(
            &state.db,
            &listings_status_sql,
            crate::db::proto::listing_status_from_code,
        ),
        async {
            query_scalar_i64(
                &state.db,
                "SELECT COUNT(*) FROM watched_zips WHERE active = 1",
            )
            .await
            .unwrap_or(0)
        },
        safe_table_count(&state.db, "listing_factors"),
        safe_table_count(&state.db, "school_metrics"),
        async {
            query_scalar_i64(
                &state.db,
                "SELECT COUNT(DISTINCT actor_key) FROM user_search_history",
            )
            .await
            .unwrap_or(0)
        },
        safe_table_count(&state.db, "user_search_history"),
        safe_table_count(&state.db, "subscriptions"),
        async {
            query_scalar_i64(
                &state.db,
                "SELECT COUNT(*) FROM subscriptions WHERE active = 1",
            )
            .await
            .unwrap_or(0)
        },
        safe_table_count(&state.db, "subscription_notifications"),
        safe_table_count(&state.db, "property_likes"),
        async { state.db.count_osrm_degraded_reports().await.unwrap_or(0) },
        state
            .osrm_recovery
            .health_snapshot(&state.http, &state.env, Duration::from_secs(30)),
    );

    let jobs_queued =
        *job_status_map.get("queued").unwrap_or(&0) + *job_status_map.get("pending").unwrap_or(&0);
    let jobs_ai_pending = *job_status_map.get("ai_pending").unwrap_or(&0);
    let jobs_running = *job_status_map.get("building").unwrap_or(&0)
        + *job_status_map.get("running").unwrap_or(&0)
        + *job_status_map.get("in_progress").unwrap_or(&0)
        + jobs_ai_pending;
    let jobs_done = *job_status_map.get("complete").unwrap_or(&0)
        + *job_status_map.get("done").unwrap_or(&0)
        + *job_status_map.get("ready").unwrap_or(&0)
        + *job_status_map.get("completed").unwrap_or(&0);
    let jobs_error =
        *job_status_map.get("failed").unwrap_or(&0) + *job_status_map.get("error").unwrap_or(&0);
    let jobs_total: i64 = job_status_map.values().sum();

    let mut mem_active = 0_usize;
    let mem_tracked = state.job_mgr.jobs_by_id.len();
    for entry in state.job_mgr.jobs_by_id.iter() {
        let snap = entry.value().snapshot();
        if !snap.done && snap.error.is_none() {
            mem_active += 1;
        }
    }

    let for_sale = *listing_status_map.get("for_sale").unwrap_or(&0);
    let pending = *listing_status_map.get("pending").unwrap_or(&0);
    let sold = *listing_status_map.get("sold").unwrap_or(&0);

    let scan_metrics = json!({});
    let tile_hits = scan_metrics
        .get("tileCacheHits")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tile_misses = scan_metrics
        .get("tileCacheUpstreamCalls")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tile_waiters = scan_metrics
        .get("tileCacheCoalescedWaiters")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let dedup_ratio = scan_metrics
        .get("deduplicationRatio")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let (disk_cache_configured, disk_cache_files, disk_cache_mb, disk_cache_capped) =
        measure_disk_cache_bounded(state.http.disk_cache_dir());

    let l1_reports_entries = state.db.l1_reports.entry_count();
    let l1_llm_entries = state.db.l1_llm.entry_count();
    let http_l1_entries = state.http.l1_cache().entry_count();

    let sys_mem = read_process_memory_info();
    let connectors = build_connector_catalog(&state.env);
    let security_egress = build_sanitized_egress_summary(state);
    let (routing_fallback_summary, routing_fallback_items) =
        crate::modules::routing_providers::build_routing_fallback_telemetry(
            &state.env,
            &state.extension_hub,
        );

    json!({
        "ok": true,
        "generatedAt": now_iso(),
        "brand": {
            "name": state.env.brand_name(),
            "domain": state.env.brand_domain(),
        },
        "server": {
            "startedAt": http_snap.started_at,
            "uptimeSecs": http_snap.uptime_secs,
            "totalRequests": http_snap.total_requests,
            "inFlightRequests": http_snap.in_flight_requests,
            "statusCounts": http_snap.status_counts,
            "errorRatePct": http_snap.error_rate_pct,
        },
        "routes": http_snap.routes,
        "timeSeries": http_snap.time_series,
        "recentEvents": http_snap.recent_events,
        "database": db_info,
        "inventory": {
            "reportsTotal": reports_total,
            "recentReports": recent_reports,
            "reportJobs": {
                "queued": jobs_queued,
                "running": jobs_running,
                "aiPending": jobs_ai_pending,
                "done": jobs_done,
                "error": jobs_error,
                "total": jobs_total,
                "inMemoryActive": mem_active,
                "inMemoryTracked": mem_tracked,
            },
            "llmCacheEntries": llm_cache_entries,
            "llmCacheActiveEntries": llm_cache_active,
            "docReviewsCached": doc_reviews_cached,
            "indexedHomesTotal": indexed_homes_total,
            "indexedHomesByStatus": {
                "forSale": for_sale,
                "pending": pending,
                "sold": sold,
            },
            "watchedZipsActive": watched_zips_active,
            "listingFactorsCount": listing_factors_count,
            "schoolMetricsCount": school_metrics_count,
            "dealRoomsCount": deal_rooms_count,
            "searchHistoryEntries": search_history_entries,
            "alertSubscriptionsTotal": subscriptions_total,
            "alertSubscriptionsActive": subscriptions_active,
            "notificationsSentTotal": notifications_sent_total,
            "propertyLikesTotal": property_likes_total,
            "osrmDegradedReports": osrm_degraded_reports,
        },
        "osrm": {
            "enabled": osrm_health.enabled,
            "degradedReports": osrm_degraded_reports,
            "probedAt": osrm_health.probed_at,
            "healthyRegions": osrm_health.healthy_regions,
            "totalRegions": osrm_health.total_regions,
            "regions": osrm_health.regions,
            "lastRecovery": osrm_health.last_recovery,
            "fallbackPool": routing_fallback_summary,
        },
        "routingFallbackPool": routing_fallback_items,
        "cacheEfficiency": {
            "l1ReportsEntries": l1_reports_entries,
            "l1LlmEntries": l1_llm_entries,
            "httpL1Entries": http_l1_entries,
            "httpDiskCacheConfigured": disk_cache_configured,
            "httpDiskCacheFiles": disk_cache_files,
            "httpDiskCacheMb": disk_cache_mb,
            "httpDiskCacheCapped": disk_cache_capped,
            "tileCacheHits": tile_hits,
            "tileCacheUpstreamCalls": tile_misses,
            "tileCacheCoalescedWaiters": tile_waiters,
            "deduplicationRatio": dedup_ratio,
            "dedupPct": round_n(dedup_ratio * 100.0, 1),
            "scanEngine": scan_metrics,
        },
        "system": sys_mem,
        "securityAndEgress": security_egress,
        "connectors": connectors,
    })
}

// =================================================================================================
// Sanctioned Admin & Ops Access Control (`OPEN_ESTATE_OPS_TOKEN` / Bearer / Header / Cookie / Loopback)
// =================================================================================================

#[derive(Debug, Clone)]
pub struct AdminAuthContext {
    pub actor: String,
    pub via_token: bool,
    pub client_ip: String,
}

#[derive(Debug, Clone)]
pub struct AdminAuthError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: &'static str,
    pub token_configured: bool,
}

pub fn configured_ops_admin_token(env: &EnvConfig) -> Option<String> {
    for key in [
        "OPEN_ESTATE_OPS_TOKEN",
        "OPEN_ESTATE_ADMIN_TOKEN",
        "ADMIN_TOKEN",
    ] {
        if let Some(val) = env.get(key).map(str::trim).filter(|s| !s.is_empty()) {
            return Some(val.to_string());
        }
    }
    None
}

pub fn extract_ops_admin_token(headers: &HeaderMap) -> Option<(String, &'static str)> {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        if auth.len() > 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
            let tok = auth[7..].trim();
            if !tok.is_empty() {
                return Some((tok.to_string(), "bearer"));
            }
        }
    }
    for hdr_name in ["x-openestate-ops-token", "x-openestate-admin-token"] {
        if let Some(tok) = headers
            .get(hdr_name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some((tok.to_string(), "header"));
        }
    }
    if let Some(cookie_hdr) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for part in cookie_hdr.split(';') {
            let trimmed = part.trim();
            for prefix in ["open_estate_ops_token=", "open_estate_admin_token="] {
                if let Some(val) = trimmed.strip_prefix(prefix) {
                    let clean = val.trim().trim_matches('"');
                    if !clean.is_empty() {
                        return Some((clean.to_string(), "cookie"));
                    }
                }
            }
        }
    }
    None
}

fn sanitize_actor_label(raw: Option<&str>, fallback: &str) -> String {
    if let Some(r) = raw {
        let cleaned: String = r
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | ':'))
            .take(64)
            .collect();
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    fallback.to_string()
}

pub fn verify_ops_admin_access(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> Result<AdminAuthContext, AdminAuthError> {
    let proxy_cfg = crate::web::admin_console::ProxyAuthConfig::from_env(&state.env);
    if proxy_cfg.enabled && proxy_cfg.only {
        return Err(AdminAuthError {
            status: StatusCode::UNAUTHORIZED,
            code: "PROXY_AUTH_REQUIRED",
            message: "Authenticated reverse-proxy credentials (user header and X-Proxy-Auth-Secret) are required.",
            token_configured: false,
        });
    }

    let client_ip = resolve_client_ip_from_req(state, headers, extensions);
    let actor_header = headers.get("x-admin-actor").and_then(|v| v.to_str().ok());

    if let Some(expected_token) = configured_ops_admin_token(&state.env) {
        if let Some((candidate, source)) = extract_ops_admin_token(headers) {
            if constant_time_eq(candidate.as_bytes(), expected_token.as_bytes()) {
                let default_actor = format!("operator:{source}");
                return Ok(AdminAuthContext {
                    actor: sanitize_actor_label(actor_header, &default_actor),
                    via_token: true,
                    client_ip,
                });
            }
        }
        return Err(AdminAuthError {
            status: StatusCode::UNAUTHORIZED,
            code: "ADMIN_AUTH_REQUIRED",
            message: "Valid OPEN_ESTATE_OPS_TOKEN (Bearer header, X-OpenEstate-Ops-Token, or session cookie) is required.",
            token_configured: true,
        });
    }

    let is_loopback_ip = client_ip
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false);
    let is_in_process_harness =
        client_ip == "unknown" && extensions.get::<ConnectInfo<SocketAddr>>().is_none();

    if is_loopback_ip || is_in_process_harness {
        let default_actor = if is_loopback_ip {
            format!("loopback:{client_ip}")
        } else {
            "loopback:local".to_string()
        };
        return Ok(AdminAuthContext {
            actor: sanitize_actor_label(actor_header, &default_actor),
            via_token: false,
            client_ip,
        });
    }

    Err(AdminAuthError {
        status: StatusCode::FORBIDDEN,
        code: "ADMIN_LOOPBACK_ONLY",
        message: "Admin and Ops endpoints are restricted to loopback connections unless OPEN_ESTATE_OPS_TOKEN is configured.",
        token_configured: false,
    })
}

pub fn admin_auth_error_json_response(err: &AdminAuthError) -> Response {
    let mut resp = (
        err.status,
        Json(json!({
            "ok": false,
            "code": err.code,
            "error": err.message,
            "tokenConfigured": err.token_configured,
        })),
    )
        .into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

/// `GET /api/ops/telemetry` — returns live non-sensitive platform ops, latency, cache, and connector JSON.
pub async fn handle_ops_telemetry_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(auth_err) =
        crate::web::admin_console::verify_admin_access(&state, &headers, &extensions).await
    {
        return admin_auth_error_json_response(&auth_err);
    }
    let payload = build_ops_telemetry_snapshot(&state).await;
    let mut resp = (StatusCode::OK, Json(payload)).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

// =================================================================================================
// Self-Contained Responsive HTML Control Panel (`GET /ops`)
// =================================================================================================

fn format_uptime_ssr(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h:02}h {m:02}m")
    } else if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else {
        format!("{m}m {s:02}s")
    }
}

/// `GET /ops` — renders the Platform Ops & Telemetry Control Panel.
pub async fn handle_ops_dashboard_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(auth_err) =
        crate::web::admin_console::verify_admin_access(&state, &headers, &extensions).await
    {
        return admin_auth_error_json_response(&auth_err);
    }
    let snapshot = build_ops_telemetry_snapshot(&state).await;
    let html = render_ops_dashboard_html(&snapshot, &state.env.brand_name());
    let mut resp = (StatusCode::OK, Html(html)).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

fn render_ssr_connectors(snapshot: &Value) -> String {
    let Some(items) = snapshot.pointer("/connectors").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for c in items {
        let configured = c
            .get("configured")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let status = c.get("status").and_then(Value::as_str).unwrap_or("standby");
        let name = encode_text(c.get("name").and_then(Value::as_str).unwrap_or("Connector"));
        let cat = encode_text(c.get("category").and_then(Value::as_str).unwrap_or(""));
        let mode = encode_text(c.get("authMode").and_then(Value::as_str).unwrap_or(""));
        let ready_cls = if configured { " ready" } else { "" };
        let (badge_cls, badge_txt) = if configured {
            ("pill-good", "Ready")
        } else if status == "clean_room_restricted" {
            ("pill-warn", "Clean-Room")
        } else {
            ("pill-muted", "Standby")
        };
        out.push_str(&format!(
            r#"<div class="conn-card{ready_cls}"><div class="conn-top"><div class="conn-name">{name}</div><span class="pill {badge_cls}">{badge_txt}</span></div><div class="conn-meta"><span>{cat}</span><span class="mono">{mode}</span></div></div>"#
        ));
    }
    out
}

fn render_ssr_routes(snapshot: &Value) -> String {
    let Some(items) = snapshot.pointer("/routes").and_then(Value::as_array) else {
        return String::new();
    };
    let max_p95 = items
        .iter()
        .filter_map(|r| r.get("p95Ms").and_then(Value::as_f64))
        .fold(1.0_f64, f64::max);
    let mut out = String::new();
    for r in items {
        let route = encode_text(r.get("route").and_then(Value::as_str).unwrap_or("other"));
        let reqs = r.get("requests").and_then(Value::as_u64).unwrap_or(0);
        let errs = r.get("errors").and_then(Value::as_u64).unwrap_or(0);
        let mean_ms = r.get("meanMs").and_then(Value::as_f64).unwrap_or(0.0);
        let p50_ms = r.get("p50Ms").and_then(Value::as_f64).unwrap_or(0.0);
        let p95_ms = r.get("p95Ms").and_then(Value::as_f64).unwrap_or(0.0);
        let max_ms = r.get("maxMs").and_then(Value::as_f64).unwrap_or(0.0);
        let bar_pct = ((p95_ms / max_p95) * 100.0).clamp(0.0, 100.0);
        let err_cell = if errs > 0 {
            format!(r#"<span class="pill pill-warn">{errs}</span>"#)
        } else {
            "0".to_string()
        };
        out.push_str(&format!(
            r#"<tr><td class="mono" style="font-weight:700;color:#0f172a;white-space:nowrap">{route}</td><td class="mono">{reqs}</td><td class="mono">{err_cell}</td><td class="mono" style="white-space:nowrap">{mean_ms:.2} ms</td><td class="mono" style="white-space:nowrap">{p50_ms:.2} ms</td><td class="mono" style="white-space:nowrap">{p95_ms:.2} ms <span class="lat-bar-bg"><span class="lat-bar-fg" style="width:{bar_pct:.1}%"></span></span></td><td class="mono" style="white-space:nowrap">{max_ms:.2} ms</td></tr>"#
        ));
    }
    out
}

fn render_ssr_recent_reports(snapshot: &Value, brand_name: &str) -> String {
    let eff_brand = encode_text(effective_brand_name(brand_name));
    let Some(items) = snapshot
        .pointer("/inventory/recentReports")
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    if items.is_empty() {
        return format!(
            r#"<tr><td colspan="6" style="text-align:center;color:#64748b;padding:18px">No cached reports yet. Search an address on {eff_brand} to populate reports.</td></tr>"#
        );
    }
    let mut out = String::new();
    for r in items {
        let addr = encode_text(
            r.get("displayAddress")
                .and_then(Value::as_str)
                .unwrap_or("Property"),
        );
        let slug = encode_text(r.get("slug").and_then(Value::as_str).unwrap_or(""));
        let zip = r.get("zip").and_then(Value::as_str).unwrap_or("");
        let zip_suffix = if zip.is_empty() {
            String::new()
        } else {
            format!(" · {}", encode_text(zip))
        };
        let score_val = r.get("overallScore").and_then(Value::as_f64);
        let score_txt = score_val
            .map(|s| format!("{s:.0}/100"))
            .unwrap_or_else(|| "—".to_string());
        let score_cls = match score_val {
            Some(s) if s >= 75.0 => "pill-good",
            Some(s) if s >= 55.0 => "pill-info",
            _ => "pill-muted",
        };
        let val_txt = r
            .get("estimatedValue")
            .and_then(Value::as_f64)
            .filter(|v| *v > 0.0)
            .map(|v| format!("${:.0}", v.round()))
            .unwrap_or_else(|| "—".to_string());
        let ai_ready = r.get("aiReady").and_then(Value::as_bool).unwrap_or(false);
        let ai_cls = if ai_ready { "pill-good" } else { "pill-muted" };
        let ai_txt = if ai_ready { "AI Ready" } else { "Core" };
        let updated = encode_text(r.get("updatedAt").and_then(Value::as_str).unwrap_or("--"));
        let rep_url = encode_double_quoted_attribute(
            r.get("reportUrl").and_then(Value::as_str).unwrap_or("/"),
        );
        let cmd_url = encode_double_quoted_attribute(
            r.get("commandCenterUrl")
                .and_then(Value::as_str)
                .unwrap_or("/"),
        );
        out.push_str(&format!(
            r#"<tr><td><div style="font-weight:700;color:#0f172a">{addr}</div><div class="mono" style="font-size:11px;color:#64748b">{slug}{zip_suffix}</div></td><td><span class="pill {score_cls}">{score_txt}</span></td><td class="mono" style="white-space:nowrap">{val_txt}</td><td><span class="pill {ai_cls}">{ai_txt}</span></td><td class="mono" style="white-space:nowrap">{updated}</td><td><div class="act-links"><a class="act-btn" href="{rep_url}">Open Report ↗</a><a class="act-btn cmd" href="{cmd_url}">⚡ Buyer Command Center ↗</a></div></td></tr>"#
        ));
    }
    out
}

pub fn render_ops_dashboard_html(snapshot: &Value, brand_name: &str) -> String {
    let eff_brand = effective_brand_name(brand_name);
    let safe_brand = encode_text(eff_brand);
    let brand_wordmark = format_brand_wordmark_html(eff_brand);
    let brand_js = safe_inline_json(
        &serde_json::to_string(eff_brand).unwrap_or_else(|_| "\"Open Estate\"".into()),
    );
    let initial_json =
        safe_inline_json(&serde_json::to_string(snapshot).unwrap_or_else(|_| "{}".into()));
    let logo_svg = logo_mark_img(32);
    let favicon_tags = FAVICON_HEAD_TAGS.as_str();
    let ssr_connectors = render_ssr_connectors(snapshot);
    let ssr_routes = render_ssr_routes(snapshot);
    let ssr_reports = render_ssr_recent_reports(snapshot, eff_brand);

    let db_connected = snapshot
        .pointer("/database/connected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let err_rate = snapshot
        .pointer("/server/errorRatePct")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let uptime_secs = snapshot
        .pointer("/server/uptimeSecs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let uptime_str = format_uptime_ssr(uptime_secs);
    let total_req = snapshot
        .pointer("/server/totalRequests")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let in_flight = snapshot
        .pointer("/server/inFlightRequests")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reports_total = snapshot
        .pointer("/inventory/reportsTotal")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let indexed_homes = snapshot
        .pointer("/inventory/indexedHomesTotal")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let llm_cache = snapshot
        .pointer("/inventory/llmCacheEntries")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let db_engine = encode_text(
        snapshot
            .pointer("/database/engine")
            .and_then(Value::as_str)
            .unwrap_or("SQLite WAL"),
    );
    let rss_display = snapshot
        .pointer("/system/rssMb")
        .and_then(Value::as_f64)
        .map(|m| format!("{m:.1} MB"))
        .unwrap_or_else(|| "N/A".to_string());
    let status_class = if db_connected && err_rate < 15.0 {
        "ok"
    } else {
        "warn"
    };
    let status_label = if db_connected {
        "OPERATIONAL"
    } else {
        "DEGRADED"
    };

    format!(
        r###"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{safe_brand} — Platform Ops &amp; Telemetry</title>
{favicon_tags}
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700;800&family=JetBrains+Mono:wght@500;700&display=swap" rel="stylesheet">
<script src="https://cdn.jsdelivr.net/npm/echarts@5.5.1/dist/echarts.min.js"></script>
<style>
:root{{
  --cmd-bg:#0b1120;
  --cmd-surface:#0f172a;
  --cmd-surface2:#1e293b;
  --cmd-border:rgba(148,163,184,.18);
  --cmd-text:#f8fafc;
  --cmd-muted:#94a3b8;
  --canvas:#f1f5f9;
  --card:#ffffff;
  --ink:#0f172a;
  --ink2:#334155;
  --muted:#64748b;
  --line:#e2e8f0;
  --brand:#0f766e;
  --brand2:#14b8a6;
  --brand-soft:#ecfdf5;
  --blue:#2563eb;
  --amber:#d97706;
  --red:#dc2626;
  --green:#16a34a;
  --font:'Inter',system-ui,-apple-system,'Segoe UI',Roboto,'PingFang SC','Microsoft YaHei',sans-serif;
  --mono:'JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace;
  --radius:14px;
  --shadow:0 1px 3px rgba(15,23,42,.06),0 12px 28px -18px rgba(15,23,42,.18);
}}
*{{box-sizing:border-box}}
html,body{{margin:0;padding:0;max-width:100%;overflow-x:hidden;background:var(--canvas);color:var(--ink);font:14px/1.5 var(--font);-webkit-font-smoothing:antialiased}}
a{{color:var(--brand);text-decoration:none}}
a:hover{{text-decoration:underline}}
.mono{{font-family:var(--mono);font-variant-numeric:tabular-nums}}

/* Command Header Shell */
.cmd-shell{{
  background:radial-gradient(1100px 360px at 12% -30%,rgba(20,184,166,.22),transparent 62%),
             radial-gradient(900px 320px at 88% -20%,rgba(37,99,235,.16),transparent 60%),
             linear-gradient(180deg,var(--cmd-bg) 0%,var(--cmd-surface) 100%);
  color:var(--cmd-text);
  border-bottom:1px solid var(--cmd-border);
}}
.cmd-inner{{max-width:1400px;margin:0 auto;padding:16px 20px 14px}}
.cmd-top{{display:flex;align-items:center;justify-content:space-between;gap:12px;flex-wrap:wrap}}
.brand-group{{display:flex;align-items:center;gap:11px;min-width:0;flex-wrap:wrap}}
.logo-link{{display:inline-flex;align-items:center;gap:9px;color:#fff;font-weight:800;font-size:18px;letter-spacing:-.2px}}
.logo-link:hover{{text-decoration:none}}
.logo-link b{{color:#2dd4bf}}
.logo-mark{{width:32px;height:32px;border-radius:8px;flex:0 0 32px;display:block}}
.ops-tag{{
  font:700 11px/1 var(--mono);letter-spacing:.08em;text-transform:uppercase;
  padding:5px 9px;border-radius:999px;background:rgba(20,184,166,.14);
  border:1px solid rgba(45,212,191,.35);color:#5eead4;white-space:nowrap
}}
.status-led{{
  display:inline-flex;align-items:center;gap:6px;font:700 11px/1 var(--mono);
  padding:5px 10px;border-radius:999px;letter-spacing:.05em;white-space:nowrap
}}
.status-led.ok{{background:rgba(22,163,74,.16);border:1px solid rgba(74,222,128,.36);color:#86efac}}
.status-led.warn{{background:rgba(217,119,6,.18);border:1px solid rgba(251,191,36,.4);color:#fde68a}}
.led-dot{{width:7px;height:7px;border-radius:50%;background:currentColor;box-shadow:0 0 8px currentColor;animation:pulse 2.2s infinite}}
@keyframes pulse{{0%,100%{{opacity:1}}50%{{opacity:.45}}}}

.cmd-actions{{display:flex;align-items:center;gap:8px;flex-wrap:wrap}}
.cmd-btn{{
  display:inline-flex;align-items:center;gap:6px;padding:7px 12px;border-radius:10px;
  font:600 12.5px/1.2 var(--font);border:1px solid rgba(148,163,184,.26);
  background:rgba(30,41,59,.78);color:#e2e8f0;cursor:pointer;white-space:nowrap;transition:all .15s
}}
.cmd-btn:hover{{border-color:#2dd4bf;color:#fff;background:rgba(15,118,110,.35);text-decoration:none}}
.cmd-btn.active{{border-color:rgba(45,212,191,.6);background:rgba(15,118,110,.4);color:#99f6e4}}
.cmd-btn.primary{{background:#0f766e;border-color:#14b8a6;color:#fff}}
.cmd-btn.primary:hover{{background:#0d9488}}
.cmd-btn:disabled{{opacity:.6;cursor:wait}}
.lang-sw{{
  display:inline-flex;align-items:center;background:rgba(15,23,42,.85);
  border:1px solid rgba(148,163,184,.28);border-radius:999px;padding:2px;gap:2px
}}
.lang-sw button{{
  border:0;background:transparent;color:#94a3b8;font:700 11.5px var(--font);
  padding:5px 10px;border-radius:999px;cursor:pointer;transition:all .15s
}}
.lang-sw button.on{{background:#14b8a6;color:#042f2e}}

.cmd-strip{{
  margin-top:14px;padding-top:12px;border-top:1px solid var(--cmd-border);
  display:flex;align-items:center;justify-content:space-between;gap:12px;flex-wrap:wrap;
  font-size:12px;color:var(--cmd-muted)
}}
.strip-pills{{display:flex;align-items:center;gap:14px;flex-wrap:wrap}}
.strip-item{{display:inline-flex;align-items:center;gap:6px}}
.strip-item strong{{color:#f1f5f9;font-family:var(--mono);font-weight:700}}

/* Main Light-Card Workspace */
.wrap{{max-width:1400px;margin:0 auto;padding:20px 20px 56px}}
.kpi-grid{{display:grid;grid-template-columns:repeat(6,minmax(0,1fr));gap:12px;margin-bottom:16px}}
.grid-2{{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:14px;margin-bottom:16px}}
.grid-3{{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:14px;margin-bottom:16px}}
@media(max-width:1240px){{.kpi-grid{{grid-template-columns:repeat(3,minmax(0,1fr))}}}}
@media(max-width:980px){{.grid-2,.grid-3{{grid-template-columns:minmax(0,1fr)}}}}
@media(max-width:680px){{
  .cmd-inner,.wrap{{padding-left:12px;padding-right:12px}}
  .kpi-grid{{grid-template-columns:repeat(2,minmax(0,1fr));gap:10px}}
}}
@media(max-width:420px){{
  .kpi-grid{{grid-template-columns:minmax(0,1fr)}}
}}

.card{{
  min-width:0;background:var(--card);border:1px solid var(--line);border-radius:var(--radius);
  padding:16px;box-shadow:var(--shadow);overflow:hidden
}}
.kpi-card{{
  min-width:0;background:var(--card);border:1px solid var(--line);border-radius:var(--radius);
  padding:14px 15px;box-shadow:var(--shadow);position:relative
}}
.kpi-card::before{{
  content:"";position:absolute;top:0;left:0;right:0;height:3px;border-radius:var(--radius) var(--radius) 0 0;
  background:linear-gradient(90deg,var(--brand),var(--brand2))
}}
.kpi-lbl{{font-size:11.5px;font-weight:700;text-transform:uppercase;letter-spacing:.04em;color:var(--muted);margin-bottom:6px}}
.kpi-val{{font:800 23px/1.15 var(--mono);color:var(--ink);margin-bottom:6px;word-break:break-word}}
.kpi-sub{{font-size:12px;color:var(--ink2);display:flex;align-items:center;gap:6px;flex-wrap:wrap}}

.card-hdr{{display:flex;align-items:flex-start;justify-content:space-between;gap:10px;margin-bottom:12px;flex-wrap:wrap}}
.card-title{{margin:0;font-size:15px;font-weight:800;color:var(--ink);letter-spacing:-.15px}}
.card-sub{{margin:2px 0 0;font-size:12px;color:var(--muted)}}

.pill{{
  display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;
  font:700 11px/1.35 var(--mono);white-space:nowrap
}}
.pill-good{{background:#ecfdf5;color:#15803d;border:1px solid #bbf7d0}}
.pill-warn{{background:#fffbeb;color:#b45309;border:1px solid #fde68a}}
.pill-bad{{background:#fef2f2;color:#b91c1c;border:1px solid #fecaca}}
.pill-info{{background:#eff6ff;color:#1d4ed8;border:1px solid #bfdbfe}}
.pill-muted{{background:#f1f5f9;color:#475569;border:1px solid #cbd5e1}}

.chart-box{{width:100%;height:280px;min-height:240px;position:relative}}
.chart-box.compact{{height:215px;min-height:200px}}

/* Progress & Gauge Bars */
.meter-list{{display:flex;flex-direction:column;gap:10px}}
.meter-row{{display:flex;flex-direction:column;gap:4px}}
.meter-top{{display:flex;justify-content:space-between;align-items:center;font-size:12.5px;gap:8px}}
.meter-top span:first-child{{color:var(--ink2);font-weight:600}}
.meter-top span:last-child{{font-family:var(--mono);font-weight:700;color:var(--ink)}}
.meter-track{{height:8px;background:#e2e8f0;border-radius:999px;overflow:hidden}}
.meter-fill{{height:100%;border-radius:999px;background:linear-gradient(90deg,var(--brand),var(--brand2));transition:width .35s ease}}
.meter-fill.blue{{background:linear-gradient(90deg,#2563eb,#38bdf8)}}
.meter-fill.amber{{background:linear-gradient(90deg,#d97706,#fbbf24)}}

/* Connector Grid */
.conn-filters{{display:inline-flex;gap:4px;background:#f1f5f9;padding:3px;border-radius:9px;border:1px solid var(--line)}}
.conn-filters button{{border:0;background:transparent;padding:4px 10px;border-radius:7px;font:700 11.5px var(--font);color:var(--muted);cursor:pointer}}
.conn-filters button.on{{background:#fff;color:var(--brand);box-shadow:0 1px 2px rgba(15,23,42,.08)}}
.conn-grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:10px}}
@media(max-width:520px){{.conn-grid{{grid-template-columns:minmax(0,1fr)}}}}
.conn-card{{
  border:1px solid var(--line);border-radius:11px;padding:11px 12px;background:#f8fafc;
  display:flex;flex-direction:column;gap:6px;transition:border-color .15s,background .15s
}}
.conn-card.ready{{background:#fff;border-color:#cbd5e1}}
.conn-card:hover{{border-color:var(--brand2)}}
.conn-top{{display:flex;justify-content:space-between;align-items:flex-start;gap:8px}}
.conn-name{{font-weight:700;font-size:13px;color:var(--ink);line-height:1.3}}
.conn-meta{{display:flex;align-items:center;justify-content:space-between;gap:6px;font-size:11px;color:var(--muted)}}

/* Tables */
.tbl-wrap{{width:100%;max-width:100%;overflow-x:auto;-webkit-overflow-scrolling:touch;border:1px solid var(--line);border-radius:10px}}
table.ops-tbl{{width:100%;border-collapse:collapse;font-size:12.5px;background:#fff}}
table.ops-tbl th{{
  background:#f8fafc;color:var(--muted);font:700 11px/1.2 var(--font);text-transform:uppercase;
  letter-spacing:.04em;text-align:left;padding:9px 11px;border-bottom:1px solid var(--line);white-space:nowrap
}}
table.ops-tbl td{{padding:9px 11px;border-bottom:1px solid #f1f5f9;vertical-align:middle;color:var(--ink2)}}
table.ops-tbl tr:last-child td{{border-bottom:0}}
table.ops-tbl tr:hover td{{background:#f8fafc}}
.lat-bar-bg{{width:72px;height:6px;background:#e2e8f0;border-radius:999px;overflow:hidden;display:inline-block;vertical-align:middle;margin-left:6px}}
.lat-bar-fg{{height:100%;background:var(--brand2);border-radius:999px}}

.act-links{{display:flex;align-items:center;gap:6px;flex-wrap:wrap}}
.act-btn{{
  display:inline-flex;align-items:center;gap:4px;padding:5px 9px;border-radius:8px;
  font:700 11.5px/1.2 var(--font);border:1px solid var(--line);background:#fff;color:var(--ink);
  white-space:nowrap;transition:all .15s
}}
.act-btn:hover{{border-color:var(--brand);color:var(--brand);text-decoration:none;background:var(--brand-soft)}}
.act-btn.cmd{{border-color:#99f6e4;background:#f0fdfa;color:#0f766e}}
.act-btn.cmd:hover{{background:#ccfbf1}}

.inv-strip{{display:grid;grid-template-columns:repeat(auto-fill,minmax(145px,1fr));gap:8px;margin-top:12px}}
.inv-mini{{background:#f8fafc;border:1px solid var(--line);border-radius:10px;padding:9px 11px}}
.inv-mini .k{{font-size:11px;color:var(--muted);font-weight:600}}
.inv-mini .v{{font:800 16px/1.2 var(--mono);color:var(--ink);margin-top:2px}}

.foot-note{{margin-top:18px;font-size:12px;color:var(--muted);display:flex;justify-content:space-between;gap:10px;flex-wrap:wrap}}
</style>
</head>
<body>
<header class="cmd-shell">
  <div class="cmd-inner">
    <div class="cmd-top">
      <div class="brand-group">
        <a class="logo-link" href="/">{logo_svg}<span>{brand_wordmark}</span></a>
        <span class="ops-tag" data-i18n="opsTag">OPS TELEMETRY</span>
        <span id="healthBadge" class="status-led {status_class}"><span class="led-dot"></span><span id="healthBadgeText">{status_label}</span></span>
      </div>
      <div class="cmd-actions">
        <a class="cmd-btn" href="/" data-i18n="backHome">← Back to {safe_brand}</a>
        <a class="cmd-btn" href="/deal-room" data-i18n="dealRoom">Deal Room</a>
        <a class="cmd-btn" href="/admin">Admin Console</a>
        <button type="button" id="autoRefreshBtn" class="cmd-btn active" aria-pressed="true">
          <span class="led-dot"></span><span id="autoRefreshLabel">Auto-Refresh (5s)</span>
        </button>
        <button type="button" id="refreshNowBtn" class="cmd-btn primary" data-i18n="refreshNow">Refresh Now</button>
        <div class="lang-sw" id="langSwitch" role="group" aria-label="Language">
          <button type="button" data-lang="en" class="on">EN</button>
          <button type="button" data-lang="zh">中文</button>
          <button type="button" data-lang="es">ES</button>
        </div>
      </div>
    </div>
    <div class="cmd-strip">
      <div class="strip-pills">
        <span class="strip-item"><span data-i18n="stripUptime">Uptime</span>: <strong id="stripUptime">{uptime_str}</strong></span>
        <span class="strip-item"><span data-i18n="stripInFlight">In-Flight</span>: <strong id="stripInFlight">{in_flight}</strong></span>
        <span class="strip-item"><span data-i18n="stripDb">Storage</span>: <strong id="stripDb">{db_engine}</strong></span>
        <span class="strip-item"><span data-i18n="stripEgress">Egress</span>: <strong id="stripEgress">direct</strong></span>
        <span class="strip-item"><span data-i18n="stripMem">RSS</span>: <strong id="stripMem">{rss_display}</strong></span>
      </div>
      <div><span data-i18n="lastUpdated">Updated</span>: <span id="lastUpdatedAt" class="mono">--</span></div>
    </div>
  </div>
</header>

<main class="wrap">
  <section class="kpi-grid" aria-label="Platform KPIs">
    <div class="kpi-card">
      <div class="kpi-lbl" data-i18n="kpiReq">HTTP Requests</div>
      <div class="kpi-val" id="kpiTotalReq">{total_req}</div>
      <div class="kpi-sub" id="kpiReqSub"><span class="pill pill-good">2xx: 0</span><span class="pill pill-muted">4xx/5xx: 0</span></div>
    </div>
    <div class="kpi-card">
      <div class="kpi-lbl" data-i18n="kpiLatency">Peak Route P95</div>
      <div class="kpi-val" id="kpiP95">0.0 ms</div>
      <div class="kpi-sub" id="kpiLatencySub"><span class="pill pill-info">Mean: 0.0 ms</span></div>
    </div>
    <div class="kpi-card">
      <div class="kpi-lbl" data-i18n="kpiReports">Cached Reports</div>
      <div class="kpi-val" id="kpiReports">{reports_total}</div>
      <div class="kpi-sub" id="kpiQueueSub"><span class="pill pill-muted">Queue: 0</span></div>
    </div>
    <div class="kpi-card">
      <div class="kpi-lbl" data-i18n="kpiCache">AI &amp; L1 Cache</div>
      <div class="kpi-val" id="kpiLlmCache">{llm_cache}</div>
      <div class="kpi-sub" id="kpiCacheSub"><span class="pill pill-good">Dedup: 0%</span></div>
    </div>
    <div class="kpi-card">
      <div class="kpi-lbl" data-i18n="kpiMemory">Process Memory &amp; DB</div>
      <div class="kpi-val" id="kpiRss">{rss_display}</div>
      <div class="kpi-sub" id="kpiDbSub"><span class="pill pill-muted">DB: 0 MB</span></div>
    </div>
    <div class="kpi-card">
      <div class="kpi-lbl" data-i18n="kpiHomes">Indexed Homes &amp; Alerts</div>
      <div class="kpi-val" id="kpiHomes">{indexed_homes}</div>
      <div class="kpi-sub" id="kpiAlertsSub"><span class="pill pill-info">Alerts: 0</span></div>
    </div>
  </section>

  <section class="grid-2">
    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title" data-i18n="chartLatTitle">Route Latency Distribution (P50 / P95 / Mean ms)</h2>
          <p class="card-sub" data-i18n="chartLatSub">Rolling 256-sample latency ring per route bucket</p>
        </div>
        <span class="pill pill-info">ms</span>
      </div>
      <div id="latencyRouteChart" class="chart-box"></div>
    </div>
    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title" data-i18n="chartTrendTitle">Throughput &amp; Latency Timeline</h2>
          <p class="card-sub" data-i18n="chartTrendSub">10-second rolling request volume vs average latency</p>
        </div>
        <span id="statusMixPill" class="pill pill-good">0% err</span>
      </div>
      <div id="throughputTrendChart" class="chart-box"></div>
    </div>
  </section>

  <section class="grid-3">
    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title" data-i18n="cacheCardTitle">Storage &amp; Cache Efficiency</h2>
          <p class="card-sub" data-i18n="cacheCardSub">L1 Moka caches, spatial tile dedup &amp; synthesis cache</p>
        </div>
      </div>
      <div id="cacheGaugeChart" class="chart-box compact"></div>
      <div id="cacheMeters" class="meter-list"></div>
    </div>

    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title" data-i18n="sysCardTitle">Memory, Disk &amp; Egress Posture</h2>
          <p class="card-sub" data-i18n="sysCardSub">Linux RSS memory, WAL footprint &amp; outbound traffic mode</p>
        </div>
      </div>
      <div id="sysMeters" class="meter-list"></div>
    </div>

    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title" data-i18n="shieldCardTitle">Defense Shield &amp; Job Queue</h2>
          <p class="card-sub" data-i18n="shieldCardSub">Concurrency semaphores, rate limits &amp; build pipeline</p>
        </div>
      </div>
      <div id="shieldMeters" class="meter-list"></div>
    </div>
  </section>

  <section class="card" style="margin-bottom:16px">
    <div class="card-hdr">
      <div>
        <h2 class="card-title" data-i18n="connTitle">Upstream Data Connector Health</h2>
        <p class="card-sub" data-i18n="connSub">Configured status only — credentials, tokens &amp; proxy endpoints are never exposed</p>
      </div>
      <div class="conn-filters" id="connFilters">
        <button type="button" data-filter="all" class="on" data-i18n="filterAll">All</button>
        <button type="button" data-filter="ready" data-i18n="filterReady">Ready</button>
        <button type="button" data-filter="standby" data-i18n="filterStandby">Standby</button>
      </div>
    </div>
    <div id="connectorGrid" class="conn-grid">{ssr_connectors}</div>
  </section>

  <section class="grid-2">
    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title" data-i18n="routeTblTitle">Per-Route Latency P50 / P95 Breakdown</h2>
          <p class="card-sub" data-i18n="routeTblSub">Traffic volume, HTTP 4xx/5xx errors &amp; percentile latencies</p>
        </div>
      </div>
      <div class="tbl-wrap">
        <table class="ops-tbl">
          <thead>
            <tr>
              <th data-i18n="thRoute">Route Bucket</th>
              <th data-i18n="thReq">Req</th>
              <th data-i18n="thErr">Err</th>
              <th data-i18n="thMean">Mean</th>
              <th data-i18n="thP50">P50</th>
              <th data-i18n="thP95">P95</th>
              <th data-i18n="thMax">Max</th>
            </tr>
          </thead>
          <tbody id="routeTableBody">{ssr_routes}</tbody>
        </table>
      </div>
    </div>

    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title" data-i18n="eventsTitle">Recent Request Events (Last 30, Sanitized)</h2>
          <p class="card-sub" data-i18n="eventsSub">Query strings, cookies, tokens &amp; IPs are stripped at ingestion</p>
        </div>
      </div>
      <div class="tbl-wrap">
        <table class="ops-tbl">
          <thead>
            <tr>
              <th data-i18n="thTime">Time</th>
              <th data-i18n="thMethod">Method</th>
              <th data-i18n="thPath">Sanitized Path</th>
              <th data-i18n="thStatus">Status</th>
              <th data-i18n="thDur">Latency</th>
            </tr>
          </thead>
          <tbody id="eventsTableBody"></tbody>
        </table>
      </div>
    </div>
  </section>

  <section class="card">
    <div class="card-hdr">
      <div>
        <h2 class="card-title" data-i18n="recentReportsTitle">Recent Cached Property Reports &amp; Platform Inventory</h2>
        <p class="card-sub" data-i18n="recentReportsSub">Jump directly into any cached property report or its Buyer Command Center</p>
      </div>
    </div>
    <div class="tbl-wrap">
      <table class="ops-tbl" id="recentReportsTable">
        <thead>
          <tr>
            <th data-i18n="thProperty">Property Address</th>
            <th data-i18n="thScore">Score</th>
            <th data-i18n="thVal">Est. Value</th>
            <th data-i18n="thAi">Synthesis</th>
            <th data-i18n="thUpdated">Updated</th>
            <th data-i18n="thActions">Quick Actions</th>
          </tr>
        </thead>
        <tbody id="recentReportsBody">{ssr_reports}</tbody>
      </table>
    </div>
    <div id="inventoryStrip" class="inv-strip"></div>
  </section>

  <footer class="foot-note">
    <span data-i18n="footerNote">{safe_brand} Platform Telemetry — Zero PII, zero credential exposure, live SQLite/MySQL observability.</span>
    <span class="mono">GET /api/ops/telemetry</span>
  </footer>
</main>

<script id="ops-initial-data" type="application/json">{initial_json}</script>
<script>
'use strict';
(function () {{
  const LANG_COOKIE = 'open_estate_lang';
  const BRAND_NAME = {brand_js};
  const I18N = {{
    en: {{
      opsTag: 'OPS TELEMETRY',
      backHome: '← Back to ' + BRAND_NAME,
      dealRoom: 'Deal Room',
      autoOn: 'Auto-Refresh (5s)',
      autoOff: 'Auto-Refresh: Paused',
      refreshNow: 'Refresh Now',
      refreshing: 'Refreshing…',
      stripUptime: 'Uptime',
      stripInFlight: 'In-Flight',
      stripDb: 'Storage',
      stripEgress: 'Egress',
      stripMem: 'RSS',
      lastUpdated: 'Updated',
      operational: 'OPERATIONAL',
      degraded: 'DEGRADED',
      kpiReq: 'HTTP Requests',
      kpiLatency: 'Peak Route P95',
      kpiReports: 'Cached Reports',
      kpiCache: 'AI & L1 Cache',
      kpiMemory: 'Process Memory & DB',
      kpiHomes: 'Indexed Homes & Alerts',
      chartLatTitle: 'Route Latency Distribution (P50 / P95 / Mean ms)',
      chartLatSub: 'Rolling 256-sample latency ring per route bucket',
      chartTrendTitle: 'Throughput & Latency Timeline',
      chartTrendSub: '10-second rolling request volume vs average latency',
      cacheCardTitle: 'Storage & Cache Efficiency',
      cacheCardSub: 'L1 Moka caches, spatial tile dedup & synthesis cache',
      sysCardTitle: 'Memory, Disk & Egress Posture',
      sysCardSub: 'Linux RSS memory, WAL footprint & outbound traffic mode',
      shieldCardTitle: 'Defense Shield & Job Queue',
      shieldCardSub: 'Concurrency semaphores, rate limits & build pipeline',
      connTitle: 'Upstream Data Connector Health',
      connSub: 'Configured status only — credentials, tokens & proxy endpoints are never exposed',
      filterAll: 'All',
      filterReady: 'Ready',
      filterStandby: 'Standby',
      routeTblTitle: 'Per-Route Latency P50 / P95 Breakdown',
      routeTblSub: 'Traffic volume, HTTP 4xx/5xx errors & percentile latencies',
      eventsTitle: 'Recent Request Events (Last 30, Sanitized)',
      eventsSub: 'Query strings, cookies, tokens & IPs are stripped at ingestion',
      recentReportsTitle: 'Recent Cached Property Reports & Platform Inventory',
      recentReportsSub: 'Jump directly into any cached property report or its Buyer Command Center',
      thRoute: 'Route Bucket', thReq: 'Req', thErr: 'Err', thMean: 'Mean', thP50: 'P50', thP95: 'P95', thMax: 'Max',
      thTime: 'Time', thMethod: 'Method', thPath: 'Sanitized Path', thStatus: 'Status', thDur: 'Latency',
      thProperty: 'Property Address', thScore: 'Score', thVal: 'Est. Value', thAi: 'Synthesis', thUpdated: 'Updated', thActions: 'Quick Actions',
      openReport: 'Open Report ↗',
      openCmd: '⚡ Buyer Command Center ↗',
      noEvents: 'No requests recorded yet — traffic will appear here automatically.',
      noReports: 'No cached reports yet. Search an address on ' + BRAND_NAME + ' to populate reports.',
      ready: 'Ready',
      standby: 'Standby',
      cleanRoom: 'Clean-Room',
      tileDedup: 'Tile Dedup Ratio',
      aiReady: 'AI Ready',
      aiCore: 'Core',
      mAiCache: 'AI Synthesis Cache (Active / Total)',
      mL1Cache: 'L1 Report + AI Memory Entries',
      mHttpCache: 'HTTP L1 & Disk Cache',
      mRss: 'Process RSS vs Peak HWM',
      mDb: 'Database',
      mDiskCache: 'HTTP Disk Cache Volume',
      mEgressSlots: 'Egress Slots Healthy',
      mUncachedPermits: 'Uncached Build Permits Active',
      mAiPermits: 'AI Concurrency Permits Active',
      mBuildQueue: 'Build Queue Completion',
      mShield: 'Shield Blocked / Rate-Limited',
      invForSale: 'For Sale',
      invPending: 'Pending',
      invSold: 'Sold Comps',
      invWatchedZips: 'Watched ZIPs',
      invDealRooms: 'Deal Rooms',
      invSubs: 'Subscriptions',
      invDocReviews: 'Doc Reviews',
      footerNote: BRAND_NAME + ' Platform Telemetry — Zero PII, zero credential exposure, live SQLite/MySQL observability.'
    }},
    zh: {{
      opsTag: '运维监控面板',
      backHome: '← 返回 ' + BRAND_NAME,
      dealRoom: '交易对比室',
      autoOn: '自动刷新 (5秒)',
      autoOff: '自动刷新: 已暂停',
      refreshNow: '立即刷新',
      refreshing: '刷新中…',
      stripUptime: '运行时间',
      stripInFlight: '并发请求',
      stripDb: '存储引擎',
      stripEgress: '出站通道',
      stripMem: '进程内存',
      lastUpdated: '更新时间',
      operational: '运行正常',
      degraded: '降级告警',
      kpiReq: 'HTTP 总请求数',
      kpiLatency: '路由峰值 P95',
      kpiReports: '已缓存报告',
      kpiCache: 'AI 与 L1 缓存',
      kpiMemory: '进程内存与数据库',
      kpiHomes: '房源索引与订阅',
      chartLatTitle: '路由延迟分布 (P50 / P95 / 均值 ms)',
      chartLatSub: '每个路由桶最近 256 次请求的滚动分位延迟',
      chartTrendTitle: '吞吐量与平均延迟趋势',
      chartTrendSub: '每 10 秒窗口请求量与平均响应时间',
      cacheCardTitle: '存储与缓存命中效率',
      cacheCardSub: 'L1 内存缓存、瓦片去重率与 AI 合成缓存',
      sysCardTitle: '内存、磁盘与出站状态',
      sysCardSub: 'Linux RSS 物理内存、WAL 文件大小与安全出站模式',
      shieldCardTitle: '安全防护墙与构建队列',
      shieldCardSub: '并发信号量、限流拦截统计与报告队列状态',
      connTitle: '上游数据源连接器状态',
      connSub: '仅展示配置状态 — 绝不暴露密钥、令牌或代理 IP',
      filterAll: '全部',
      filterReady: '就绪',
      filterStandby: '未启用',
      routeTblTitle: '各路由 P50 / P95 延迟明细',
      routeTblSub: '请求量、4xx/5xx 错误数与延迟分位表',
      eventsTitle: '最近 30 条请求日志 (已脱敏)',
      eventsSub: '已彻底剥离查询参数、Cookie、令牌与 IP 地址',
      recentReportsTitle: '最近缓存房产报告与平台库存',
      recentReportsSub: '一键打开任意缓存房产报告或买方指挥中心',
      thRoute: '路由桶', thReq: '请求', thErr: '错误', thMean: '均值', thP50: 'P50', thP95: 'P95', thMax: '最大',
      thTime: '时间', thMethod: '方法', thPath: '脱敏路径', thStatus: '状态', thDur: '耗时',
      thProperty: '房产地址', thScore: '综合评分', thVal: '模型估值', thAi: 'AI 研判', thUpdated: '更新时间', thActions: '快捷操作',
      openReport: '打开报告 ↗',
      openCmd: '⚡ 买方指挥中心 ↗',
      noEvents: '暂无请求记录 — 流量到达后将实时显示。',
      noReports: '暂无缓存报告。请先在首页查询一套美国房产地址。',
      ready: '就绪',
      standby: '待命',
      cleanRoom: '净室模式',
      tileDedup: '瓦片去重率',
      aiReady: 'AI 已生成',
      aiCore: '基础报告',
      mAiCache: 'AI 合成缓存 (有效 / 总量)',
      mL1Cache: 'L1 报告与 AI 内存条目',
      mHttpCache: 'HTTP L1 与磁盘缓存',
      mRss: '进程物理内存 vs 峰值水位',
      mDb: '数据库引擎',
      mDiskCache: 'HTTP 磁盘缓存占用',
      mEgressSlots: '健康出站槽位',
      mUncachedPermits: '构建并发许可 (活跃 / 上限)',
      mAiPermits: 'AI 并发许可 (活跃 / 上限)',
      mBuildQueue: '报告队列完成率',
      mShield: '安全墙拦截与限流计数',
      invForSale: '在售房源',
      invPending: '交易中',
      invSold: '成交参考',
      invWatchedZips: '追踪邮编',
      invDealRooms: '交易对比室',
      invSubs: '活跃订阅',
      invDocReviews: '文档审阅',
      footerNote: BRAND_NAME + ' 平台运维遥测 — 零个人隐私、零密钥泄露、实时 SQLite/MySQL 监控。'
    }},
    es: {{
      opsTag: 'TELEMETRÍA OPS',
      backHome: '← Volver a ' + BRAND_NAME,
      dealRoom: 'Sala de Comparación',
      autoOn: 'Auto-Refresco (5s)',
      autoOff: 'Auto-Refresco: Pausado',
      refreshNow: 'Actualizar',
      refreshing: 'Actualizando…',
      stripUptime: 'Activo',
      stripInFlight: 'En curso',
      stripDb: 'Motor DB',
      stripEgress: 'Salida',
      stripMem: 'RSS',
      lastUpdated: 'Actualizado',
      operational: 'OPERATIVO',
      degraded: 'DEGRADADO',
      kpiReq: 'Solicitudes HTTP',
      kpiLatency: 'P95 Máximo de Ruta',
      kpiReports: 'Informes en Caché',
      kpiCache: 'Caché IA y L1',
      kpiMemory: 'Memoria y Base DB',
      kpiHomes: 'Viviendas y Alertas',
      chartLatTitle: 'Latencia por Ruta (P50 / P95 / Media ms)',
      chartLatSub: 'Ventana móvil de últimas 256 muestras por ruta',
      chartTrendTitle: 'Rendimiento y Latencia en el Tiempo',
      chartTrendSub: 'Solicitudes por ventana de 10s frente a latencia media',
      cacheCardTitle: 'Eficiencia de Almacenamiento y Caché',
      cacheCardSub: 'Cachés L1 Moka, deduplicación espacial y síntesis',
      sysCardTitle: 'Memoria, Disco y Tráfico Saliente',
      sysCardSub: 'Memoria RSS Linux, tamaño WAL y modo de salida',
      shieldCardTitle: 'Escudo de Defensa y Cola de Trabajos',
      shieldCardSub: 'Semáforos de concurrencia, bloqueos y cola de informes',
      connTitle: 'Estado de Conectores Externos',
      connSub: 'Solo estado de configuración — nunca expone claves ni IPs proxy',
      filterAll: 'Todos',
      filterReady: 'Activos',
      filterStandby: 'En espera',
      routeTblTitle: 'Desglose P50 / P95 por Ruta',
      routeTblSub: 'Volumen, errores 4xx/5xx y percentiles de latencia',
      eventsTitle: 'Últimas 30 Solicitudes (Saneadas)',
      eventsSub: 'Parámetros, cookies, tokens e IPs eliminados en origen',
      recentReportsTitle: 'Informes Recientes e Inventario de Plataforma',
      recentReportsSub: 'Abre cualquier informe o su Centro de Mando del Comprador en 1 clic',
      thRoute: 'Ruta', thReq: 'Sol.', thErr: 'Err', thMean: 'Media', thP50: 'P50', thP95: 'P95', thMax: 'Máx',
      thTime: 'Hora', thMethod: 'Método', thPath: 'Ruta Saneada', thStatus: 'Estado', thDur: 'Latencia',
      thProperty: 'Dirección', thScore: 'Nota', thVal: 'Valor Est.', thAi: 'Síntesis', thUpdated: 'Actualizado', thActions: 'Acciones',
      openReport: 'Abrir Informe ↗',
      openCmd: '⚡ Centro de Mando ↗',
      noEvents: 'Sin solicitudes registradas aún — el tráfico aparecerá aquí.',
      noReports: 'Aún no hay informes en caché. Busca una dirección en ' + BRAND_NAME + '.',
      ready: 'Activo',
      standby: 'En espera',
      cleanRoom: 'Sala Limpia',
      tileDedup: 'Ratio Dedup. Tiles',
      aiReady: 'IA Lista',
      aiCore: 'Base',
      mAiCache: 'Caché Síntesis IA (Activa / Total)',
      mL1Cache: 'Entradas L1 Informe + IA',
      mHttpCache: 'Caché HTTP L1 y Disco',
      mRss: 'RSS del Proceso vs Pico HWM',
      mDb: 'Base de Datos',
      mDiskCache: 'Volumen de Caché en Disco HTTP',
      mEgressSlots: 'Ranuras de Salida Saludables',
      mUncachedPermits: 'Permisos de Construcción Activos',
      mAiPermits: 'Permisos de Concurrencia IA',
      mBuildQueue: 'Finalización de Cola de Informes',
      mShield: 'Escudo Bloqueado / Límite Tasa',
      invForSale: 'En Venta',
      invPending: 'Pendientes',
      invSold: 'Vendidas',
      invWatchedZips: 'ZIPs Vigilados',
      invDealRooms: 'Salas Deal',
      invSubs: 'Suscripciones',
      invDocReviews: 'Reseñas Doc.',
      footerNote: 'Telemetría ' + BRAND_NAME + ' — Cero PII, cero credenciales expuestas, observabilidad en vivo.'
    }}
  }};

  function detectLang() {{
    try {{
      const qp = new URLSearchParams(location.search).get('lang');
      if (qp) {{
        const low = qp.toLowerCase();
        if (low.startsWith('zh')) return 'zh';
        if (low.startsWith('es')) return 'es';
        return 'en';
      }}
      const ck = document.cookie.split(';').map(s => s.trim()).find(s => s.startsWith(LANG_COOKIE + '='));
      if (ck) {{
        const low = decodeURIComponent(ck.split('=')[1] || '').toLowerCase();
        if (low.startsWith('zh')) return 'zh';
        if (low.startsWith('es')) return 'es';
        return 'en';
      }}
    }} catch (_) {{}}
    return 'en';
  }}

  let currentLang = detectLang();
  let autoRefresh = true;
  let connFilter = 'all';
  let latestData = {{}};
  try {{
    latestData = JSON.parse(document.getElementById('ops-initial-data').textContent || '{{}}');
  }} catch (_) {{}}

  const charts = {{ lat: null, trend: null, gauge: null }};

  function tr(key) {{
    const dict = I18N[currentLang] || I18N.en;
    return dict[key] || I18N.en[key] || key;
  }}

  function esc(str) {{
    return String(str == null ? '' : str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }}

  function fmtNum(n) {{
    const v = Number(n || 0);
    return Number.isFinite(v) ? v.toLocaleString() : '0';
  }}

  function fmtUptime(secs) {{
    const s = Math.max(0, Math.floor(Number(secs || 0)));
    const d = Math.floor(s / 86400);
    const h = Math.floor((s % 86400) / 3600);
    const m = Math.floor((s % 3600) / 60);
    const rem = s % 60;
    const pad = (x) => String(x).padStart(2, '0');
    if (d > 0) return `${{d}}d ${{pad(h)}}h ${{pad(m)}}m`;
    if (h > 0) return `${{h}}h ${{pad(m)}}m ${{pad(rem)}}s`;
    return `${{m}}m ${{pad(rem)}}s`;
  }}

  function fmtUsd(n) {{
    const v = Number(n);
    if (!Number.isFinite(v) || v <= 0) return '—';
    return '$' + Math.round(v).toLocaleString();
  }}

  function fmtTimeShort(iso) {{
    if (!iso) return '--';
    const t = String(iso);
    const idx = t.indexOf('T');
    if (idx !== -1 && t.length >= idx + 9) return t.slice(idx + 1, idx + 9);
    return t.slice(0, 19);
  }}

  function applyLang(lang) {{
    currentLang = (lang === 'zh' || lang === 'es') ? lang : 'en';
    document.documentElement.lang = currentLang === 'zh' ? 'zh-CN' : currentLang;
    try {{
      document.cookie = `${{LANG_COOKIE}}=${{currentLang}}; path=/; max-age=31536000; SameSite=Lax`;
    }} catch (_) {{}}
    document.querySelectorAll('#langSwitch button').forEach((btn) => {{
      btn.classList.toggle('on', btn.getAttribute('data-lang') === currentLang);
    }});
    document.querySelectorAll('[data-i18n]').forEach((el) => {{
      const k = el.getAttribute('data-i18n');
      if (k) el.textContent = tr(k);
    }});
    document.getElementById('autoRefreshLabel').textContent = autoRefresh ? tr('autoOn') : tr('autoOff');
    renderAll(latestData);
  }}

  function renderMeters(elId, rows) {{
    const el = document.getElementById(elId);
    if (!el) return;
    el.innerHTML = rows.map((r) => `
      <div class="meter-row">
        <div class="meter-top"><span>${{esc(r.label)}}</span><span>${{esc(r.valueText)}}</span></div>
        <div class="meter-track"><div class="meter-fill ${{r.tone || ''}}" style="width:${{Math.max(0, Math.min(100, r.pct)).toFixed(1)}}%"></div></div>
      </div>
    `).join('');
  }}

  function renderFallbackBars(container, items) {{
    container.innerHTML = `<div style="display:flex;flex-direction:column;gap:8px;padding:6px 0">` +
      items.map(it => `
        <div style="display:grid;grid-template-columns:130px 1fr 72px;gap:8px;align-items:center;font-size:12px">
          <span class="mono" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${{esc(it.label)}}</span>
          <div style="height:10px;background:#e2e8f0;border-radius:999px;overflow:hidden">
            <div style="height:100%;width:${{Math.min(100, it.pct).toFixed(1)}}%;background:${{it.color || '#0f766e'}};border-radius:999px"></div>
          </div>
          <span class="mono" style="text-align:right;font-weight:700">${{esc(it.valText)}}</span>
        </div>
      `).join('') + `</div>`;
  }}

  function getOrCreateChart(key, elId) {{
    const el = document.getElementById(elId);
    if (!el || !window.echarts || typeof window.echarts.init !== 'function') return null;
    if (!charts[key]) {{
      charts[key] = window.echarts.init(el, null, {{ renderer: 'canvas' }});
    }}
    return charts[key];
  }}

  function renderCharts(d) {{
    const routes = Array.isArray(d.routes) ? d.routes : [];
    const primaryRoutes = routes.filter(r => r.route !== 'other' || r.requests > 0);
    const latEl = document.getElementById('latencyRouteChart');
    const maxP95 = Math.max(10, ...primaryRoutes.map(r => Number(r.p95Ms || 0)));

    const latChart = getOrCreateChart('lat', 'latencyRouteChart');
    if (latChart) {{
      latChart.setOption({{
        tooltip: {{ trigger: 'axis', axisPointer: {{ type: 'shadow' }} }},
        legend: {{ data: ['P50', 'P95', 'Mean'], top: 0, textStyle: {{ fontSize: 11, color: '#475569' }} }},
        grid: {{ left: 10, right: 16, top: 34, bottom: 8, containLabel: true }},
        xAxis: {{ type: 'value', axisLabel: {{ fontSize: 11, color: '#64748b', formatter: '{{value}} ms' }}, splitLine: {{ lineStyle: {{ color: '#f1f5f9' }} }} }},
        yAxis: {{ type: 'category', data: primaryRoutes.map(r => r.route), axisLabel: {{ fontSize: 11, color: '#334155', fontFamily: 'JetBrains Mono, monospace' }} }},
        series: [
          {{ name: 'P50', type: 'bar', data: primaryRoutes.map(r => r.p50Ms || 0), itemStyle: {{ color: '#14b8a6', borderRadius: [0, 3, 3, 0] }} }},
          {{ name: 'P95', type: 'bar', data: primaryRoutes.map(r => r.p95Ms || 0), itemStyle: {{ color: '#0f172a', borderRadius: [0, 3, 3, 0] }} }},
          {{ name: 'Mean', type: 'bar', data: primaryRoutes.map(r => r.meanMs || 0), itemStyle: {{ color: '#3b82f6', borderRadius: [0, 3, 3, 0] }} }}
        ]
      }}, true);
    }} else if (latEl) {{
      renderFallbackBars(latEl, primaryRoutes.map(r => ({{
        label: r.route,
        pct: (Number(r.p95Ms || 0) / maxP95) * 100,
        valText: `${{Number(r.p95Ms || 0).toFixed(1)}} ms`,
        color: '#0f766e'
      }})));
    }}

    const ts = Array.isArray(d.timeSeries) ? d.timeSeries : [];
    const trendEl = document.getElementById('throughputTrendChart');
    const trendChart = getOrCreateChart('trend', 'throughputTrendChart');
    if (trendChart) {{
      const labels = ts.length ? ts.map(p => p.label) : ['--:--:--'];
      const reqs = ts.length ? ts.map(p => p.requests) : [0];
      const avgs = ts.length ? ts.map(p => p.avgMs) : [0];
      trendChart.setOption({{
        tooltip: {{ trigger: 'axis' }},
        legend: {{ data: [tr('thReq'), tr('thMean')], top: 0, textStyle: {{ fontSize: 11, color: '#475569' }} }},
        grid: {{ left: 10, right: 12, top: 34, bottom: 8, containLabel: true }},
        xAxis: {{ type: 'category', data: labels, axisLabel: {{ fontSize: 11, color: '#64748b' }} }},
        yAxis: [
          {{ type: 'value', name: tr('thReq'), minInterval: 1, axisLabel: {{ fontSize: 11, color: '#64748b' }}, splitLine: {{ lineStyle: {{ color: '#f1f5f9' }} }} }},
          {{ type: 'value', name: 'ms', axisLabel: {{ fontSize: 11, color: '#64748b' }}, splitLine: {{ show: false }} }}
        ],
        series: [
          {{ name: tr('thReq'), type: 'bar', barMaxWidth: 22, data: reqs, itemStyle: {{ color: '#0f766e', borderRadius: [4, 4, 0, 0] }} }},
          {{ name: tr('thMean'), type: 'line', yAxisIndex: 1, smooth: true, data: avgs, lineStyle: {{ color: '#2563eb', width: 2.5 }}, itemStyle: {{ color: '#2563eb' }}, areaStyle: {{ color: 'rgba(37,99,235,.12)' }} }}
        ]
      }}, true);
    }} else if (trendEl) {{
      const maxReq = Math.max(1, ...ts.map(p => p.requests || 0));
      renderFallbackBars(trendEl, (ts.length ? ts : [{{ label: '--', requests: 0, avgMs: 0 }}]).slice(-8).map(p => ({{
        label: p.label,
        pct: (Number(p.requests || 0) / maxReq) * 100,
        valText: `${{p.requests}} req`,
        color: '#2563eb'
      }})));
    }}

    const cache = d.cacheEfficiency || {{}};
    const dedupPct = Number(cache.dedupPct || 0);
    const gaugeEl = document.getElementById('cacheGaugeChart');
    const gaugeChart = getOrCreateChart('gauge', 'cacheGaugeChart');
    if (gaugeChart) {{
      gaugeChart.setOption({{
        series: [{{
          type: 'gauge',
          startAngle: 205,
          endAngle: -25,
          min: 0,
          max: 100,
          radius: '92%',
          center: ['50%', '58%'],
          progress: {{ show: true, width: 12, itemStyle: {{ color: '#0f766e' }} }},
          axisLine: {{ lineStyle: {{ width: 12, color: [[1, '#e2e8f0']] }} }},
          axisTick: {{ show: false }},
          splitLine: {{ show: false }},
          axisLabel: {{ show: false }},
          pointer: {{ show: false }},
          detail: {{ valueAnimation: true, fontSize: 22, fontWeight: 800, fontFamily: 'JetBrains Mono, monospace', color: '#0f172a', formatter: '{{value}}%', offsetCenter: [0, '8%'] }},
          title: {{ fontSize: 11, color: '#64748b', offsetCenter: [0, '38%'] }},
          data: [{{ value: dedupPct, name: tr('tileDedup') }}]
        }}]
      }}, true);
    }} else if (gaugeEl) {{
      renderFallbackBars(gaugeEl, [{{ label: tr('tileDedup'), pct: dedupPct, valText: `${{dedupPct.toFixed(1)}}%`, color: '#0f766e' }}]);
    }}
  }}

  function renderAll(d) {{
    if (!d || typeof d !== 'object') return;
    const srv = d.server || {{}};
    const sc = srv.statusCounts || {{}};
    const db = d.database || {{}};
    const inv = d.inventory || {{}};
    const jobs = inv.reportJobs || {{}};
    const cache = d.cacheEfficiency || {{}};
    const sys = d.system || {{}};
    const sec = d.securityAndEgress || {{}};
    const shield = sec.shield || {{}};
    const sems = shield.semaphores || {{}};
    const inf = shield.inflight || {{}};
    const routes = Array.isArray(d.routes) ? d.routes : [];

    const dbOk = !!db.connected;
    const errRate = Number(srv.errorRatePct || 0);
    const badge = document.getElementById('healthBadge');
    badge.className = 'status-led ' + (dbOk && errRate < 15 ? 'ok' : 'warn');
    document.getElementById('healthBadgeText').textContent = dbOk ? tr('operational') : tr('degraded');

    document.getElementById('stripUptime').textContent = fmtUptime(srv.uptimeSecs);
    document.getElementById('stripInFlight').textContent = fmtNum(srv.inFlightRequests);
    document.getElementById('stripDb').textContent = db.engine || db.dialect || 'SQLite';
    document.getElementById('stripEgress').textContent = sec.egressMode || 'direct';
    document.getElementById('stripMem').textContent = sys.rssMb != null ? `${{Number(sys.rssMb).toFixed(1)}} MB` : 'N/A';
    document.getElementById('lastUpdatedAt').textContent = fmtTimeShort(d.generatedAt);

    // KPI Cards
    document.getElementById('kpiTotalReq').textContent = fmtNum(srv.totalRequests);
    const c2 = Number(sc['2xx'] || 0);
    const cErr = Number(sc['4xx'] || 0) + Number(sc['5xx'] || 0);
    document.getElementById('kpiReqSub').innerHTML =
      `<span class="pill pill-good">2xx: ${{fmtNum(c2)}}</span>` +
      `<span class="pill ${{cErr > 0 ? 'pill-warn' : 'pill-muted'}}">4xx/5xx: ${{fmtNum(cErr)}}</span>`;

    const activeRoutes = routes.filter(r => r.requests > 0);
    const peakP95 = activeRoutes.length ? Math.max(...activeRoutes.map(r => Number(r.p95Ms || 0))) : 0;
    const avgMean = activeRoutes.length ? activeRoutes.reduce((a, r) => a + Number(r.meanMs || 0), 0) / activeRoutes.length : 0;
    document.getElementById('kpiP95').textContent = `${{peakP95.toFixed(1)}} ms`;
    document.getElementById('kpiLatencySub').innerHTML =
      `<span class="pill pill-info">Mean: ${{avgMean.toFixed(1)}} ms</span>` +
      `<span class="pill pill-muted">${{tr('stripInFlight')}}: ${{fmtNum(srv.inFlightRequests)}}</span>`;

    document.getElementById('kpiReports').textContent = fmtNum(inv.reportsTotal);
    document.getElementById('kpiQueueSub').innerHTML =
      `<span class="pill pill-good">Done: ${{fmtNum(jobs.done)}}</span>` +
      `<span class="pill ${{jobs.running > 0 ? 'pill-info' : 'pill-muted'}}">Run: ${{fmtNum(jobs.running)}}</span>` +
      `<span class="pill ${{jobs.error > 0 ? 'pill-bad' : 'pill-muted'}}">Err: ${{fmtNum(jobs.error)}}</span>`;

    document.getElementById('kpiLlmCache').textContent = fmtNum(inv.llmCacheActiveEntries != null ? inv.llmCacheActiveEntries : inv.llmCacheEntries);
    document.getElementById('kpiCacheSub').innerHTML =
      `<span class="pill pill-good">L1: ${{fmtNum(Number(cache.l1ReportsEntries || 0) + Number(cache.l1LlmEntries || 0))}}</span>` +
      `<span class="pill pill-info">Dedup: ${{Number(cache.dedupPct || 0).toFixed(0)}}%</span>`;

    document.getElementById('kpiRss').textContent = sys.rssMb != null ? `${{Number(sys.rssMb).toFixed(1)}} MB` : 'N/A';
    document.getElementById('kpiDbSub').innerHTML =
      `<span class="pill pill-muted">DB: ${{Number(db.totalMb || 0).toFixed(2)}} MB</span>` +
      `<span class="pill pill-muted">${{esc(db.dialect || 'sqlite')}}</span>`;

    document.getElementById('kpiHomes').textContent = fmtNum(inv.indexedHomesTotal);
    document.getElementById('kpiAlertsSub').innerHTML =
      `<span class="pill pill-info">Alerts: ${{fmtNum(inv.alertSubscriptionsActive)}}</span>` +
      `<span class="pill pill-muted">Deals: ${{fmtNum(inv.dealRoomsCount)}}</span>`;

    const statusMixPill = document.getElementById('statusMixPill');
    statusMixPill.textContent = `${{errRate.toFixed(1)}}% err`;
    statusMixPill.className = 'pill ' + (errRate > 5 ? 'pill-warn' : 'pill-good');

    // Meters
    const llmTotal = Math.max(1, Number(inv.llmCacheEntries || 0));
    const llmActive = Number(inv.llmCacheActiveEntries || 0);
    renderMeters('cacheMeters', [
      {{ label: tr('mAiCache'), valueText: `${{fmtNum(llmActive)}} / ${{fmtNum(inv.llmCacheEntries)}}`, pct: (llmActive / llmTotal) * 100 }},
      {{ label: tr('mL1Cache'), valueText: `${{fmtNum(cache.l1ReportsEntries)}} / ${{fmtNum(cache.l1LlmEntries)}}`, pct: Math.min(100, (Number(cache.l1ReportsEntries || 0) + Number(cache.l1LlmEntries || 0)) / 2) }},
      {{ label: tr('mHttpCache'), valueText: `${{fmtNum(cache.httpL1Entries)}} mem · ${{fmtNum(cache.httpDiskCacheFiles)}} files`, pct: Math.min(100, Number(cache.httpL1Entries || 0) / 10), tone: 'blue' }}
    ]);

    const rssMb = Number(sys.rssMb || 0);
    const peakMb = Math.max(rssMb, Number(sys.peakRssMb || 0), 1);
    const dbTotalMb = Number(db.totalMb || 0);
    const httpDiskMb = Number(cache.httpDiskCacheMb || 0);
    renderMeters('sysMeters', [
      {{ label: tr('mRss'), valueText: sys.rssMb != null ? `${{rssMb.toFixed(1)}} / ${{peakMb.toFixed(1)}} MB` : 'N/A', pct: sys.rssMb != null ? (rssMb / peakMb) * 100 : 0 }},
      {{ label: `${{tr('mDb')}} (${{db.fileName || db.dialect || 'sqlite'}})`, valueText: `${{dbTotalMb.toFixed(2)}} MB (WAL ${{((Number(db.walBytes || 0)) / (1024 * 1024)).toFixed(2)}} MB)`, pct: Math.min(100, Math.max(4, dbTotalMb * 4)), tone: 'blue' }},
      {{ label: tr('mDiskCache'), valueText: `${{httpDiskMb.toFixed(2)}} MB (${{fmtNum(cache.httpDiskCacheFiles)}} files)`, pct: Math.min(100, httpDiskMb * 2), tone: 'amber' }},
      {{ label: tr('mEgressSlots'), valueText: `${{fmtNum(sec.healthySlots)}} / ${{fmtNum(sec.totalSlots)}} · ${{sec.egressMode || 'direct'}}`, pct: sec.totalSlots > 0 ? (Number(sec.healthySlots || 0) / Number(sec.totalSlots)) * 100 : 100 }}
    ]);

    const uncachedMax = Math.max(1, Number(sems.uncached_builds_max || 64));
    const uncachedAct = Number(inf.uncached_builds_active || 0);
    const aiMax = Math.max(1, Number(sems.ai_max || 24));
    const aiAct = Number(inf.ai_calls_active || 0);
    const qTotal = Math.max(1, Number(jobs.total || 0));
    renderMeters('shieldMeters', [
      {{ label: tr('mUncachedPermits'), valueText: `${{uncachedAct}} / ${{uncachedMax}}`, pct: (uncachedAct / uncachedMax) * 100 }},
      {{ label: tr('mAiPermits'), valueText: `${{aiAct}} / ${{aiMax}}`, pct: (aiAct / aiMax) * 100, tone: 'blue' }},
      {{ label: tr('mBuildQueue'), valueText: `${{fmtNum(jobs.done)}} done · ${{fmtNum(jobs.queued)}} queued · ${{fmtNum(jobs.error)}} err`, pct: jobs.total > 0 ? (Number(jobs.done || 0) / qTotal) * 100 : 100 }},
      {{ label: tr('mShield'), valueText: `${{fmtNum(shield.rate_limited_total)}} RL · ${{fmtNum(shield.malformed_blocked_total)}} mal · ${{fmtNum(shield.spoofed_xff_ignored)}} XFF`, pct: Math.min(100, Number(shield.rate_limited_total || 0) + Number(shield.malformed_blocked_total || 0)), tone: 'amber' }}
    ]);

    // Connectors
    const conns = Array.isArray(d.connectors) ? d.connectors : [];
    const filtered = conns.filter((c) => {{
      if (connFilter === 'ready') return !!c.configured;
      if (connFilter === 'standby') return !c.configured;
      return true;
    }});
    document.getElementById('connectorGrid').innerHTML = filtered.map((c) => {{
      const isReady = !!c.configured;
      const badgeCls = isReady ? 'pill-good' : (c.status === 'clean_room_restricted' ? 'pill-warn' : 'pill-muted');
      const badgeTxt = isReady ? tr('ready') : (c.status === 'clean_room_restricted' ? tr('cleanRoom') : tr('standby'));
      return `
        <div class="conn-card ${{isReady ? 'ready' : ''}}">
          <div class="conn-top">
            <div class="conn-name">${{esc(c.name)}}</div>
            <span class="pill ${{badgeCls}}">${{esc(badgeTxt)}}</span>
          </div>
          <div class="conn-meta">
            <span>${{esc(c.category)}}</span>
            <span class="mono">${{esc(c.authMode)}}</span>
          </div>
        </div>
      `;
    }}).join('');

    // Route table
    const maxRouteP95 = Math.max(1, ...routes.map(r => Number(r.p95Ms || 0)));
    document.getElementById('routeTableBody').innerHTML = routes.map((r) => {{
      const barPct = Math.min(100, (Number(r.p95Ms || 0) / maxRouteP95) * 100);
      return `
        <tr>
          <td class="mono" style="font-weight:700;color:#0f172a;white-space:nowrap">${{esc(r.route)}}</td>
          <td class="mono">${{fmtNum(r.requests)}}</td>
          <td class="mono">${{r.errors > 0 ? `<span class="pill pill-warn">${{fmtNum(r.errors)}}</span>` : '0'}}</td>
          <td class="mono" style="white-space:nowrap">${{Number(r.meanMs || 0).toFixed(2)}} ms</td>
          <td class="mono" style="white-space:nowrap">${{Number(r.p50Ms || 0).toFixed(2)}} ms</td>
          <td class="mono" style="white-space:nowrap">${{Number(r.p95Ms || 0).toFixed(2)}} ms <span class="lat-bar-bg"><span class="lat-bar-fg" style="width:${{barPct.toFixed(1)}}%"></span></span></td>
          <td class="mono" style="white-space:nowrap">${{Number(r.maxMs || 0).toFixed(2)}} ms</td>
        </tr>
      `;
    }}).join('');

    // Recent Events
    const events = Array.isArray(d.recentEvents) ? d.recentEvents : [];
    const evBody = document.getElementById('eventsTableBody');
    if (!events.length) {{
      evBody.innerHTML = `<tr><td colspan="5" style="text-align:center;color:#64748b;padding:18px">${{esc(tr('noEvents'))}}</td></tr>`;
    }} else {{
      evBody.innerHTML = events.map((ev) => {{
        const st = Number(ev.status || 200);
        const stCls = st >= 500 ? 'pill-bad' : (st >= 400 ? 'pill-warn' : 'pill-good');
        return `
          <tr>
            <td class="mono" style="white-space:nowrap">${{esc(fmtTimeShort(ev.timestamp))}}</td>
            <td><span class="pill pill-info">${{esc(ev.method)}}</span></td>
            <td class="mono" style="max-width:240px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${{esc(ev.path)}}">${{esc(ev.path)}}</td>
            <td><span class="pill ${{stCls}}">${{st}}</span></td>
            <td class="mono" style="white-space:nowrap">${{Number(ev.durationMs || 0).toFixed(2)}} ms</td>
          </tr>
        `;
      }}).join('');
    }}

    // Recent Reports
    const recs = Array.isArray(inv.recentReports) ? inv.recentReports : [];
    const recBody = document.getElementById('recentReportsBody');
    if (!recs.length) {{
      recBody.innerHTML = `<tr><td colspan="6" style="text-align:center;color:#64748b;padding:18px">${{esc(tr('noReports'))}}</td></tr>`;
    }} else {{
      recBody.innerHTML = recs.map((r) => {{
        const score = r.overallScore != null ? `${{Number(r.overallScore).toFixed(0)}}/100` : '—';
        const scoreCls = r.overallScore >= 75 ? 'pill-good' : (r.overallScore >= 55 ? 'pill-info' : 'pill-muted');
        return `
          <tr>
            <td>
              <div style="font-weight:700;color:#0f172a">${{esc(r.displayAddress)}}</div>
              <div class="mono" style="font-size:11px;color:#64748b">${{esc(r.slug)}}${{r.zip ? ' · ' + esc(r.zip) : ''}}</div>
            </td>
            <td><span class="pill ${{scoreCls}}">${{esc(score)}}</span></td>
            <td class="mono" style="white-space:nowrap">${{esc(fmtUsd(r.estimatedValue))}}</td>
            <td><span class="pill ${{r.aiReady ? 'pill-good' : 'pill-muted'}}">${{esc(r.aiReady ? tr('aiReady') : tr('aiCore'))}}</span></td>
            <td class="mono" style="white-space:nowrap">${{esc(fmtTimeShort(r.updatedAt))}}</td>
            <td>
              <div class="act-links">
                <a class="act-btn" href="${{esc(r.reportUrl)}}">${{esc(tr('openReport'))}}</a>
                <a class="act-btn cmd" href="${{esc(r.commandCenterUrl)}}">${{esc(tr('openCmd'))}}</a>
              </div>
            </td>
          </tr>
        `;
      }}).join('');
    }}

    // Platform Inventory Strip
    const byStatus = inv.indexedHomesByStatus || {{}};
    const osrm = d.osrm || {{}};
    const invItems = [
      [tr('invForSale'), fmtNum(byStatus.forSale)],
      [tr('invPending'), fmtNum(byStatus.pending)],
      [tr('invSold'), fmtNum(byStatus.sold)],
      [tr('invWatchedZips'), fmtNum(inv.watchedZipsActive)],
      [tr('invDealRooms'), fmtNum(inv.dealRoomsCount)],
      [tr('invSubs'), fmtNum(inv.alertSubscriptionsActive)],
      [tr('invDocReviews'), fmtNum(inv.docReviewsCached)],
      ['OSRM Regions', osrm.enabled ? `${{fmtNum(osrm.healthyRegions)}}/${{fmtNum(osrm.totalRegions)}}` : 'Unset'],
      ['OSRM Degraded', fmtNum(inv.osrmDegradedReports)]
    ];
    document.getElementById('inventoryStrip').innerHTML = invItems.map(([k, v]) => `
      <div class="inv-mini"><div class="k">${{esc(k)}}</div><div class="v">${{esc(v)}}</div></div>
    `).join('');

    renderCharts(d);
  }}

  async function refreshNow(isManual) {{
    const btn = document.getElementById('refreshNowBtn');
    if (isManual && btn) {{
      btn.disabled = true;
      btn.textContent = tr('refreshing');
    }}
    try {{
      const resp = await fetch('/api/ops/telemetry?poll=1', {{ cache: 'no-store' }});
      if (resp.ok) {{
        latestData = await resp.json();
        renderAll(latestData);
      }}
    }} catch (_) {{}}
    finally {{
      if (isManual && btn) {{
        btn.disabled = false;
        btn.textContent = tr('refreshNow');
      }}
    }}
  }}

  document.getElementById('refreshNowBtn').addEventListener('click', () => refreshNow(true));
  document.getElementById('autoRefreshBtn').addEventListener('click', () => {{
    autoRefresh = !autoRefresh;
    const btn = document.getElementById('autoRefreshBtn');
    btn.classList.toggle('active', autoRefresh);
    btn.setAttribute('aria-pressed', autoRefresh ? 'true' : 'false');
    document.getElementById('autoRefreshLabel').textContent = autoRefresh ? tr('autoOn') : tr('autoOff');
  }});

  document.querySelectorAll('#langSwitch button').forEach((btn) => {{
    btn.addEventListener('click', () => applyLang(btn.getAttribute('data-lang')));
  }});

  document.querySelectorAll('#connFilters button').forEach((btn) => {{
    btn.addEventListener('click', () => {{
      connFilter = btn.getAttribute('data-filter') || 'all';
      document.querySelectorAll('#connFilters button').forEach(b => b.classList.toggle('on', b === btn));
      renderAll(latestData);
    }});
  }});

  function resizeAllCharts() {{
    Object.values(charts).forEach((c) => {{
      if (c && typeof c.resize === 'function') c.resize();
    }});
  }}
  window.addEventListener('resize', resizeAllCharts);
  if (typeof ResizeObserver === 'function') {{
    const ro = new ResizeObserver(() => resizeAllCharts());
    ['latencyRouteChart', 'throughputTrendChart', 'cacheGaugeChart'].forEach((id) => {{
      const el = document.getElementById(id);
      if (el) ro.observe(el);
    }});
  }}

  applyLang(currentLang);
  setInterval(() => {{
    if (autoRefresh && !document.hidden) refreshNow(false);
  }}, 5000);
}})();
</script>
</body>
</html>"###
    )
}
