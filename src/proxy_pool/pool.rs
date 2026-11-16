//! Working-set management, request-time selection and the periodic maintenance loop.

use super::candidates::{xorshift, CandKey, CandidateStore};
use super::checker::{CheckStage, ProxyCheckResult, ProxyChecker};
use super::extract::{extract_proxies, ExtractedProxy, SourceSpec};
use super::registry::{
    conditional_get, fetch_registries, load_registry_cache, save_registry_cache, write_atomic,
    FetchOutcome, RegistryState, SourceOrigin, SourceState,
};
use super::{is_private_or_reserved_ip, ProxyEndpoint, ProxyPoolConfig, ProxySelectionStrategy};
use crate::shared::{now_unix_ms, now_unix_secs};
use anyhow::{anyhow, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify};

const WORKING_CACHE_FILE: &str = "free_proxies.json";
const HOST_TABLE_CAP: usize = 32;
const HOST_AFFINITY_SECS: i64 = 15 * 60;

thread_local! {
    static RNG: Cell<u64> = Cell::new({
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut h);
        (now_unix_ms() as u64) ^ h.finish() ^ 0x9E37_79B9_7F4A_7C15
    });
}

/// Uniform `(0, 1]` from a per-thread xorshift generator.
fn rand_unit() -> f64 {
    RNG.with(|c| {
        let mut s = c.get() | 1;
        let r = xorshift(&mut s);
        c.set(s);
        1.0 - ((r >> 11) as f64) / ((1u64 << 53) as f64)
    })
}

#[derive(Debug, Clone)]
struct HostStat {
    host: Box<str>,
    cooldown_until: i64,
    last_ok: i64,
    fails: u16,
    touched: i64,
}

/// A verified proxy with a ready-to-use `reqwest::Client` and live health statistics.
#[derive(Debug)]
pub struct WorkingProxy {
    pub endpoint: ProxyEndpoint,
    key: Arc<str>,
    cand_key: Option<CandKey>,
    pub client: reqwest::Client,
    pub exit_ip: Option<String>,
    pub exit_country: Option<String>,
    pub latency_ewma_ms: AtomicU64,
    pub successes: AtomicU64,
    pub failures: AtomicU64,
    pub consecutive_failures: AtomicU32,
    pub cooldown_until: AtomicI64,
    pub last_success_at: AtomicI64,
    pub last_checked_at: AtomicI64,
    pub last_used_at: AtomicI64,
    hosts: Mutex<Vec<HostStat>>,
}

