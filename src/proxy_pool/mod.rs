//! Self-updating free proxy pool (`open_estate::proxy_pool`).
//!
//! Every stage is periodic and nothing depends on a hardcoded proxy list:
//!
//! 1. **Registry** — optional operator-configured proxy-source registries
//!    (`FREE_PROXY_REGISTRY_URLS`, empty by default), refreshed periodically with conditional
//!    requests and cached on disk.
//! 2. **Sources** — each source is polled with `If-None-Match` / `If-Modified-Since`, size-capped,
//!    parsed by one format-agnostic extractor (plain text, JSON, HTML tables) and backed off
//!    exponentially when it fails.
//! 3. **Candidates** — a deduplicated store ranks proxies by how many independent sources list
//!    them, weighted by each source's measured yield, and remembers dead proxies (exponential
//!    re-check backoff) so the same dead endpoints are not re-tested every round.
//! 4. **Verification** — a pipelined two-phase checker: a cheap TCP connect probe, then a real
//!    HTTPS request through the proxy to an IP-echo judge with certificate verification (rejects
//!    TLS-intercepting proxies), recording exit IP and exit country.
//! 5. **Continuous Background Daemon** — three decoupled background loops (discovery, proactive
//!    high-watermark replenishment, and idle/suspect health rechecking) run for the lifetime of
//!    the process so a warm pool of usable proxies is always ready before scraping jobs need one.
//! 6. **Serving** — [`crate::shared::HttpClient`] asks for the top-k healthy proxies per request
//!    (latency/reliability score, per-host affinity, preferred exit country) and reports every
//!    outcome back; active proxies are passively verified by live traffic, suspect cooling-down
//!    proxies are re-probed (and evicted if dead) before their cooldown expires.

pub mod candidates;
pub mod checker;
pub mod extract;
pub mod pool;
pub mod registry;

pub use checker::{
    is_tls_interception, parse_judge_response, CheckStage, JudgeInfo, ProxyCheckResult,
    ProxyChecker, DEFAULT_CHECK_URLS, DEFAULT_INTEGRITY_URLS,
};
pub use extract::{extract_proxies, parse_registry, ExtractedProxy, SourceSpec};
pub use pool::{ProxyPool, ProxyPoolStatus, WorkingProxy};
pub use registry::DEFAULT_REGISTRY_URLS;

use crate::shared::{is_truthy_flag, EnvConfig};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv6Addr};
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyProtocol {
    Http,
    Https,
    Socks4,
    Socks5,
    Socks5h,
}

impl ProxyProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks4 => "socks4",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }

    /// Parses a protocol name as written by users (`https` = TLS to the proxy itself).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "socks4" | "socks4a" => Some(Self::Socks4),
            "socks5" | "socks" => Some(Self::Socks5),
            "socks5h" => Some(Self::Socks5h),
            _ => None,
        }
    }

    /// Protocol of a scheme or group name found in a *public list*. Lists label HTTP proxies
    /// that support `CONNECT` as `https`, so that maps to [`ProxyProtocol::Http`].
    pub fn from_list_label(raw: &str) -> Option<Self> {
        match Self::parse(raw)? {
            Self::Https => Some(Self::Http),
            other => Some(other),
        }
    }

    /// `reqwest` resolves the target host locally for these protocols.
    pub(crate) fn resolves_locally(self) -> bool {
        matches!(self, Self::Socks4 | Self::Socks5)
    }
}

