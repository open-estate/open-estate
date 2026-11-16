//! Pipelined two-phase proxy verification.
//!
//! Phase A is a bare TCP connect (no TLS, no HTTP) with a short timeout at high concurrency —
//! most published proxies are dead, and this rejects them for the price of one SYN. Phase B sends
//! a real HTTPS request through each TCP-reachable proxy to an IP-echo judge with certificate
//! verification on (a TLS-intercepting proxy fails here), and records exit IP and exit country.
//! Both phases run concurrently and results stream out as soon as each proxy is decided.

use super::{ProxyEndpoint, ProxyPoolConfig};
use crate::shared::now_unix_secs;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Judges in preference order. Cloudflare's trace endpoint returns both the exit IP and the exit
/// country (`ip=…`, `loc=US`) in a single request; the others are IP-only fallbacks.
pub const DEFAULT_CHECK_URLS: &[&str] = &[
    "https://www.cloudflare.com/cdn-cgi/trace",
    "https://api.ipify.org?format=json",
    "https://icanhazip.com",
    "https://checkip.amazonaws.com",
];

/// Canary hosts for the integrity probe: real Open Estate data hosts that are not proxy-checker
/// endpoints. Live testing found "free proxies" that pass every well-known judge while
/// presenting forged certificates for exactly these kinds of hosts.
pub const DEFAULT_INTEGRITY_URLS: &[&str] = &[
    "https://geocoding.geo.census.gov/geocoder/",
    "https://services.arcgis.com/",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStage {
    /// Proxy port did not accept a TCP connection.
    Tcp,
    /// TCP worked but the proxy protocol/tunnel/TLS failed.
    Proxy,
    /// The request went through but the judge response was wrong (status, body, latency).
    Judge,
    /// The judge passed but TLS to a non-judge canary host failed: the proxy intercepts or
    /// blocks real traffic while whitelisting well-known checker endpoints.
    Integrity,
    /// Verified working.
    Ok,
}

/// `true` when `err` (or any error in its source chain) is a TLS certificate rejection — through
/// a proxy that means the proxy presented its own certificate, i.e. it intercepts TLS.
pub fn is_tls_interception(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        let s = e.to_string().to_ascii_lowercase();
        if s.contains("invalid peer certificate")
            || s.contains("certificate verify failed")
            || s.contains("unknownissuer")
            || s.contains("notvalidforname")
            || s.contains("unknown issuer")
        {
            return true;
        }
        cur = e.source();
    }
    false
}

#[derive(Debug, Clone)]
pub struct ProxyCheckResult {
    pub endpoint: ProxyEndpoint,
    pub ok: bool,
    pub stage: CheckStage,
    pub latency_ms: Option<u64>,
    pub exit_ip: Option<String>,
    pub exit_country: Option<String>,
    pub error: Option<String>,
}