impl WorkingProxy {
    pub fn build_egress_client(ep: &ProxyEndpoint, user_agent: &str) -> Result<reqwest::Client> {
        let proxy = reqwest::Proxy::all(ep.url())?;
        // Small idle pool per proxy: with up to `max_working_proxies` clients alive, idle sockets
        // stay bounded (≈2 per proxy/target) instead of competing with probes for descriptors.
        Ok(reqwest::Client::builder()
            .user_agent(user_agent)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_millis(3_500))
            .timeout(Duration::from_millis(20_000))
            .redirect(reqwest::redirect::Policy::limited(5))
            .no_proxy()
            .proxy(proxy)
            .build()?)
    }

    fn new(
        endpoint: ProxyEndpoint,
        latency_ms: u64,
        exit_ip: Option<String>,
        exit_country: Option<String>,
        user_agent: &str,
    ) -> Result<Self> {
        let client = Self::build_egress_client(&endpoint, user_agent)?;
        let now = now_unix_secs();
        Ok(Self {
            key: Arc::from(endpoint.key()),
            cand_key: CandKey::from_endpoint(&endpoint),
            endpoint,
            client,
            exit_ip,
            exit_country,
            latency_ewma_ms: AtomicU64::new(latency_ms.max(1)),
            successes: AtomicU64::new(1),
            failures: AtomicU64::new(0),
            consecutive_failures: AtomicU32::new(0),
            cooldown_until: AtomicI64::new(0),
            last_success_at: AtomicI64::new(now),
            last_checked_at: AtomicI64::new(now),
            last_used_at: AtomicI64::new(0),
            hosts: Mutex::new(Vec::new()),
        })
    }

    /// Canonical key without credentials (precomputed; no allocation on the hot path).
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn is_healthy_for_host(&self, host: &str, now: i64) -> bool {
        if self.cooldown_until.load(Ordering::Relaxed) > now {
            return false;
        }
        if host.is_empty() {
            return true;
        }
        let Ok(hosts) = self.hosts.lock() else {
            return true;
        };
        !hosts
            .iter()
            .any(|h| &*h.host == host && h.cooldown_until > now)
    }

    fn has_affinity(&self, host: &str, now: i64) -> bool {
        if host.is_empty() {
            return false;
        }
        self.hosts
            .lock()
            .map(|hosts| {
                hosts
                    .iter()
                    .any(|h| &*h.host == host && now - h.last_ok < HOST_AFFINITY_SECS)
            })
            .unwrap_or(false)
    }

    fn with_host<F: FnOnce(&mut HostStat)>(&self, host: &str, now: i64, f: F) {
        let Ok(mut hosts) = self.hosts.lock() else {
            return;
        };
        if let Some(h) = hosts.iter_mut().find(|h| &*h.host == host) {
            h.touched = now;
            f(h);
            return;
        }
        if hosts.len() >= HOST_TABLE_CAP {
            if let Some(oldest) = hosts
                .iter()
                .enumerate()
                .min_by_key(|(_, h)| h.touched)
                .map(|(i, _)| i)
            {
                hosts.swap_remove(oldest);
            }
        }
        let mut h = HostStat {
            host: host.into(),
            cooldown_until: 0,
            last_ok: 0,
            fails: 0,
            touched: now,
        };
        f(&mut h);
        hosts.push(h);
    }

    fn mark_host_ok(&self, host: &str, now: i64) {
        self.with_host(host, now, |h| {
            h.last_ok = now;
            h.fails = 0;
            h.cooldown_until = 0;
        });
    }

    /// Per-host cool-down after an origin refusal: 2 min, doubling, capped at 30 min.
    fn mark_host_fail(&self, host: &str, now: i64) {
        self.with_host(host, now, |h| {
            h.fails = h.fails.saturating_add(1);
            let secs = (120_i64 << u32::from(h.fails.saturating_sub(1)).min(4)).min(1_800);
            h.cooldown_until = now + secs;
        });
    }

    pub fn score(&self) -> f64 {
        let lat = self.latency_ewma_ms.load(Ordering::Relaxed).max(1) as f64;
        let succ = self.successes.load(Ordering::Relaxed) as f64;
        let fail = self.failures.load(Ordering::Relaxed) as f64;
        let reliability = (succ + 1.0) / (succ + fail + 2.0);
        let streak = self.consecutive_failures.load(Ordering::Relaxed).min(8);
        (1_000.0 / (lat + 50.0)) * reliability * 0.5_f64.powi(streak as i32)
    }

    pub fn record_success(&self, latency_ms: u64) {
        let now = now_unix_secs();
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.cooldown_until.store(0, Ordering::Relaxed);
        self.last_success_at.store(now, Ordering::Relaxed);
        self.last_checked_at.store(now, Ordering::Relaxed);
        let prev = self.latency_ewma_ms.load(Ordering::Relaxed);
        let next = if prev == 0 {
            latency_ms.max(1)
        } else {
            (latency_ms as f64 * 0.3 + prev as f64 * 0.7).round().max(1.0) as u64
        };
        self.latency_ewma_ms.store(next, Ordering::Relaxed);
    }

    pub fn record_failure(&self, cooldown_base_secs: i64) -> u32 {
        let now = now_unix_secs();
        self.failures.fetch_add(1, Ordering::Relaxed);
        let streak = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        self.last_checked_at.store(now, Ordering::Relaxed);
        if cooldown_base_secs > 0 {
            let secs = cooldown_base_secs.saturating_mul(1_i64 << streak.saturating_sub(1).min(4));
            self.cooldown_until.store(now + secs, Ordering::Relaxed);
        }
        streak
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedProxy {
    endpoint: ProxyEndpoint,
    #[serde(default)]
    exit_country: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkingCacheFile {
    saved_at: i64,
    proxies: Vec<CachedProxy>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepSummary {
    pub sources_due: usize,
    pub fresh: usize,
    pub not_modified: usize,
    pub failed: usize,
    pub new_candidates: usize,
    pub listed_candidates: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyPoolStatus {
    pub running: bool,
    pub working_count: usize,
    pub healthy_count: usize,
    pub cooldown_count: usize,
    pub avg_latency_ms: Option<u64>,
    pub exit_countries: BTreeMap<String, usize>,
    pub registry: Value,
    pub sources: Value,
    pub candidates: Value,
    pub checks: Value,
    pub last_sweep_at: Option<i64>,
    pub last_round_at: Option<i64>,
    pub last_recheck_at: Option<i64>,
    pub sample_proxies: Vec<Value>,
}

struct PoolState {
    registry: RegistryState,
    sources: Vec<SourceState>,
    candidates: CandidateStore,
    sources_initialized: bool,
}

#[derive(Default)]
struct Counters {
    rounds: AtomicU64,
    checks: AtomicU64,
    tcp_dead: AtomicU64,
    proxy_failed: AtomicU64,
    judge_failed: AtomicU64,
    integrity_failed: AtomicU64,
    intercepting_evicted: AtomicU64,
    verified: AtomicU64,
    source_fetches: AtomicU64,
    source_not_modified: AtomicU64,
    source_failures: AtomicU64,
    source_bytes: AtomicU64,
    registry_refreshes: AtomicU64,
    registry_not_modified: AtomicU64,
}

pub struct ProxyPool {
    config: ProxyPoolConfig,
    checker: Arc<ProxyChecker>,
    http: reqwest::Client,
    working: RwLock<Arc<Vec<Arc<WorkingProxy>>>>,
    state: Mutex<PoolState>,
    seed_queue: Mutex<Vec<ProxyEndpoint>>,
    trusted_seeds: Mutex<Vec<ProxyEndpoint>>,
    rr_counter: AtomicUsize,
    running: AtomicBool,
    stopped: AtomicBool,
    refresh_requested: AtomicBool,
    ready_notify: Notify,
    discovery_notify: Notify,
    replenish_notify: Notify,
    sweep_lock: AsyncMutex<()>,
    round_lock: AsyncMutex<()>,
    recheck_lock: AsyncMutex<()>,
    counters: Counters,
    last_registry_ms: AtomicI64,
    last_sweep_ms: AtomicI64,
    last_round_ms: AtomicI64,
    last_recheck_ms: AtomicI64,
    empty_rounds: AtomicU32,
}

impl ProxyPool {
    pub fn new(config: ProxyPoolConfig) -> Arc<Self> {
        let checker = Arc::new(ProxyChecker::new(config.clone()));
        let http = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .no_proxy()
            .connect_timeout(Duration::from_secs(6))
            .timeout(config.fetch_timeout)
            .pool_max_idle_per_host(4)
            .gzip(true)
            .brotli(true)
            .build()
            .unwrap_or_default();
        let max_candidates = config.max_candidates_stored;
        Arc::new(Self {
            config,
            checker,
            http,
            working: RwLock::new(Arc::new(Vec::new())),
            state: Mutex::new(PoolState {
                registry: RegistryState::default(),
                sources: Vec::new(),
                candidates: CandidateStore::new(max_candidates),
                sources_initialized: false,
            }),
            seed_queue: Mutex::new(Vec::new()),
            trusted_seeds: Mutex::new(Vec::new()),
            rr_counter: AtomicUsize::new(0),
            running: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            refresh_requested: AtomicBool::new(false),
            ready_notify: Notify::new(),
            discovery_notify: Notify::new(),
            replenish_notify: Notify::new(),
            sweep_lock: AsyncMutex::new(()),
            round_lock: AsyncMutex::new(()),
            recheck_lock: AsyncMutex::new(()),
            counters: Counters::default(),
            last_registry_ms: AtomicI64::new(0),
            last_sweep_ms: AtomicI64::new(0),
            last_round_ms: AtomicI64::new(0),
            last_recheck_ms: AtomicI64::new(0),
            empty_rounds: AtomicU32::new(0),
        })
    }

    pub fn config(&self) -> &ProxyPoolConfig {
        &self.config
    }

    pub fn checker(&self) -> &Arc<ProxyChecker> {
        &self.checker
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire) && !self.stopped.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> Arc<Vec<Arc<WorkingProxy>>> {
        self.working
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|e| Arc::clone(&e.into_inner()))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cache_file(&self, name: &str) -> Option<PathBuf> {
        self.config.cache_dir.as_ref().map(|d| d.join(name))
    }

    /// Queues proxies for verification (`verify = true`) or trusts them immediately.
    pub fn seed_proxies(&self, proxies: impl IntoIterator<Item = ProxyEndpoint>, verify: bool) {
        let target = if verify { &self.seed_queue } else { &self.trusted_seeds };
        target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(proxies);
        if verify {
            self.replenish_notify.notify_one();
        } else if self.is_running() {
            self.inject_trusted();
        }
    }

    fn inject_trusted(&self) {
        let seeds: Vec<ProxyEndpoint> =
            std::mem::take(&mut *self.trusted_seeds.lock().unwrap_or_else(|e| e.into_inner()));
        for ep in seeds {
            if let Ok(wp) = WorkingProxy::new(ep, 50, None, None, &self.config.user_agent) {
                self.insert_working(Arc::new(wp));
            }
        }
    }

    /// Loads disk caches and spawns the three decoupled background loops (idempotent):
    /// discovery (registry + streaming sources), proactive replenishment, and health rechecking.
    pub fn start(self: &Arc<Self>) {
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.stopped.store(false, Ordering::Release);
        self.last_recheck_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
        self.load_caches();
        self.inject_trusted();
        let disc = Arc::clone(self);
        tokio::spawn(async move { disc.discovery_loop().await });
        let repl = Arc::clone(self);
        tokio::spawn(async move { repl.replenish_loop().await });
        let rech = Arc::clone(self);
        tokio::spawn(async move { rech.recheck_loop().await });
    }

    /// Starts the background daemon automatically whenever a Tokio runtime is active and the pool
    /// has not been explicitly stopped.
    pub fn ensure_started(self: &Arc<Self>) {
        if !self.is_running()
            && !self.stopped.load(Ordering::Acquire)
            && tokio::runtime::Handle::try_current().is_ok()
        {
            self.start();
        }
    }

    pub async fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.running.store(false, Ordering::Release);
        self.discovery_notify.notify_waiters();
        self.replenish_notify.notify_waiters();
        self.ready_notify.notify_waiters();
        self.save_working_cache().await;
    }

    pub fn trigger_refresh(&self) {
        self.refresh_requested.store(true, Ordering::Release);
        self.replenish_notify.notify_one();
        self.discovery_notify.notify_one();
    }

    /// High watermark below which background replenishment proactively tops the pool back up to
    /// `target_working_proxies`, well before `min_working_proxies` is ever reached.
    pub fn replenish_watermark(&self) -> usize {
        let target = self.config.target_working_proxies.max(1);
        if target <= 4 {
            target
        } else {
            ((target * 85) / 100)
                .max(self.config.min_working_proxies)
                .min(target)
        }
    }

    fn maybe_wake_replenish(&self) {
        let h = self.healthy_count();
        if h < self.config.min_working_proxies {
            self.trigger_refresh();
        } else if h < self.replenish_watermark() {
            self.replenish_notify.notify_one();
        }
    }

    pub fn healthy_count(&self) -> usize {
        let now = now_unix_secs();
        self.snapshot()
            .iter()
            .filter(|w| w.is_healthy_for_host("", now))
            .count()
    }

    pub fn working_snapshot(&self, include_cooldown: bool) -> Vec<Arc<WorkingProxy>> {
        let now = now_unix_secs();
        let mut items: Vec<Arc<WorkingProxy>> = self
            .snapshot()
            .iter()
            .filter(|w| include_cooldown || w.is_healthy_for_host("", now))
            .cloned()
            .collect();
        items.sort_by(|a, b| b.score().partial_cmp(&a.score()).unwrap_or(std::cmp::Ordering::Equal));
        items
    }

    pub async fn wait_until_ready(&self, min_proxies: usize, timeout: Duration) -> usize {
        if min_proxies == 0 {
            return self.healthy_count();
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.ready_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let n = self.healthy_count();
            if n >= min_proxies {
                return n;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return n;
            }
            self.trigger_refresh();
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return self.healthy_count();
            }
        }
    }

    /// Top-`k` healthy proxies for `host` in strategy order. O(n) partial selection over the
    /// copy-on-write snapshot; proxies that recently succeeded on `host` (×4) and proxies whose
    /// verified exit country is preferred (×2) are boosted.
    pub fn ordered_clients_for_host(&self, host: &str, k: usize) -> Vec<Arc<WorkingProxy>> {
        let snap = self.snapshot();
        if snap.is_empty() || k == 0 {
            return Vec::new();
        }
        let now = now_unix_secs();
        let healthy: Vec<&Arc<WorkingProxy>> =
            snap.iter().filter(|w| w.is_healthy_for_host(host, now)).collect();
        if healthy.is_empty() {
            return Vec::new();
        }
        let k = k.min(healthy.len());

        if self.config.selection_strategy == ProxySelectionStrategy::RoundRobin {
            let start = self.rr_counter.fetch_add(1, Ordering::Relaxed) % healthy.len();
            return (0..k)
                .map(|i| Arc::clone(healthy[(start + i) % healthy.len()]))
                .collect();
        }

        let prefer = &self.config.prefer_countries;
        let mut keyed: Vec<(f64, &Arc<WorkingProxy>)> = healthy
            .into_iter()
            .map(|w| {
                let mut boost = 1.0;
                if w.has_affinity(host, now) {
                    boost *= 4.0;
                }
                if !prefer.is_empty()
                    && w.exit_country
                        .as_deref()
                        .or(w.endpoint.country.as_deref())
                        .map(|c| prefer.iter().any(|p| p.eq_ignore_ascii_case(c)))
                        .unwrap_or(false)
                {
                    boost *= 2.0;
                }
                let key = match self.config.selection_strategy {
                    ProxySelectionStrategy::Fastest => {
                        w.latency_ewma_ms.load(Ordering::Relaxed).max(1) as f64 / boost
                    }
                    ProxySelectionStrategy::LeastRecentlyUsed => {
                        w.last_used_at.load(Ordering::Relaxed) as f64
                    }
                    // Efraimidis–Spirakis weighted sampling without replacement.
                    _ => -rand_unit().ln() / (w.score() * boost).max(1e-9),
                };
                (key, w)
            })
            .collect();
        let cmp = |a: &(f64, &Arc<WorkingProxy>), b: &(f64, &Arc<WorkingProxy>)| {
            a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
        };
        if k < keyed.len() {
            keyed.select_nth_unstable_by(k - 1, cmp);
            keyed.truncate(k);
        }
        keyed.sort_unstable_by(cmp);
        keyed.into_iter().map(|(_, w)| Arc::clone(w)).collect()
    }

    pub async fn acquire_client(self: &Arc<Self>, host: &str, timeout: Duration) -> Result<Arc<WorkingProxy>> {
        if !self.is_running() && !self.stopped.load(Ordering::Acquire) {
            self.start();
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.ready_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(w) = self.ordered_clients_for_host(host, 1).into_iter().next() {
                w.last_used_at.store(now_unix_secs(), Ordering::Relaxed);
                return Ok(w);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("no healthy working proxy available within {timeout:?}"));
            }
            self.trigger_refresh();
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }

    pub fn report_success(&self, wp: &Arc<WorkingProxy>, host: &str, latency_ms: u64) {
        wp.record_success(latency_ms);
        wp.mark_host_ok(host, now_unix_secs());
    }

    /// Origin refused this proxy (403/407/429/502/503): cool it down for `host` only.
    pub fn report_host_failure(&self, wp: &Arc<WorkingProxy>, host: &str, _status: u16) {
        if !host.is_empty() {
            wp.mark_host_fail(host, now_unix_secs());
        }
    }

    /// The proxy itself failed (connect/tunnel/TLS/timeout): global back-off, then eviction.
    pub fn report_transport_failure(&self, wp: &Arc<WorkingProxy>) {
        let streak = wp.record_failure(self.config.cooldown_base.as_secs() as i64);
        if streak >= self.config.max_consecutive_failures {
            self.evict(wp, self.config.quarantine_duration.as_secs() as i64);
        }
        self.maybe_wake_replenish();
    }

    /// Classifies a request error: a forged TLS certificate means the proxy intercepts traffic,
    /// so it is evicted at once and quarantined for 24 h; anything else counts as a transport
    /// failure.
    pub fn report_request_error(&self, wp: &Arc<WorkingProxy>, err: &(dyn std::error::Error + 'static)) {
        if super::checker::is_tls_interception(err) {
            wp.record_failure(self.config.cooldown_base.as_secs() as i64);
            self.counters.intercepting_evicted.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(proxy = %wp.endpoint.redacted_url(), "proxy presented a forged TLS certificate; evicted");
            self.evict(wp, 24 * 3600);
            self.maybe_wake_replenish();
        } else {
            self.report_transport_failure(wp);
        }
    }

    fn evict(&self, wp: &Arc<WorkingProxy>, quarantine_secs: i64) {
        let removed = {
            let mut guard = self.working.write().unwrap_or_else(|e| e.into_inner());
            if guard.iter().any(|w| Arc::ptr_eq(w, wp)) {
                let next: Vec<Arc<WorkingProxy>> =
                    guard.iter().filter(|w| !Arc::ptr_eq(w, wp)).cloned().collect();
                *guard = Arc::new(next);
                true
            } else {
                false
            }
        };
        if removed {
            if let Some(k) = wp.cand_key {
                self.lock_state()
                    .candidates
                    .quarantine(&k, now_unix_secs(), quarantine_secs);
            }
            tracing::debug!(proxy = %wp.endpoint.redacted_url(), "evicted failing proxy");
            self.maybe_wake_replenish();
        }
    }

    fn insert_working(&self, wp: Arc<WorkingProxy>) -> bool {
        let inserted = {
            let mut guard = self.working.write().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = guard.iter().find(|w| w.key() == wp.key()) {
                existing.record_success(wp.latency_ewma_ms.load(Ordering::Relaxed));
                false
            } else {
                let mut next: Vec<Arc<WorkingProxy>> = guard.iter().cloned().collect();
                next.push(wp);
                if next.len() > self.config.max_working_proxies {
                    next.sort_by(|a, b| {
                        b.score().partial_cmp(&a.score()).unwrap_or(std::cmp::Ordering::Equal)
                    });
                    next.truncate(self.config.max_working_proxies);
                }
                *guard = Arc::new(next);
                true
            }
        };
        self.ready_notify.notify_waiters();
        inserted
    }

    fn promote(&self, res: &ProxyCheckResult) -> bool {
        match WorkingProxy::new(
            res.endpoint.clone(),
            res.latency_ms.unwrap_or(500),
            res.exit_ip.clone(),
            res.exit_country.clone(),
            &self.config.user_agent,
        ) {
            Ok(wp) => self.insert_working(Arc::new(wp)),
            Err(_) => false,
        }
    }

    fn working_keys(&self) -> HashSet<CandKey> {
        self.snapshot().iter().filter_map(|w| w.cand_key).collect()
    }

    // ---------------------------------------------------------------------------------------
    // Registry & sources
    // ---------------------------------------------------------------------------------------

    fn load_caches(&self) {
        let Some(dir) = self.config.cache_dir.clone() else {
            self.ensure_sources(&[]);
            return;
        };
        let cached_registry = load_registry_cache(&dir).unwrap_or_default();
        if !cached_registry.is_empty() {
            let mut st = self.lock_state();
            st.registry.loaded_from_cache = true;
            for url in &self.config.registry_urls {
                st.registry.per_url.entry(url.clone()).or_default().entries = cached_registry.clone();
            }
        }
        self.ensure_sources(&cached_registry);

        if let Ok(raw) = std::fs::read_to_string(dir.join(WORKING_CACHE_FILE)) {
            if let Ok(file) = serde_json::from_str::<WorkingCacheFile>(&raw) {
                let eps: Vec<ProxyEndpoint> = file.proxies.into_iter().map(|c| c.endpoint).collect();
                if !eps.is_empty() {
                    tracing::info!(count = eps.len(), "re-verifying cached working proxies");
                    self.seed_proxies(eps, true);
                }
            }
        }
    }

    /// Reconciles the source list with registry entries plus operator sources.
    fn ensure_sources(&self, registry_entries: &[SourceSpec]) {
        let mut st = self.lock_state();
        let mut wanted: Vec<(SourceSpec, SourceOrigin)> = registry_entries
            .iter()
            .cloned()
            .map(|s| (s, SourceOrigin::Registry))
            .collect();
        wanted.extend(self.config.extra_sources.iter().cloned().map(|s| (s, SourceOrigin::Operator)));
        let mut wanted_set: HashMap<(String, super::ProxyProtocol), SourceOrigin> = HashMap::new();
        for (s, o) in &wanted {
            let e = wanted_set.entry((s.url.clone(), s.protocol)).or_insert(*o);
            if *o == SourceOrigin::Operator {
                *e = SourceOrigin::Operator;
            }
        }
        for src in st.sources.iter_mut() {
            match wanted_set.remove(&(src.spec.url.clone(), src.spec.protocol)) {
                Some(origin) => {
                    src.active = true;
                    src.origin = origin;
                }
                None => {
                    // Registry dropped it (or a previous operator source was removed).
                    src.active = false;
                    src.keys.clear();
                }
            }
        }
        for (spec, _) in wanted {
            let Some(o) = wanted_set.remove(&(spec.url.clone(), spec.protocol)) else {
                continue;
            };
            if st.sources.len() >= u16::MAX as usize {
                break;
            }
            let id = st.sources.len() as u16;
            st.sources.push(SourceState::new(id, spec, o));
        }
        st.sources_initialized = true;
    }

    /// Downloads every registry (conditional GET), merges sources, persists the union.
    pub async fn refresh_registry_now(&self) -> usize {
        let urls = self.config.registry_urls.clone();
        let now = now_unix_secs();
        if urls.is_empty() {
            self.ensure_sources(&[]);
            self.last_registry_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
            return self.active_source_count();
        }
        let prev: Vec<(String, Option<String>, Option<String>)> = {
            let st = self.lock_state();
            urls.iter()
                .map(|u| {
                    let s = st.registry.per_url.get(u);
                    (
                        u.clone(),
                        s.and_then(|s| (!s.entries.is_empty()).then(|| s.etag.clone()).flatten()),
                        s.and_then(|s| (!s.entries.is_empty()).then(|| s.last_modified.clone()).flatten()),
                    )
                })
                .collect()
        };
        let results = fetch_registries(&self.http, &urls, &prev, self.config.fetch_timeout).await;
        let union = {
            let mut st = self.lock_state();
            let mut any_ok = false;
            for (url, outcome, parsed) in results {
                let entry = st.registry.per_url.entry(url).or_default();
                match outcome {
                    FetchOutcome::NotModified => {
                        any_ok = true;
                        entry.last_ok_at = now;
                        entry.last_error = None;
                        self.counters.registry_not_modified.fetch_add(1, Ordering::Relaxed);
                    }
                    FetchOutcome::Fresh { etag, last_modified, .. } => {
                        let specs = parsed.unwrap_or_default();
                        if specs.is_empty() {
                            entry.last_error = Some("registry parsed to zero sources".into());
                        } else {
                            any_ok = true;
                            entry.entries = specs;
                            entry.etag = etag;
                            entry.last_modified = last_modified;
                            entry.last_ok_at = now;
                            entry.last_error = None;
                        }
                    }
                    FetchOutcome::Failed { error, .. } => entry.last_error = Some(error),
                }
            }
            st.registry.last_refresh_at = now;
            if any_ok {
                st.registry.consecutive_failures = 0;
            } else {
                st.registry.consecutive_failures = st.registry.consecutive_failures.saturating_add(1);
            }
            let refresh = self.config.registry_refresh_interval.as_secs() as i64;
            st.registry.next_refresh_at = if any_ok {
                now + refresh
            } else {
                let backoff = 60_i64 << st.registry.consecutive_failures.saturating_sub(1).min(6);
                now + backoff.min(refresh)
            };
            st.registry.union(&urls)
        };
        self.counters.registry_refreshes.fetch_add(1, Ordering::Relaxed);
        self.last_registry_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
        if !union.is_empty() {
            self.ensure_sources(&union);
            if let Some(dir) = self.config.cache_dir.as_ref() {
                save_registry_cache(dir, &union, now).await;
            }
        } else {
            self.ensure_sources(&[]);
        }
        let n = self.active_source_count();
        tracing::info!(sources = n, registry_entries = union.len(), "free proxy registry refreshed");
        n
    }

    fn active_source_count(&self) -> usize {
        self.lock_state().sources.iter().filter(|s| s.active).count()
    }

    /// Makes every active source due now (ops "refresh now"; the next sweep still sends
    /// `If-None-Match`, so unchanged lists cost a 304, not a download).
    pub fn mark_all_sources_due(&self) {
        let mut st = self.lock_state();
        for s in st.sources.iter_mut().filter(|s| s.active) {
            s.next_fetch_at = 0;
        }
    }

    fn accept_extracted(&self, ex: &ExtractedProxy) -> bool {
        let cfg = &self.config;
        if !cfg.allowed_protocols.is_empty() && !cfg.allowed_protocols.contains(&ex.protocol) {
            return false;
        }
        if !cfg.allow_private_ips && is_private_or_reserved_ip(ex.ip) {
            return false;
        }
        if let (Some(allow), Some(cc)) = (&cfg.countries, ex.country) {
            let c = String::from_utf8_lossy(&cc);
            if !allow.iter().any(|a| a.eq_ignore_ascii_case(&c)) {
                return false;
            }
        }
        true
    }

    fn rebuild_generation(st: &mut PoolState, now: i64) {
        let PoolState { sources, candidates, .. } = st;
        candidates.begin_generation();
        for src in sources.iter().filter(|s| s.active) {
            let w = src.weight();
            for k in &src.keys {
                candidates.mark_listed(*k, None, None, src.id, w, now);
            }
        }
    }

    fn has_due_sources(&self, now: i64) -> bool {
        self.lock_state()
            .sources
            .iter()
            .any(|s| s.active && s.next_fetch_at <= now)
    }

    /// Polls every due source (conditional GET, bounded concurrency, size cap), parses each fresh
    /// body off the async workers as soon as it arrives, flushes an early cold-start batch so
    /// slow/hanging lists never delay verification, and re-lists candidates into a new generation.
    pub async fn sweep_sources_now(&self) -> SweepSummary {
        let _guard = self.sweep_lock.lock().await;
        let now = now_unix_secs();
        if !self.lock_state().sources_initialized {
            self.ensure_sources(&[]);
        }
        let due: Vec<(u16, SourceSpec, Option<String>, Option<String>)> = {
            let st = self.lock_state();
            st.sources
                .iter()
                .filter(|s| s.active && s.next_fetch_at <= now)
                .map(|s| (s.id, s.spec.clone(), s.etag.clone(), s.last_modified.clone()))
                .collect()
        };
        let mut summary = SweepSummary {
            sources_due: due.len(),
            ..Default::default()
        };
        if due.is_empty() {
            self.last_sweep_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
            return summary;
        }

        let before = self.lock_state().candidates.len();
        let base = self.config.fetch_interval.as_secs().max(1) as i64;
        let http = &self.http;
        let timeout = self.config.fetch_timeout;
        let max_bytes = self.config.max_source_bytes;

        let mut stream = futures::stream::iter(due)
            .map(|(id, spec, etag, lm)| async move {
                let out =
                    conditional_get(http, &spec.url, etag.as_deref(), lm.as_deref(), timeout, max_bytes)
                        .await;
                // Parse each fresh body on the blocking pool as soon as its HTTP stream finishes,
                // rather than buffering every list in RAM until the slowest source times out.
                let (slim, parsed) = match out {
                    FetchOutcome::Fresh { body, bytes, etag, last_modified, truncated } => {
                        let proto = spec.protocol;
                        let list = tokio::task::spawn_blocking(move || extract_proxies(&body, proto))
                            .await
                            .unwrap_or_default();
                        (
                            FetchOutcome::Fresh {
                                body: String::new(),
                                bytes,
                                etag,
                                last_modified,
                                truncated,
                            },
                            Some(list),
                        )
                    }
                    other => (other, None),
                };
                (id, slim, parsed)
            })
            .buffer_unordered(self.config.source_concurrency.max(1));

        let mut early_flushed = false;
        let mut fresh_keys_accum = 0usize;

        while let Some((id, outcome, list)) = stream.next().await {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let now_item = now_unix_secs();
            let accepted_opt: Option<Vec<ExtractedProxy>> = list.map(|raw| {
                raw.into_iter()
                    .filter(|ex| self.accept_extracted(ex))
                    .collect()
            });

            let mut do_early_wake = false;
            {
                let mut st = self.lock_state();
                if let Some(src) = st.sources.get_mut(id as usize) {
                    src.last_fetch_at = now_item;
                    src.fetches += 1;
                    self.counters.source_fetches.fetch_add(1, Ordering::Relaxed);
                    // Up to 10 % jitter so sources do not all come due in the same second.
                    let mut jitter_seed = (now_item as u64) ^ (u64::from(id) << 32) ^ 0xA5A5_5A5A;
                    let jitter = (xorshift(&mut jitter_seed) % (base as u64 / 10 + 1)) as i64;
                    match outcome {
                        FetchOutcome::NotModified => {
                            summary.not_modified += 1;
                            src.not_modified += 1;
                            src.consecutive_failures = 0;
                            src.last_status = Some(304);
                            src.last_error = None;
                            self.counters.source_not_modified.fetch_add(1, Ordering::Relaxed);
                            src.next_fetch_at = now_item + base * src.interval_multiplier() + jitter;
                        }
                        FetchOutcome::Fresh { bytes, etag, last_modified, truncated, .. } => {
                            summary.fresh += 1;
                            summary.bytes += bytes;
                            self.counters.source_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                            src.consecutive_failures = 0;
                            src.last_status = Some(200);
                            src.last_error = truncated.then(|| "truncated at size cap".to_string());
                            src.last_bytes = bytes;
                            src.etag = etag;
                            src.last_modified = last_modified;
                            let accepted = accepted_opt.unwrap_or_default();
                            src.last_count = accepted.len();
                            src.keys = accepted.iter().map(CandKey::from_extracted).collect();
                            src.next_fetch_at = now_item + base * src.interval_multiplier() + jitter;
                            fresh_keys_accum += accepted.len();
                            for ex in &accepted {
                                st.candidates.seed_meta(
                                    CandKey::from_extracted(ex),
                                    ex.auth.as_ref(),
                                    ex.country,
                                    now_item,
                                );
                            }
                        }
                        FetchOutcome::Failed { status, error } => {
                            summary.failed += 1;
                            src.failures += 1;
                            src.consecutive_failures = src.consecutive_failures.saturating_add(1);
                            src.last_status = status;
                            src.last_error = Some(error);
                            self.counters.source_failures.fetch_add(1, Ordering::Relaxed);
                            let backoff = base
                                .saturating_mul(1_i64 << src.consecutive_failures.min(7))
                                .min(6 * 3600);
                            src.next_fetch_at = now_item + backoff + jitter;
                        }
                    }
                }
                // Cold-start / depleted fast-path: once the fastest ~10 lists stream in (enough for
                // multi-source consensus ranking without waiting on slow/hanging stragglers),
                // publish an early generation immediately.
                if !early_flushed
                    && fresh_keys_accum >= 500
                    && summary.fresh >= summary.sources_due.min(10)
                    && self.healthy_count() < self.config.min_working_proxies
                {
                    Self::rebuild_generation(&mut st, now_item);
                    early_flushed = true;
                    do_early_wake = true;
                }
            }
            if do_early_wake {
                self.empty_rounds.store(0, Ordering::Relaxed);
                self.last_sweep_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
                self.replenish_notify.notify_one();
            }
        }

        let end_now = now_unix_secs();
        let protect = self.working_keys();
        {
            let mut st = self.lock_state();
            Self::rebuild_generation(&mut st, end_now);
            summary.new_candidates = st.candidates.len().saturating_sub(before);
            st.candidates
                .prune(end_now, self.config.candidate_stale_after.as_secs() as i64, &protect);
            summary.listed_candidates = st.candidates.listed_count();
        }
        if summary.new_candidates > 0 {
            self.empty_rounds.store(0, Ordering::Relaxed);
        }
        self.last_sweep_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
        self.replenish_notify.notify_one();
        tracing::debug!(?summary, "free proxy source sweep finished");
        summary
    }

    // ---------------------------------------------------------------------------------------
    // Verification rounds & continuous background loops
    // ---------------------------------------------------------------------------------------

    async fn verify_seeds(&self) -> usize {
        let seeds: Vec<ProxyEndpoint> =
            std::mem::take(&mut *self.seed_queue.lock().unwrap_or_else(|e| e.into_inner()));
        if seeds.is_empty() {
            return 0;
        }
        self.checker.prepare().await;
        let (tx, mut rx) = mpsc::channel(seeds.len().clamp(1, 256));
        let task = tokio::spawn(Arc::clone(&self.checker).run_pipeline(seeds, tx));
        let mut promoted = 0;
        while let Some(res) = rx.recv().await {
            self.count_result(&res);
            if res.ok && self.promote(&res) {
                promoted += 1;
            }
        }
        let _ = task.await;
        if promoted > 0 {
            self.save_working_cache().await;
        }
        promoted
    }

    fn count_result(&self, res: &ProxyCheckResult) {
        let c = &self.counters;
        c.checks.fetch_add(1, Ordering::Relaxed);
        match res.stage {
            CheckStage::Tcp => c.tcp_dead.fetch_add(1, Ordering::Relaxed),
            CheckStage::Proxy => c.proxy_failed.fetch_add(1, Ordering::Relaxed),
            CheckStage::Judge => c.judge_failed.fetch_add(1, Ordering::Relaxed),
            CheckStage::Integrity => c.integrity_failed.fetch_add(1, Ordering::Relaxed),
            CheckStage::Ok => c.verified.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Verifies the highest-priority candidates until the pool reaches its target size or the
    /// per-round budget is spent. Returns the number of newly promoted proxies.
    pub async fn run_check_round(&self) -> usize {
        let _guard = self.round_lock.lock().await;
        let now = now_unix_secs();
        self.refresh_requested.store(false, Ordering::Release);
        // Healthy (not cooling down) proxies count towards the target; the working set itself
        // stays capped at `max_working_proxies` by `insert_working`.
        let target = self.config.target_working_proxies.max(1);
        if self.healthy_count() >= target {
            self.last_round_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
            return 0;
        }
        let exclude = self.working_keys();
        let (has_listed, endpoints): (bool, Vec<ProxyEndpoint>) = {
            let st = self.lock_state();
            if st.candidates.listed_count() == 0 {
                (false, Vec::new())
            } else {
                let picks = st.candidates.select_for_check(
                    self.config.max_checks_per_round,
                    now,
                    &exclude,
                    self.config.explore_fraction,
                    now_unix_ms() as u64,
                );
                let eps = picks
                    .into_iter()
                    .filter_map(|k| {
                        let e = st.candidates.get(&k)?;
                        let label = e
                            .sources()
                            .first()
                            .and_then(|id| st.sources.get(*id as usize))
                            .map(|s| s.label.clone())
                            .unwrap_or_default();
                        let mut ep = ProxyEndpoint::new(k.protocol, k.ip.to_string(), k.port)?
                            .with_source(label)
                            .with_country(e.country_str());
                        if let Some(a) = e.auth.as_ref() {
                            ep.username = Some(a.0.clone());
                            ep.password = Some(a.1.clone());
                        }
                        Some(ep)
                    })
                    .collect();
                (true, eps)
            }
        };
        if !has_listed {
            return 0;
        }
        self.counters.rounds.fetch_add(1, Ordering::Relaxed);
        if endpoints.is_empty() {
            self.empty_rounds.fetch_add(1, Ordering::Relaxed);
            self.last_round_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
            return 0;
        }

        self.checker.prepare().await;
        let (tx, mut rx) = mpsc::channel(512);
        let task = tokio::spawn(Arc::clone(&self.checker).run_pipeline(endpoints, tx));
        let base = self.config.dead_backoff_base.as_secs() as i64;
        let max = self.config.dead_backoff_max.as_secs() as i64;
        let mut promoted = 0usize;
        while let Some(res) = rx.recv().await {
            self.count_result(&res);
            if let Some(key) = CandKey::from_endpoint(&res.endpoint) {
                let mut st = self.lock_state();
                let srcs = st.candidates.record_check(&key, res.ok, now_unix_secs(), base, max);
                for id in srcs {
                    if let Some(s) = st.sources.get_mut(id as usize) {
                        s.checked += 1;
                        if res.ok {
                            s.verified += 1;
                        }
                    }
                }
            }
            if res.ok && self.promote(&res) {
                promoted += 1;
                if self.healthy_count() >= target {
                    break;
                }
            }
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
        }
        drop(rx);
        let _ = task.await;

        if promoted > 0 {
            self.empty_rounds.store(0, Ordering::Relaxed);
            self.save_working_cache().await;
        } else {
            self.empty_rounds.fetch_add(1, Ordering::Relaxed);
        }
        self.last_round_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
        tracing::info!(
            promoted,
            healthy = self.healthy_count(),
            working = self.snapshot().len(),
            "free proxy check round finished"
        );
        promoted
    }

    /// Re-verifies *idle* healthy proxies (active ones are passively verified by live traffic) AND
    /// *suspect* cooling-down proxies (`consecutive_failures > 0`) before their cooldown expires,
    /// so dead proxies are evicted in the background without ever leaking back to scraping jobs.
    pub async fn recheck_now(&self) -> (usize, usize) {
        let _guard = self.recheck_lock.lock().await;
        let now = now_unix_secs();
        let interval = self.config.recheck_interval.as_secs().max(1) as i64;
        let suspect_tick = (interval / 3).max(1);
        let due: Vec<Arc<WorkingProxy>> = self
            .snapshot()
            .iter()
            .filter(|w| {
                if w.consecutive_failures.load(Ordering::Relaxed) > 0 {
                    now - w.last_checked_at.load(Ordering::Relaxed) >= suspect_tick
                } else {
                    let last = w
                        .last_success_at
                        .load(Ordering::Relaxed)
                        .max(w.last_checked_at.load(Ordering::Relaxed));
                    now - last >= interval
                }
            })
            .cloned()
            .collect();
        self.last_recheck_ms.store(now_unix_ms() as i64, Ordering::Relaxed);
        if due.is_empty() {
            return (0, 0);
        }
        self.checker.prepare().await;
        let conc = self.config.recheck_concurrency.max(1);
        // Keep failed proxies cooling down past the next `suspect_tick` so they are re-probed (and
        // evicted on streak >= recheck_failures_to_evict) before any scrape job can select them.
        let fail_cooldown = (self.config.cooldown_base.as_secs() as i64).max(suspect_tick + 2);
        let mut kept = 0usize;
        let mut evicted = 0usize;
        let mut queue = due.into_iter();
        let mut inflight = FuturesUnordered::new();
        loop {
            while inflight.len() < conc {
                let Some(w) = queue.next() else {
                    break;
                };
                let checker = Arc::clone(&self.checker);
                inflight.push(async move {
                    let r = checker.judge_probe(&w.endpoint).await;
                    (w, r)
                });
            }
            let Some((w, res)) = inflight.next().await else {
                break;
            };
            self.count_result(&res);
            if res.ok {
                w.record_success(res.latency_ms.unwrap_or(500));
                kept += 1;
            } else {
                let streak = w.record_failure(fail_cooldown);
                if streak >= self.config.recheck_failures_to_evict {
                    self.evict(&w, self.config.quarantine_duration.as_secs() as i64);
                    evicted += 1;
                }
            }
        }
        self.maybe_wake_replenish();
        if evicted > 0 {
            self.save_working_cache().await;
        }
        tracing::debug!(kept, evicted, "background proxy recheck finished");
        (kept, evicted)
    }

    /// One full discovery pass (registry if never loaded, due sources, one check round).
    pub async fn refresh_now(&self) -> usize {
        if self.last_registry_ms.load(Ordering::Relaxed) == 0 {
            self.refresh_registry_now().await;
        }
        self.verify_seeds().await;
        self.sweep_sources_now().await;
        self.run_check_round().await
    }

    /// Background Loop 1: periodically refreshes source registries and streams due proxy lists.
    /// Decoupled from verification and health rechecks so a slow HTTP source never stalls either.
    async fn discovery_loop(self: Arc<Self>) {
        while !self.stopped.load(Ordering::Acquire) {
            let now = now_unix_secs();
            let registry_due = {
                let st = self.lock_state();
                self.last_registry_ms.load(Ordering::Relaxed) == 0
                    || (!self.config.registry_urls.is_empty() && st.registry.next_refresh_at <= now)
            };
            if registry_due {
                self.refresh_registry_now().await;
            }

            let fetch_ms = self.config.fetch_interval.as_millis() as i64;
            let since_sweep = now_unix_ms() as i64 - self.last_sweep_ms.load(Ordering::Relaxed);
            let urgent = self.refresh_requested.load(Ordering::Acquire)
                || self.healthy_count() < self.config.min_working_proxies;
            if self.last_sweep_ms.load(Ordering::Relaxed) == 0
                || since_sweep >= fetch_ms
                || (urgent && since_sweep >= fetch_ms.min(60_000))
                || self.has_due_sources(now_unix_secs())
            {
                self.sweep_sources_now().await;
            }

            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let now_ms = now_unix_ms() as i64;
            let mut wait_ms = fetch_ms - (now_ms - self.last_sweep_ms.load(Ordering::Relaxed));
            let next_registry = self.lock_state().registry.next_refresh_at * 1000;
            if !self.config.registry_urls.is_empty() && next_registry > 0 {
                wait_ms = wait_ms.min(next_registry - now_ms);
            }
            let wait = Duration::from_millis(wait_ms.clamp(100, 30_000) as u64);
            let notified = self.discovery_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let _ = tokio::time::timeout(wait, notified).await;
        }
    }

    /// Background Loop 2: proactively keeps the working pool topped up to `target_working_proxies`
    /// whenever churn drops healthy proxies below `replenish_watermark` or new lists arrive.
    async fn replenish_loop(self: Arc<Self>) {
        while !self.stopped.load(Ordering::Acquire) {
            self.verify_seeds().await;

            let healthy = self.healthy_count();
            let target = self.config.target_working_proxies.max(1);
            let watermark = self.replenish_watermark();
            let new_data = self.last_sweep_ms.load(Ordering::Relaxed)
                > self.last_round_ms.load(Ordering::Relaxed);
            let requested = self.refresh_requested.load(Ordering::Acquire);
            let since_round = now_unix_ms() as i64 - self.last_round_ms.load(Ordering::Relaxed);
            let backoff_ms = self.empty_round_backoff_ms();

            if healthy < target
                && (new_data || ((healthy < watermark || requested) && since_round >= backoff_ms))
            {
                let promoted = self.run_check_round().await;
                if promoted == 0 && self.healthy_count() < self.config.min_working_proxies {
                    self.discovery_notify.notify_one();
                }
                if promoted > 0 && self.healthy_count() < watermark {
                    tokio::task::yield_now().await;
                    continue;
                }
            } else if healthy >= target {
                self.refresh_requested.store(false, Ordering::Release);
            }

            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let now_ms = now_unix_ms() as i64;
            let healthy = self.healthy_count();
            let wait_ms = if healthy < watermark && healthy < target {
                (self.empty_round_backoff_ms()
                    - (now_ms - self.last_round_ms.load(Ordering::Relaxed)))
                .clamp(100, 15_000)
            } else {
                15_000
            };
            let notified = self.replenish_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.stopped.load(Ordering::Acquire)
                || !self.seed_queue.lock().map(|q| q.is_empty()).unwrap_or(true)
            {
                continue;
            }
            let _ = tokio::time::timeout(Duration::from_millis(wait_ms as u64), notified).await;
        }
    }

    /// Background Loop 3: continuously probes idle proxies and suspect cooling-down proxies so
    /// dead entries are evicted and replaced in the background before scraping jobs hit them.
    async fn recheck_loop(self: Arc<Self>) {
        let interval_ms = self.config.recheck_interval.as_millis() as i64;
        if interval_ms <= 0 {
            return;
        }
        let tick_ms = (interval_ms / 3).max(250);
        while !self.stopped.load(Ordering::Acquire) {
            let since = now_unix_ms() as i64 - self.last_recheck_ms.load(Ordering::Relaxed);
            if since >= tick_ms {
                self.recheck_now().await;
            }
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let rem = (tick_ms
                - (now_unix_ms() as i64 - self.last_recheck_ms.load(Ordering::Relaxed)))
            .clamp(100, 30_000);
            tokio::time::sleep(Duration::from_millis(rem as u64)).await;
        }
    }

    /// 1 s after a productive round, then 5 s × 2^(n-1) after `n` empty rounds (≤ fetch interval).
    fn empty_round_backoff_ms(&self) -> i64 {
        let n = self.empty_rounds.load(Ordering::Relaxed);
        if n == 0 {
            return 1_000;
        }
        let ms = 5_000_i64 << n.saturating_sub(1).min(6);
        ms.min((self.config.fetch_interval.as_millis() as i64).max(1_000))
    }

    async fn save_working_cache(&self) {
        let Some(path) = self.cache_file(WORKING_CACHE_FILE) else {
            return;
        };
        let proxies: Vec<CachedProxy> = self
            .snapshot()
            .iter()
            .map(|w| CachedProxy {
                endpoint: w.endpoint.clone(),
                exit_country: w.exit_country.clone(),
            })
            .collect();
        let file = WorkingCacheFile {
            saved_at: now_unix_secs(),
            proxies,
        };
        if let Ok(json) = serde_json::to_vec_pretty(&file) {
            write_atomic(&path, &json).await;
        }
    }

    // ---------------------------------------------------------------------------------------
    // Status
    // ---------------------------------------------------------------------------------------

    pub fn status(&self) -> ProxyPoolStatus {
        let now = now_unix_secs();
        let snap = self.snapshot();
        let healthy: Vec<&Arc<WorkingProxy>> =
            snap.iter().filter(|w| w.is_healthy_for_host("", now)).collect();
        let avg_latency_ms = (!healthy.is_empty()).then(|| {
            healthy
                .iter()
                .map(|w| w.latency_ewma_ms.load(Ordering::Relaxed))
                .sum::<u64>()
                / healthy.len() as u64
        });
        let mut exit_countries: BTreeMap<String, usize> = BTreeMap::new();
        for w in snap.iter() {
            let c = w.exit_country.clone().unwrap_or_else(|| "??".to_string());
            *exit_countries.entry(c).or_default() += 1;
        }
        let sample_proxies: Vec<Value> = healthy
            .iter()
            .take(5)
            .map(|w| {
                json!({
                    "proxy": w.endpoint.redacted_url(),
                    "exitCountry": w.exit_country,
                    "latencyMs": w.latency_ewma_ms.load(Ordering::Relaxed),
                    "successes": w.successes.load(Ordering::Relaxed),
                    "failures": w.failures.load(Ordering::Relaxed),
                    "source": w.endpoint.source,
                })
            })
            .collect();

        let c = &self.counters;
        let (registry, sources, candidates) = {
            let st = self.lock_state();
            let active: Vec<&SourceState> = st.sources.iter().filter(|s| s.active).collect();
            let mut by_yield: Vec<&SourceState> =
                active.iter().copied().filter(|s| s.checked > 0).collect();
            by_yield.sort_by(|a, b| {
                b.weight().partial_cmp(&a.weight()).unwrap_or(std::cmp::Ordering::Equal)
            });
            let registry = json!({
                "urls": self.config.registry_urls,
                "lastRefreshAt": (st.registry.last_refresh_at > 0).then_some(st.registry.last_refresh_at),
                "nextRefreshAt": (st.registry.next_refresh_at > 0).then_some(st.registry.next_refresh_at),
                "loadedFromCache": st.registry.loaded_from_cache,
                "refreshes": c.registry_refreshes.load(Ordering::Relaxed),
                "notModified": c.registry_not_modified.load(Ordering::Relaxed),
                "errors": st.registry.per_url.iter()
                    .filter_map(|(u, s)| s.last_error.as_ref().map(|e| json!({"url": u, "error": e})))
                    .collect::<Vec<_>>(),
            });
            let sources = json!({
                "active": active.len(),
                "operator": active.iter().filter(|s| s.origin == SourceOrigin::Operator).count(),
                "failing": active.iter().filter(|s| s.consecutive_failures > 0).count(),
                "fetches": c.source_fetches.load(Ordering::Relaxed),
                "notModified": c.source_not_modified.load(Ordering::Relaxed),
                "failures": c.source_failures.load(Ordering::Relaxed),
                "bytesDownloaded": c.source_bytes.load(Ordering::Relaxed),
                "topByYield": by_yield.iter().take(5).map(|s| json!({
                    "source": s.label,
                    "listed": s.last_count,
                    "checked": s.checked,
                    "verified": s.verified,
                })).collect::<Vec<_>>(),
            });
            let candidates = json!({
                "known": st.candidates.len(),
                "listed": st.candidates.listed_count(),
                "deadBackoff": st.candidates.backoff_count(now),
            });
            (registry, sources, candidates)
        };
        let checks = json!({
            "rounds": c.rounds.load(Ordering::Relaxed),
            "total": c.checks.load(Ordering::Relaxed),
            "tcpDead": c.tcp_dead.load(Ordering::Relaxed),
            "proxyFailed": c.proxy_failed.load(Ordering::Relaxed),
            "judgeFailed": c.judge_failed.load(Ordering::Relaxed),
            "integrityFailed": c.integrity_failed.load(Ordering::Relaxed),
            "interceptingEvicted": c.intercepting_evicted.load(Ordering::Relaxed),
            "verified": c.verified.load(Ordering::Relaxed),
        });
        let ms = |a: &AtomicI64| {
            let v = a.load(Ordering::Relaxed);
            (v > 0).then_some(v / 1000)
        };
        ProxyPoolStatus {
            running: self.is_running(),
            working_count: snap.len(),
            healthy_count: healthy.len(),
            cooldown_count: snap.len().saturating_sub(healthy.len()),
            avg_latency_ms,
            exit_countries,
            registry,
            sources,
            candidates,
            checks,
            last_sweep_at: ms(&self.last_sweep_ms),
            last_round_at: ms(&self.last_round_ms),
            last_recheck_at: ms(&self.last_recheck_ms),
            sample_proxies,
        }
    }

    pub fn status_json(&self) -> Value {
        serde_json::to_value(self.status()).unwrap_or_else(|_| json!({}))
    }
}