/// Protocol of a proxy source URL inferred from its path (`.../socks5.txt` → SOCKS5).
pub fn infer_protocol_from_url(url: &str) -> ProxyProtocol {
    let low = url.to_ascii_lowercase();
    let path = low.split_once("://").map(|(_, rest)| rest).unwrap_or(&low);
    if path.contains("socks5") {
        ProxyProtocol::Socks5
    } else if path.contains("socks4") {
        ProxyProtocol::Socks4
    } else {
        ProxyProtocol::Http
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxySelectionStrategy {
    #[default]
    WeightedRandom,
    RoundRobin,
    Fastest,
    LeastRecentlyUsed,
}

impl ProxySelectionStrategy {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "round_robin" | "roundrobin" | "rr" => Self::RoundRobin,
            "fastest" | "lowest_latency" => Self::Fastest,
            "lru" | "least_recently_used" => Self::LeastRecentlyUsed,
            _ => Self::WeightedRandom,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyEndpoint {
    pub protocol: ProxyProtocol,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Country hint published by the source list (the verified exit country lives on
    /// [`WorkingProxy`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default)]
    pub source: String,
}

impl ProxyEndpoint {
    pub fn new(protocol: ProxyProtocol, host: impl Into<String>, port: u16) -> Option<Self> {
        let clean_host = normalize_host(&host.into());
        if !is_valid_host(&clean_host) || port == 0 {
            return None;
        }
        Some(Self {
            protocol,
            host: clean_host,
            port,
            username: None,
            password: None,
            country: None,
            source: "manual".to_string(),
        })
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn with_country(mut self, country: Option<String>) -> Self {
        self.country = country
            .map(|c| c.trim().to_ascii_uppercase())
            .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()));
        self
    }

    pub fn formatted_host(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// Canonical identity without credentials (e.g. `http://1.2.3.4:8080`).
    pub fn key(&self) -> String {
        format!("{}://{}:{}", self.protocol.as_str(), self.formatted_host(), self.port)
    }

    /// Full proxy URL (with percent-encoded credentials) for `reqwest::Proxy::all`.
    pub fn url(&self) -> String {
        let host_port = format!("{}:{}", self.formatted_host(), self.port);
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => format!(
                "{}://{}:{}@{host_port}",
                self.protocol.as_str(),
                urlencoding::encode(u),
                urlencoding::encode(p)
            ),
            (Some(u), None) => {
                format!("{}://{}@{host_port}", self.protocol.as_str(), urlencoding::encode(u))
            }
            _ => format!("{}://{host_port}", self.protocol.as_str()),
        }
    }

    /// Credential-safe URL for logs, health endpoints and UI output.
    pub fn redacted_url(&self) -> String {
        let host_port = format!("{}:{}", self.formatted_host(), self.port);
        match &self.username {
            Some(u) => format!("{}://{u}:***@{host_port}", self.protocol.as_str()),
            None => format!("{}://{host_port}", self.protocol.as_str()),
        }
    }

    pub fn is_private_or_reserved(&self) -> bool {
        is_private_or_reserved_host(&self.host)
    }

    pub fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }

    /// Parses `ip:port`, `scheme://[user:pass@]host:port`, `[ipv6]:port` or `host:port:user:pass`
    /// (operator-supplied seeds; public lists go through [`extract_proxies`]).
    pub fn parse(raw: &str, default_protocol: ProxyProtocol, source: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let token = if trimmed.contains("://") {
            trimmed
        } else {
            trimmed.split_whitespace().next()?
        };

        let (protocol, host, port, username, password) = if token.contains("://") {
            let u = Url::parse(token).ok()?;
            let proto = ProxyProtocol::parse(u.scheme())?;
            let host = u.host_str()?.to_string();
            let port = u.port()?;
            let user = (!u.username().is_empty())
                .then(|| urlencoding::decode(u.username()).ok().map(|c| c.into_owned()))
                .flatten();
            let pass = u
                .password()
                .and_then(|p| urlencoding::decode(p).ok().map(|c| c.into_owned()));
            (proto, host, port, user, pass)
        } else if let Some(rest) = token.strip_prefix('[') {
            let (host, after) = rest.split_once(']')?;
            let port: u16 = after.strip_prefix(':')?.parse().ok()?;
            (default_protocol, host.to_string(), port, None, None)
        } else {
            match token.split(':').collect::<Vec<_>>().as_slice() {
                [h, p] => (default_protocol, (*h).to_string(), p.parse().ok()?, None, None),
                [h, p, user, pass] => (
                    default_protocol,
                    (*h).to_string(),
                    p.parse().ok()?,
                    Some((*user).to_string()),
                    Some((*pass).to_string()),
                ),
                _ => return None,
            }
        };

        let mut ep = Self::new(protocol, host, port)?;
        ep.username = username;
        ep.password = password;
        ep.source = source.to_string();
        Some(ep)
    }
}

pub(crate) fn normalize_host(host: &str) -> String {
    let h = host.trim();
    let h = h
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(h);
    h.to_ascii_lowercase()
}

pub(crate) fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

pub(crate) fn is_private_or_reserved_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 64) // CGNAT 100.64/10
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking 198.18/15
                || o[0] >= 240
        }
        IpAddr::V6(v6) => is_non_global_v6(v6),
    }
}