impl ProxyCheckResult {
    fn failed(endpoint: ProxyEndpoint, stage: CheckStage, error: impl Into<String>) -> Self {
        Self {
            endpoint,
            ok: false,
            stage,
            latency_ms: None,
            exit_ip: None,
            exit_country: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeInfo {
    pub ip: IpAddr,
    pub country: Option<String>,
}

fn country2(raw: &str) -> Option<String> {
    let t = raw.trim();
    (t.len() == 2 && t.bytes().all(|b| b.is_ascii_alphabetic())).then(|| t.to_ascii_uppercase())
}

/// Parses IP-echo judge output: Cloudflare trace (`ip=` / `loc=` lines), JSON (`ip`, `origin`,
/// `query` + optional country fields) or a bare IP on the first line.
pub fn parse_judge_response(body: &str) -> Option<JudgeInfo> {
    let text = body.trim();
    if text.is_empty() || text.len() > 16 * 1024 {
        return None;
    }
    if text.starts_with('{') {
        let v: Value = serde_json::from_str(text).ok()?;
        let raw = ["ip", "origin", "query", "address"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))?;
        let ip = raw.split(',').next()?.trim().parse().ok()?;
        let country = ["country_code", "countryCode", "country", "loc"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .and_then(country2);
        return Some(JudgeInfo { ip, country });
    }
    if text.lines().any(|l| l.starts_with("ip=")) {
        let mut ip = None;
        let mut country = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("ip=") {
                ip = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("loc=") {
                country = country2(v);
            }
        }
        return ip.map(|ip| JudgeInfo { ip, country });
    }
    let first = text.lines().next()?.trim();
    let ip = first.split(',').next()?.trim().parse().ok()?;
    Some(JudgeInfo { ip, country: None })
}

struct ProbeFailure {
    stage: CheckStage,
    /// The proxy answered with HTTP (judge-specific problem) — worth one try on another judge.
    http_level: bool,
    latency_ms: Option<u64>,
    error: String,
}

pub struct ProxyChecker {
    config: ProxyPoolConfig,
    judge_rr: AtomicUsize,
    integrity_rr: AtomicUsize,
    /// Judge host → addresses, pinned into probe clients so SOCKS4/SOCKS5 probes (which resolve
    /// the target locally) do not trigger one DNS lookup per probe. `(addrs, resolved_at)`.
    judge_dns: RwLock<HashMap<String, (Vec<SocketAddr>, i64)>>,
    /// This host's own public IP, used to reject transparent proxies. `(ip, detected_at)`.
    own_ip: RwLock<Option<(IpAddr, i64)>>,
}

impl ProxyChecker {
    pub fn new(config: ProxyPoolConfig) -> Self {
        Self {
            config,
            judge_rr: AtomicUsize::new(0),
            integrity_rr: AtomicUsize::new(0),
            judge_dns: RwLock::new(HashMap::new()),
            own_ip: RwLock::new(None),
        }
    }

    pub fn config(&self) -> &ProxyPoolConfig {
        &self.config
    }

    /// Phase-A concurrency, capped at a third of the soft file-descriptor limit.
    pub fn tcp_concurrency(&self) -> usize {
        let cap = super::fd_soft_limit()
            .map(|l| (l / 3) as usize)
            .unwrap_or(usize::MAX)
            .max(16);
        self.config.tcp_concurrency.clamp(1, cap)
    }

    /// Phase-B concurrency, capped at a sixth of the soft file-descriptor limit.
    pub fn judge_concurrency(&self) -> usize {
        let cap = super::fd_soft_limit()
            .map(|l| (l / 6) as usize)
            .unwrap_or(usize::MAX)
            .max(8);
        self.config.check_concurrency.clamp(1, cap)
    }

    pub fn own_ip(&self) -> Option<IpAddr> {
        self.own_ip.read().ok().and_then(|g| g.map(|(ip, _)| ip))
    }

    /// Once per round: refresh pinned judge DNS (10 min TTL) and, when transparent proxies are
    /// rejected, this host's public IP (30 min TTL; a direct control-plane request to the judge).
    pub async fn prepare(&self) {
        let now = now_unix_secs();
        for url in self.config.check_urls.iter().chain(self.config.integrity_urls.iter()) {
            let Some((host, port)) = url::Url::parse(url)
                .ok()
                .and_then(|u| Some((u.host_str()?.to_string(), u.port_or_known_default()?)))
            else {
                continue;
            };
            if host.parse::<IpAddr>().is_ok() {
                continue;
            }
            let fresh = self
                .judge_dns
                .read()
                .ok()
                .and_then(|m| m.get(&host).map(|(_, at)| now - *at < 600))
                .unwrap_or(false);
            if fresh {
                continue;
            }
            let looked_up = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((host.clone(), port)),
            )
            .await;
            if let Ok(Ok(addrs)) = looked_up {
                // Port 0 = "use the request's port" for reqwest's resolve overrides.
                let list: Vec<SocketAddr> = addrs.map(|a| SocketAddr::new(a.ip(), 0)).collect();
                if !list.is_empty() {
                    if let Ok(mut m) = self.judge_dns.write() {
                        m.insert(host, (list, now));
                    }
                }
            }
        }

        if self.config.reject_transparent {
            let stale = self
                .own_ip
                .read()
                .ok()
                .map(|g| g.map(|(_, at)| now - at > 1_800).unwrap_or(true))
                .unwrap_or(true);
            if stale {
                if let Some(ip) = self.detect_own_ip().await {
                    if let Ok(mut g) = self.own_ip.write() {
                        *g = Some((ip, now));
                    }
                }
            }
        }
    }

    async fn detect_own_ip(&self) -> Option<IpAddr> {
        let client = reqwest::Client::builder()
            .user_agent(&self.config.user_agent)
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;
        for url in self.config.check_urls.iter().take(2) {
            if let Ok(resp) = client.get(url).send().await {
                if let Ok(body) = resp.text().await {
                    if let Some(info) = parse_judge_response(&body) {
                        return Some(info.ip);
                    }
                }
            }
        }
        None
    }

    fn pinned_addrs(&self, target_url: &str) -> Option<(String, Vec<SocketAddr>)> {
        let host = url::Url::parse(target_url).ok()?.host_str()?.to_string();
        let guard = self.judge_dns.read().ok()?;
        guard.get(&host).map(|(a, _)| (host, a.clone()))
    }

    fn build_probe_client(
        &self,
        ep: &ProxyEndpoint,
        target_url: &str,
    ) -> Result<reqwest::Client, String> {
        let proxy = reqwest::Proxy::all(ep.url()).map_err(|e| format!("invalid proxy URL: {e}"))?;
        let mut b = reqwest::Client::builder()
            .user_agent(&self.config.user_agent)
            .no_proxy()
            .proxy(proxy)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.config.connect_timeout)
            .timeout(self.config.request_timeout)
            .pool_max_idle_per_host(0);
        if ep.protocol.resolves_locally() {
            if let Some((host, addrs)) = self.pinned_addrs(target_url) {
                b = b.resolve_to_addrs(&host, &addrs);
            }
        }
        if !self.config.verify_ssl_targets {
            b = b.danger_accept_invalid_certs(true);
        }
        b.build().map_err(|e| format!("probe client: {e}"))
    }

    fn validates_ip(&self, target_url: &str) -> bool {
        if self.config.expected_substring.is_some() {
            return false;
        }
        self.config.validate_ip_response.unwrap_or_else(|| {
            DEFAULT_CHECK_URLS.contains(&target_url) || target_url.contains("/cdn-cgi/trace")
        })
    }

    async fn probe_url(
        &self,
        ep: &ProxyEndpoint,
        target_url: &str,
    ) -> Result<ProxyCheckResult, ProbeFailure> {
        let fail = |stage, http_level, latency_ms, error: String| ProbeFailure {
            stage,
            http_level,
            latency_ms,
            error,
        };
        let client = self
            .build_probe_client(ep, target_url)
            .map_err(|e| fail(CheckStage::Proxy, false, None, e))?;

        let start = Instant::now();
        let mut resp = client.get(target_url).send().await.map_err(|e| {
            let kind = if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "proxy connect/tunnel/TLS failed"
            } else {
                "request failed"
            };
            fail(CheckStage::Proxy, false, None, kind.to_string())
        })?;
        let status = resp.status().as_u16();
        let mut body: Vec<u8> = Vec::with_capacity(1024);
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    let room = 65_536usize.saturating_sub(body.len());
                    body.extend_from_slice(&c[..c.len().min(room)]);
                    if body.len() >= 65_536 {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    return Err(fail(CheckStage::Proxy, false, None, "body read failed".into()));
                }
            }
        }
        let latency_ms = start.elapsed().as_millis() as u64;

        if let Some(max) = self.config.max_latency_ms {
            if latency_ms > max {
                return Err(fail(
                    CheckStage::Judge,
                    false,
                    Some(latency_ms),
                    format!("latency {latency_ms}ms > {max}ms"),
                ));
            }
        }
        if !self.config.expected_statuses.contains(&status) {
            return Err(fail(
                CheckStage::Judge,
                true,
                Some(latency_ms),
                format!("unexpected HTTP {status}"),
            ));
        }
        let text = String::from_utf8_lossy(&body);
        let mut result = ProxyCheckResult {
            endpoint: ep.clone(),
            ok: true,
            stage: CheckStage::Ok,
            latency_ms: Some(latency_ms),
            exit_ip: None,
            exit_country: None,
            error: None,
        };
        if let Some(ref needle) = self.config.expected_substring {
            if !text.contains(needle.as_str()) {
                return Err(fail(
                    CheckStage::Judge,
                    true,
                    Some(latency_ms),
                    "expected substring missing".into(),
                ));
            }
            return Ok(result);
        }
        if self.validates_ip(target_url) {
            let Some(info) = parse_judge_response(&text) else {
                return Err(fail(
                    CheckStage::Judge,
                    true,
                    Some(latency_ms),
                    "judge response is not an IP echo".into(),
                ));
            };
            if self.config.reject_transparent && self.own_ip() == Some(info.ip) {
                return Err(fail(
                    CheckStage::Judge,
                    false,
                    Some(latency_ms),
                    "transparent proxy (exit IP is our own IP)".into(),
                ));
            }
            result.exit_ip = Some(info.ip.to_string());
            result.exit_country = info.country;
        }
        Ok(result)
    }

    /// Phase A only: can a TCP connection to the proxy port be opened?
    pub async fn tcp_probe(&self, ep: &ProxyEndpoint) -> Result<u64, String> {
        let start = Instant::now();
        let connect = async {
            match ep.ip() {
                Some(ip) => tokio::net::TcpStream::connect(SocketAddr::new(ip, ep.port)).await,
                None => tokio::net::TcpStream::connect((ep.host.as_str(), ep.port)).await,
            }
        };
        match tokio::time::timeout(self.config.tcp_timeout, connect).await {
            Ok(Ok(_stream)) => Ok(start.elapsed().as_millis() as u64),
            Ok(Err(e)) => Err(format!("tcp: {}", e.kind())),
            Err(_) => Err("tcp: timeout".to_string()),
        }
    }

    /// Phase B only: end-to-end request through the proxy to the primary judge, retrying once on
    /// another judge when the proxy answered but the judge response was unusable.
    pub async fn judge_probe(&self, ep: &ProxyEndpoint) -> ProxyCheckResult {
        let urls = &self.config.check_urls;
        if urls.is_empty() {
            return ProxyCheckResult::failed(ep.clone(), CheckStage::Judge, "no judge URLs");
        }
        if !self.config.allowed_protocols.is_empty()
            && !self.config.allowed_protocols.contains(&ep.protocol)
        {
            return ProxyCheckResult::failed(ep.clone(), CheckStage::Proxy, "disallowed protocol");
        }
        match self.probe_url(ep, &urls[0]).await {
            Ok(r) => r,
            Err(f) if f.http_level && urls.len() > 1 => {
                let i = 1 + self.judge_rr.fetch_add(1, Ordering::Relaxed) % (urls.len() - 1);
                match self.probe_url(ep, &urls[i]).await {
                    Ok(r) => r,
                    Err(f2) => self.failure(ep, f2),
                }
            }
            Err(f) => self.failure(ep, f),
        }
    }

    fn failure(&self, ep: &ProxyEndpoint, f: ProbeFailure) -> ProxyCheckResult {
        ProxyCheckResult {
            endpoint: ep.clone(),
            ok: false,
            stage: f.stage,
            latency_ms: f.latency_ms,
            exit_ip: None,
            exit_country: None,
            error: Some(f.error),
        }
    }

    pub async fn check_one(&self, ep: &ProxyEndpoint) -> ProxyCheckResult {
        if let Err(e) = self.tcp_probe(ep).await {
            return ProxyCheckResult::failed(ep.clone(), CheckStage::Tcp, e);
        }
        self.verify_probe(ep).await
    }

    /// TLS round-trip through the proxy to a canary host that is *not* a well-known checker
    /// (default: hosts Open Estate really scrapes). Any HTTP status passes — only a broken or
    /// intercepted TLS session fails. One small `HEAD` per newly verified proxy.
    async fn integrity_probe(&self, ep: &ProxyEndpoint) -> Result<(), String> {
        let urls = &self.config.integrity_urls;
        if urls.is_empty() {
            return Ok(());
        }
        let url = &urls[self.integrity_rr.fetch_add(1, Ordering::Relaxed) % urls.len()];
        let client = self.build_probe_client(ep, url)?;
        match client.head(url).send().await {
            Ok(_) => Ok(()),
            Err(e) if is_tls_interception(&e) => {
                Err("TLS interception: forged certificate for canary host".to_string())
            }
            Err(e) if e.is_timeout() => Err("canary host timed out through proxy".to_string()),
            Err(_) => Err("canary host unreachable/TLS broken through proxy".to_string()),
        }
    }

    /// Phase B for new candidates: judge request, then the canary integrity probe.
    pub async fn verify_probe(&self, ep: &ProxyEndpoint) -> ProxyCheckResult {
        let res = self.judge_probe(ep).await;
        if !res.ok {
            return res;
        }
        match self.integrity_probe(ep).await {
            Ok(()) => res,
            Err(e) => ProxyCheckResult {
                ok: false,
                stage: CheckStage::Integrity,
                error: Some(e),
                ..res
            },
        }
    }

    /// Runs both phases over `endpoints` and streams every decision into `tx`. Stops as soon
    /// as the receiver is dropped (e.g. once the pool reached its target size); in-flight probes
    /// are cancelled. TCP probing never runs more than `2 × judge_concurrency` ahead of the
    /// judge phase so reachable proxies are verified while they are still fresh.
    pub async fn run_pipeline(
        self: Arc<Self>,
        endpoints: Vec<ProxyEndpoint>,
        tx: mpsc::Sender<ProxyCheckResult>,
    ) {
        let tcp_conc = self.tcp_concurrency();
        let judge_conc = self.judge_concurrency();
        let mut pending = endpoints.into_iter();
        let mut tcp_inflight = FuturesUnordered::new();
        let mut judge_queue: VecDeque<ProxyEndpoint> = VecDeque::new();
        let mut judge_inflight = FuturesUnordered::new();

        loop {
            while tcp_inflight.len() < tcp_conc && judge_queue.len() < judge_conc * 2 {
                let Some(ep) = pending.next() else {
                    break;
                };
                let me = Arc::clone(&self);
                tcp_inflight.push(async move {
                    let r = me.tcp_probe(&ep).await;
                    (ep, r)
                });
            }
            while judge_inflight.len() < judge_conc {
                let Some(ep) = judge_queue.pop_front() else {
                    break;
                };
                let me = Arc::clone(&self);
                judge_inflight.push(async move { me.verify_probe(&ep).await });
            }
            if tcp_inflight.is_empty() && judge_inflight.is_empty() {
                break;
            }
            tokio::select! {
                biased;
                _ = tx.closed() => break,
                Some(res) = judge_inflight.next(), if !judge_inflight.is_empty() => {
                    if tx.send(res).await.is_err() {
                        break;
                    }
                }
                Some((ep, res)) = tcp_inflight.next(), if !tcp_inflight.is_empty() => {
                    match res {
                        Ok(_) => judge_queue.push_back(ep),
                        Err(e) => {
                            if tx.send(ProxyCheckResult::failed(ep, CheckStage::Tcp, e)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Checks everything and returns all results (convenience wrapper over the pipeline).
    pub async fn check_many(self: &Arc<Self>, endpoints: Vec<ProxyEndpoint>) -> Vec<ProxyCheckResult> {
        self.prepare().await;
        let n = endpoints.len();
        let (tx, mut rx) = mpsc::channel(n.clamp(1, 512));
        let task = tokio::spawn(Arc::clone(self).run_pipeline(endpoints, tx));
        let mut out = Vec::with_capacity(n);
        while let Some(r) = rx.recv().await {
            out.push(r);
        }
        let _ = task.await;
        out
    }
}