pub(crate) fn is_private_or_reserved_host(host: &str) -> bool {
    if host == "localhost" || host == "localhost.localdomain" {
        return true;
    }
    host.parse::<IpAddr>()
        .map(is_private_or_reserved_ip)
        .unwrap_or(false)
}

fn is_non_global_v6(v6: Ipv6Addr) -> bool {
    let seg0 = v6.segments()[0];
    v6.is_loopback()
        || v6.is_multicast()
        || v6.is_unspecified()
        || (seg0 & 0xffc0) == 0xfe80
        || (seg0 & 0xfe00) == 0xfc00
}

/// Soft `RLIMIT_NOFILE` of this process (Linux `/proc/self/limits`), used to cap probe
/// concurrency so verification can never exhaust file descriptors.
pub(crate) fn fd_soft_limit() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/self/limits").ok()?;
    let line = raw.lines().find(|l| l.starts_with("Max open files"))?;
    line.split_whitespace().nth(3)?.parse().ok()
}

#[derive(Debug, Clone)]
pub struct ProxyPoolConfig {
    // ---- Source discovery -------------------------------------------------------------------
    /// Registries listing proxy *sources* (TOML `[scraping.<protocol>] urls = [...]`, JSON or
    /// one URL per line). Empty disables registry discovery.
    pub registry_urls: Vec<String>,
    pub registry_refresh_interval: Duration,
    /// Operator-supplied sources, kept even when a registry drops them.
    pub extra_sources: Vec<SourceSpec>,
    /// How often each source is polled (conditional GET; GitHub raw files cache for 300 s).
    pub fetch_interval: Duration,
    pub fetch_timeout: Duration,
    pub source_concurrency: usize,
    pub max_source_bytes: usize,
    pub allow_private_ips: bool,
    pub allowed_protocols: Vec<ProxyProtocol>,
    /// Only keep candidates whose *listed* country is in this set (unknown countries pass).
    pub countries: Option<Vec<String>>,

    // ---- Candidate scheduling ---------------------------------------------------------------
    pub max_checks_per_round: usize,
    /// Share of each round reserved for never-checked candidates (keeps source yields honest).
    pub explore_fraction: f32,
    pub dead_backoff_base: Duration,
    pub dead_backoff_max: Duration,
    pub candidate_stale_after: Duration,
    pub max_candidates_stored: usize,

    // ---- Verification -----------------------------------------------------------------------
    pub check_urls: Vec<String>,
    /// Canary HTTPS hosts (not well-known checkers) used to catch proxies that pass judges but
    /// intercept TLS for everything else. Empty disables the canary probe.
    pub integrity_urls: Vec<String>,
    pub validate_ip_response: Option<bool>,
    pub expected_statuses: Vec<u16>,
    pub expected_substring: Option<String>,
    pub tcp_timeout: Duration,
    pub tcp_concurrency: usize,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub check_concurrency: usize,
    pub recheck_concurrency: usize,
    pub max_latency_ms: Option<u64>,
    pub recheck_interval: Duration,
    pub verify_ssl_targets: bool,
    /// Reject proxies whose exit IP equals this host's own public IP.
    pub reject_transparent: bool,

    // ---- Working set & serving ---------------------------------------------------------------
    pub min_working_proxies: usize,
    pub target_working_proxies: usize,
    pub max_working_proxies: usize,
    pub max_consecutive_failures: u32,
    pub recheck_failures_to_evict: u32,
    pub cooldown_base: Duration,
    pub quarantine_duration: Duration,
    pub selection_strategy: ProxySelectionStrategy,
    pub prefer_countries: Vec<String>,
    pub max_attempts_per_request: usize,
    pub acquire_timeout: Duration,
    pub cache_dir: Option<PathBuf>,
    pub user_agent: String,
}

impl Default for ProxyPoolConfig {
    fn default() -> Self {
        Self {
            registry_urls: DEFAULT_REGISTRY_URLS.iter().map(|s| (*s).to_string()).collect(),
            registry_refresh_interval: Duration::from_secs(6 * 3600),
            extra_sources: Vec::new(),
            fetch_interval: Duration::from_secs(300),
            fetch_timeout: Duration::from_secs(15),
            source_concurrency: 16,
            max_source_bytes: 8 * 1024 * 1024,
            allow_private_ips: false,
            allowed_protocols: vec![
                ProxyProtocol::Http,
                ProxyProtocol::Socks4,
                ProxyProtocol::Socks5,
                ProxyProtocol::Socks5h,
            ],
            countries: None,

            max_checks_per_round: 2_000,
            explore_fraction: 0.15,
            dead_backoff_base: Duration::from_secs(10 * 60),
            dead_backoff_max: Duration::from_secs(6 * 3600),
            candidate_stale_after: Duration::from_secs(24 * 3600),
            max_candidates_stored: 200_000,

            check_urls: DEFAULT_CHECK_URLS.iter().map(|s| (*s).to_string()).collect(),
            integrity_urls: DEFAULT_INTEGRITY_URLS.iter().map(|s| (*s).to_string()).collect(),
            validate_ip_response: None,
            expected_statuses: vec![200],
            expected_substring: None,
            tcp_timeout: Duration::from_millis(2_500),
            tcp_concurrency: 512,
            connect_timeout: Duration::from_millis(5_000),
            request_timeout: Duration::from_millis(8_000),
            check_concurrency: 128,
            recheck_concurrency: 32,
            max_latency_ms: Some(6_000),
            recheck_interval: Duration::from_secs(45),
            verify_ssl_targets: true,
            reject_transparent: true,

            min_working_proxies: 8,
            target_working_proxies: 40,
            max_working_proxies: 80,
            max_consecutive_failures: 3,
            recheck_failures_to_evict: 2,
            cooldown_base: Duration::from_secs(30),
            quarantine_duration: Duration::from_secs(15 * 60),
            selection_strategy: ProxySelectionStrategy::WeightedRandom,
            prefer_countries: vec!["US".to_string()],
            max_attempts_per_request: 3,
            acquire_timeout: Duration::from_secs(20),
            cache_dir: None,
            user_agent:
                "OpenEstate-Property-Research/2.0 (US Residential & Commercial Due-Diligence; +https://openestate.org)"
                    .to_string(),
        }
    }
}

fn env_u64(env: &EnvConfig, key: &str) -> Option<u64> {
    env.get(key).and_then(|s| s.trim().parse::<u64>().ok())
}

fn env_list(env: &EnvConfig, key: &str) -> Option<Vec<String>> {
    env.get(key).map(|raw| {
        raw.split([',', ';', '\n'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Parses `FREE_PROXY_SOURCES_URLS` entries: `https://…` (protocol inferred from the path) or
/// `socks5:https://…` / `socks4|https://…` with an explicit protocol.
pub fn parse_source_entry(raw: &str) -> Option<SourceSpec> {
    let t = raw.trim();
    let (proto, url) = if t.starts_with("http://") || t.starts_with("https://") {
        (None, t)
    } else {
        let (p, rest) = t.split_once([':', '|', '='])?;
        (ProxyProtocol::from_list_label(p), rest.trim())
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    Url::parse(url).ok()?;
    Some(SourceSpec {
        url: url.to_string(),
        protocol: proto.unwrap_or_else(|| infer_protocol_from_url(url)),
    })
}

impl ProxyPoolConfig {
    pub fn from_env(env: &EnvConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(urls) = env_list(env, "FREE_PROXY_REGISTRY_URLS") {
            cfg.registry_urls = urls
                .into_iter()
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                .collect();
        }
        if let Some(s) = env_u64(env, "FREE_PROXY_REGISTRY_REFRESH_SECS") {
            cfg.registry_refresh_interval = Duration::from_secs(s.max(60));
        }
        if let Some(entries) = env_list(env, "FREE_PROXY_SOURCES_URLS") {
            cfg.extra_sources = entries.iter().filter_map(|e| parse_source_entry(e)).collect();
        }
        if let Some(s) = env_u64(env, "FREE_PROXY_FETCH_INTERVAL_SECS") {
            cfg.fetch_interval = Duration::from_secs(s.max(30));
        }
        if let Some(s) = env_u64(env, "FREE_PROXY_RECHECK_INTERVAL_SECS") {
            cfg.recheck_interval = Duration::from_secs(s.max(15));
        }
        if let Some(n) = env_u64(env, "FREE_PROXY_MAX_CHECKS_PER_ROUND") {
            cfg.max_checks_per_round = (n as usize).clamp(10, 50_000);
        }
        if let Some(n) = env_u64(env, "FREE_PROXY_MIN_WORKING") {
            cfg.min_working_proxies = n as usize;
        }
        if let Some(n) = env_u64(env, "FREE_PROXY_TARGET_WORKING") {
            cfg.target_working_proxies = (n as usize).max(1);
        }
        if let Some(n) = env_u64(env, "FREE_PROXY_MAX_WORKING") {
            cfg.max_working_proxies = (n as usize).max(1);
        }
        cfg.target_working_proxies = cfg
            .target_working_proxies
            .max(cfg.min_working_proxies)
            .min(cfg.max_working_proxies);
        if let Some(n) = env_u64(env, "FREE_PROXY_CHECK_CONCURRENCY") {
            cfg.check_concurrency = (n as usize).clamp(1, 1_024);
        }
        if let Some(n) = env_u64(env, "FREE_PROXY_TCP_CONCURRENCY") {
            cfg.tcp_concurrency = (n as usize).clamp(1, 4_096);
        }
        if let Some(ms) = env_u64(env, "FREE_PROXY_TCP_TIMEOUT_MS") {
            cfg.tcp_timeout = Duration::from_millis(ms.max(200));
        }
        if let Some(ms) = env_u64(env, "FREE_PROXY_CHECK_TIMEOUT_MS") {
            cfg.request_timeout = Duration::from_millis(ms.max(500));
            cfg.connect_timeout = Duration::from_millis((ms * 2 / 3).max(500));
        }
        if let Some(ms) = env_u64(env, "FREE_PROXY_MAX_LATENCY_MS") {
            cfg.max_latency_ms = (ms > 0).then_some(ms);
        }
        if let Some(ms) = env_u64(env, "FREE_PROXY_ACQUIRE_TIMEOUT_MS") {
            cfg.acquire_timeout = Duration::from_millis(ms);
        }
        if let Some(n) = env_u64(env, "FREE_PROXY_MAX_ATTEMPTS") {
            cfg.max_attempts_per_request = (n as usize).clamp(1, 10);
        }
        if let Some(protos) = env_list(env, "FREE_PROXY_PROTOCOLS") {
            let parsed: Vec<ProxyProtocol> =
                protos.iter().filter_map(|p| ProxyProtocol::parse(p)).collect();
            if !parsed.is_empty() {
                cfg.allowed_protocols = parsed;
            }
        }
        if let Some(countries) = env_list(env, "FREE_PROXY_COUNTRIES") {
            let up: Vec<String> = countries.iter().map(|c| c.to_ascii_uppercase()).collect();
            cfg.countries = (!up.is_empty()).then_some(up);
        }
        if let Some(pref) = env_list(env, "FREE_PROXY_PREFER_COUNTRIES") {
            cfg.prefer_countries = pref.iter().map(|c| c.to_ascii_uppercase()).collect();
        }
        if let Some(urls) = env_list(env, "FREE_PROXY_CHECK_URLS") {
            let valid: Vec<String> = urls
                .into_iter()
                .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
                .collect();
            if !valid.is_empty() {
                cfg.check_urls = valid;
            }
        }
        if let Some(urls) = env_list(env, "FREE_PROXY_INTEGRITY_URLS") {
            // An empty value disables the canary probe.
            cfg.integrity_urls = urls
                .into_iter()
                .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
                .collect();
        }
        if let Some(strategy) = env.get("FREE_PROXY_STRATEGY") {
            cfg.selection_strategy = ProxySelectionStrategy::parse(strategy);
        }
        if let Some(dir) = env.get("FREE_PROXY_CACHE_DIR") {
            let t = dir.trim();
            if !t.is_empty() {
                cfg.cache_dir = Some(PathBuf::from(t));
            }
        }
        if env.get("FREE_PROXY_ALLOW_PRIVATE").map(is_truthy_flag) == Some(true) {
            cfg.allow_private_ips = true;
        }
        if let Some(v) = env.get("FREE_PROXY_REJECT_TRANSPARENT") {
            cfg.reject_transparent = is_truthy_flag(v);
        }
        cfg
    }
}

/// Server/runtime egress defaults:
/// * **Direct HTTPS by default** when neither `OUTBOUND_PROXY_URLS` nor `FREE_PROXY_POOL_ENABLED`
///   is set, so self-hosted open-source Docker deployments require zero external proxy infrastructure.
/// * Operators may opt into static HTTP/SOCKS5 proxies via `OUTBOUND_PROXY_URLS` and/or dynamic
///   public-proxy failover via `FREE_PROXY_POOL_ENABLED=true`.
pub fn apply_runtime_defaults(env: &EnvConfig) -> EnvConfig {
    env.clone()
}
