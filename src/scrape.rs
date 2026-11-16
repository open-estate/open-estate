//! Unified outbound scrape & public-data egress framework (`open_estate::scrape`).
//!
//! Every outbound scrape or public-data HTTP request in Open Estate — `GET`, `POST`, `HEAD`, JSON,
//! plain text, CSV, form-encoded Overpass queries, multipart uploads (Census batch geocoder),
//! bounded binary tiles/archives (USGS 3DEP LiDAR `.laz` octree nodes, BTS noise PNG tiles,
//! CAASPP ZIPs), and line-streamed CSV feeds — goes through [`HttpClient::scrape`]
//! ([`ScrapeRequest`]).
//!
//! ### Pipeline guarantees
//! 1. **Offline test isolation**: rejects non-loopback traffic immediately when
//!    [`HttpClient::offline_loopback_only`] is active.
//! 2. **Credential-leak guard**: requests targeting `bypass_hosts`/loopback **or** carrying secret
//!    API credentials (`Authorization`, `X-Api-Key`, `X-Goog-Api-Key`, `X-RapidAPI-Key`, etc.)
//!    route directly so credentials never traverse third-party proxies.
//! 3. **Per-host concurrency governance**: acquires [`HttpClient`]'s per-domain semaphore by
//!    default (configurable via [`ScrapeRequest::host_concurrency`]).
//! 4. **Singleflight L1 (`moka`) + disk TTL cache**: coalesces concurrent identical cacheable
//!    `GET` scrapes and persists responses on disk when [`ScrapeRequest::cache_ttl_secs`] `> 0`.
//! 5. **Tiered Hybrid Egress & mode-aware in-flight failover**:
//!    - Optional Tier-1 static upstream proxies (`OUTBOUND_PROXY_URLS`, round-robin) → optional Tier-2
//!      warm background [`crate::proxy_pool::ProxyPool`] failover (`FREE_PROXY_POOL_ENABLED=true`),
//!      or direct HTTPS by default when no proxies are configured.
//!    - [`ScrapeMode::Standard`]: fails over across egress targets on proxy transport errors,
//!      mid-body stream disconnects, and non-direct proxy/gateway HTTP refusals (`403` static-ASN
//!      block, `407`, `502`, `503`), with optional exponential backoff (`retries`). Never rotates
//!      `ProxyPool` exit IPs to work around an origin `429` rate limit.
//!    - [`ScrapeMode::Polite`]: fails over **only** when the proxy itself breaks before reaching
//!      the origin (`Err` connect/tunnel/TLS/timeout/body-stream or proxy `407`). Any HTTP answer
//!      from the origin (including `403`/`429` or `ratelimited.*` redirect → `429`) is returned
//!      as-is without rotating exit IPs.
//! 6. **End-to-end health & TLS-integrity feedback**: records latency EWMA, per-host affinity,
//!    transport cooldowns, and immediate 24h MITM eviction across both header and body reads.

use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use axum::body::Bytes;
use backon::{ExponentialBuilder, Retryable};
use futures::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_TYPE};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::shared::{net_error_reason, now_unix_secs, url_host_label, EgressHandle, HttpClient};

/// Outbound failure & retry policy for [`ScrapeRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrapeMode {
    /// Default mode for public open-data, GIS, Socrata, Census, S3, Overpass, and bulk feeds.
    ///
    /// Fails over across resolved egress targets (static `OUTBOUND_PROXY_URLS` → warm `ProxyPool`) on proxy
    /// transport errors, mid-body stream disconnects, and non-direct proxy/gateway HTTP refusals
    /// (`403` static-ASN block, `407`, `502`, `503`), with optional exponential retries. Never
    /// rotates dynamic `ProxyPool` exit IPs on an origin `429` rate limit.
    #[default]
    Standard,
    /// Polite single-origin-shot mode for portals with strict rate/access limits.
    ///
    /// Fails over to the next proxy **only** when the proxy itself breaks before reaching the
    /// origin (`Err` connect/tunnel/TLS/timeout/body-stream or proxy `407`). Any HTTP response
    /// from the origin (including `403`/`429` or a redirect to `ratelimited.*`, normalized to
    /// `429`) is returned to the caller as-is without rotating exit IPs.
    Polite,
}

/// Cloneable request payload so proxy failover and retries can replay `POST`/`PUT` bodies
/// zero-copy via [`Bytes`].
#[derive(Debug, Clone, Default)]
pub enum ScrapeBody {
    #[default]
    Empty,
    Bytes {
        bytes: Bytes,
        content_type: Option<String>,
    },
}

impl ScrapeBody {
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}

/// Buffered HTTP response produced by [`ScrapeRequest::send_buffered`].
#[derive(Debug, Clone)]
pub struct BufferedScrapeResponse {
    status: StatusCode,
    effective_status: u16,
    headers: HeaderMap,
    final_url: Url,
    bytes: Bytes,
    proxy_label: Option<String>,
}

impl BufferedScrapeResponse {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Effective HTTP status code (normalizing `ratelimited.*` redirects to `429`).
    pub fn status_code(&self) -> u16 {
        self.effective_status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn final_url(&self) -> &Url {
        &self.final_url
    }

    /// Credential-free label of the proxy that served this response (`None` when `direct`).
    pub fn proxy_label(&self) -> Option<&str> {
        self.proxy_label.as_deref()
    }

    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    pub fn text_lossy(&self) -> String {
        match std::str::from_utf8(&self.bytes) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(&self.bytes).into_owned(),
        }
    }

    pub fn into_text(self) -> String {
        match String::from_utf8(self.bytes.to_vec()) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        let txt = self.text_lossy();
        parse_json_with_preview(&txt, self.final_url.as_str())
    }
}

/// Streaming HTTP response produced by [`ScrapeRequest::send_streaming`].
///
/// Use [`ScrapeRequest::send_buffered`], [`ScrapeRequest::bytes`], [`ScrapeRequest::text`], or
/// [`ScrapeRequest::json`] for normal bounded responses so mid-body disconnects automatically fail
/// over to another proxy. Use `send_streaming` when scanning multi-megabyte streams line-by-line
/// without buffering the full payload in memory.
pub struct ScrapeResponse {
    response: reqwest::Response,
    egress: EgressHandle,
}

impl ScrapeResponse {
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    pub fn status_code(&self) -> u16 {
        effective_status_code(&self.response)
    }

    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    pub fn final_url(&self) -> &Url {
        self.response.url()
    }

    pub fn egress(&self) -> &EgressHandle {
        &self.egress
    }

    pub fn into_parts(self) -> (reqwest::Response, EgressHandle) {
        (self.response, self.egress)
    }
}

/// Unified builder for outbound scrape & public-data HTTP requests.
#[derive(Clone)]
pub struct ScrapeRequest<'a> {
    http: &'a HttpClient,
    url: String,
    method: Method,
    mode: ScrapeMode,
    timeout_ms: u64,
    retries: usize,
    cache_ttl_secs: i64,
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
    body: ScrapeBody,
    max_bytes: Option<usize>,
    host_concurrency: bool,
    accept_statuses: Vec<u16>,
    direct: bool,
}

impl<'a> ScrapeRequest<'a> {
    pub fn new(http: &'a HttpClient, url: impl Into<String>) -> Self {
        Self {
            http,
            url: url.into(),
            method: Method::GET,
            mode: ScrapeMode::Standard,
            timeout_ms: 15_000,
            retries: 0,
            cache_ttl_secs: 0,
            headers: Vec::new(),
            query: Vec::new(),
            body: ScrapeBody::Empty,
            max_bytes: None,
            host_concurrency: true,
            accept_statuses: Vec::new(),
            direct: false,
        }
    }

    /// Sends the request straight out instead of through the proxy tiers.
    ///
    /// Only for keyless, bulk, public-domain open data (e.g. the USGS 3DEP LiDAR bucket in the
    /// AWS Open Data registry): such hosts have no rate limits, gain nothing from IP rotation, and
    /// multi-megabyte downloads through free proxies are slow and unreliable. Callers decide when
    /// the operator's explicit strict-egress setting still applies (see
    /// `modules::house3d::lidar::direct_egress_allowed`).
    pub fn direct(mut self, on: bool) -> Self {
        self.direct = on;
        self
    }

    pub fn method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    pub fn mode(mut self, mode: ScrapeMode) -> Self {
        self.mode = mode;
        self
    }

    /// Enables [`ScrapeMode::Polite`]: fails over across proxies only when the proxy itself breaks
    /// before reaching the origin; returns any origin HTTP response (`403`/`429`/etc.) as-is.
    pub fn polite(mut self) -> Self {
        self.mode = ScrapeMode::Polite;
        self
    }

    /// Enables [`ScrapeMode::Standard`] (the default).
    pub fn standard(mut self) -> Self {
        self.mode = ScrapeMode::Standard;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout_ms = (timeout.as_millis() as u64).max(1);
        self
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms.max(1);
        self
    }

    pub fn retries(mut self, retries: usize) -> Self {
        self.retries = retries;
        self
    }

    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl_secs = ttl.as_secs() as i64;
        self
    }

    pub fn cache_ttl_secs(mut self, ttl_secs: i64) -> Self {
        self.cache_ttl_secs = ttl_secs;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn headers(mut self, headers: &[(&str, &str)]) -> Self {
        for (k, v) in headers {
            self.headers.push(((*k).to_string(), (*v).to_string()));
        }
        self
    }

    pub fn query<K, V>(mut self, params: &[(K, V)]) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (k, v) in params {
            self.query
                .push((k.as_ref().to_string(), v.as_ref().to_string()));
        }
        self
    }

    /// Sets an `application/x-www-form-urlencoded` body (replayed zero-copy on proxy failover).
    pub fn form<K, V>(mut self, params: &[(K, V)]) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut encoded = String::new();
        for (idx, (k, v)) in params.iter().enumerate() {
            if idx > 0 {
                encoded.push('&');
            }
            encoded.push_str(&urlencoding::encode(k.as_ref()));
            encoded.push('=');
            encoded.push_str(&urlencoding::encode(v.as_ref()));
        }
        self.body = ScrapeBody::Bytes {
            bytes: Bytes::from(encoded.into_bytes()),
            content_type: Some("application/x-www-form-urlencoded".to_string()),
        };
        self
    }

    /// Serializes `value` as a JSON request body (replayed zero-copy on proxy failover).
    pub fn json_body<T: Serialize>(mut self, value: &T) -> Result<Self> {
        let vec = serde_json::to_vec(value)?;
        self.body = ScrapeBody::Bytes {
            bytes: Bytes::from(vec),
            content_type: Some("application/json".to_string()),
        };
        Ok(self)
    }

    /// Sets a `serde_json::Value` request body.
    pub fn json_value_body(mut self, value: &Value) -> Self {
        let vec = serde_json::to_vec(value).unwrap_or_default();
        self.body = ScrapeBody::Bytes {
            bytes: Bytes::from(vec),
            content_type: Some("application/json".to_string()),
        };
        self
    }

    /// Sets raw or multipart bytes as the request body (replayed zero-copy via [`Bytes::clone`]).
    pub fn body_bytes(
        mut self,
        bytes: impl Into<Bytes>,
        content_type: Option<impl Into<String>>,
    ) -> Self {
        self.body = ScrapeBody::Bytes {
            bytes: bytes.into(),
            content_type: content_type.map(Into::into),
        };
        self
    }

    /// Caps response body size; responses exceeding `max_bytes` abort streaming immediately
    /// without rotating across other proxies.
    pub fn max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = Some(max_bytes);
        self
    }

    /// Controls whether [`HttpClient`]'s per-host concurrency semaphore is acquired (default `true`).
    pub fn host_concurrency(mut self, enabled: bool) -> Self {
        self.host_concurrency = enabled;
        self
    }

    /// Treats an additional HTTP status (e.g. `404` on sparse map tiles) as an accepted non-error
    /// response in [`ScrapeMode::Standard`].
    pub fn allow_status(mut self, status: u16) -> Self {
        if !self.accept_statuses.contains(&status) {
            self.accept_statuses.push(status);
        }
        self
    }

    pub fn allow_statuses(mut self, statuses: &[u16]) -> Self {
        for &s in statuses {
            self = self.allow_status(s);
        }
        self
    }

    fn effective_url(&self) -> Result<String> {
        if self.query.is_empty() {
            return Ok(self.url.clone());
        }
        let mut parsed = Url::parse(&self.url)?;
        {
            let mut qp = parsed.query_pairs_mut();
            for (k, v) in &self.query {
                qp.append_pair(k, v);
            }
        }
        Ok(parsed.into())
    }

    /// Defense-in-depth check: never transmit secret API keys or bearer tokens through public
    /// third-party proxies even if a caller forgets to list the host in `bypass_hosts`.
    fn has_credential_header(&self) -> bool {
        self.headers.iter().any(|(k, _)| {
            let lower = k.trim().to_ascii_lowercase();
            lower == "authorization"
                || lower == "x-api-key"
                || lower == "x-goog-api-key"
                || lower.ends_with("-api-key")
                || lower.ends_with("_api_key")
                || lower.ends_with("subscription-key")
        })
    }

    fn build_reqwest(&self, client: &reqwest::Client, url_str: &str) -> reqwest::RequestBuilder {
        let mut req = client
            .request(self.method.clone(), url_str)
            .timeout(Duration::from_millis(self.timeout_ms));
        let mut has_explicit_ct = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("content-type") {
                has_explicit_ct = true;
            }
            req = req.header(k.as_str(), v.as_str());
        }
        if let ScrapeBody::Bytes {
            ref bytes,
            ref content_type,
        } = self.body
        {
            if !has_explicit_ct {
                if let Some(ct) = content_type {
                    req = req.header(CONTENT_TYPE, ct.as_str());
                }
            }
            req = req.body(bytes.clone());
        }
        req
    }

    /// Executes the request and buffers the full (optionally size-capped) body *inside* the
    /// proxy failover loop, so mid-body TCP resets from flaky proxies automatically fail over.
    pub async fn send_buffered(self) -> Result<BufferedScrapeResponse> {
        let final_url = self.effective_url()?;
        let host = Url::parse(&final_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        if self.http.is_offline_blocked() && !HttpClient::is_loopback_host(&host) {
            return Err(anyhow!(
                "offline test mode: external HTTP blocked for {final_url}"
            ));
        }

        let _permit = if self.host_concurrency {
            let sem = self.http.host_semaphore(&final_url);
            Some(
                sem.acquire_owned()
                    .await
                    .map_err(|_| anyhow!("Host semaphore closed"))?,
            )
        } else {
            None
        };

        let force_direct = self.direct || self.has_credential_header();
        match self.mode {
            ScrapeMode::Polite => self.execute_polite(&final_url, &host, force_direct).await,
            ScrapeMode::Standard => self.execute_standard(&final_url, &host, force_direct).await,
        }
    }

    async fn execute_polite(
        &self,
        url_str: &str,
        host: &str,
        force_direct: bool,
    ) -> Result<BufferedScrapeResponse> {
        let host_lbl = url_host_label(url_str);
        let targets = if force_direct {
            vec![self.http.direct_egress_target()]
        } else {
            self.http.resolve_egress_targets(host).await?
        };
        let n = targets.len();
        let mut last_err = anyhow!("{host_lbl} request failed");

        for (pos, target) in targets.into_iter().enumerate() {
            let egress = self.http.make_egress_handle(target, host.to_string());
            match self.build_reqwest(egress.client(), url_str).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let eff = effective_status_code(&resp);
                    let headers = resp.headers().clone();
                    let final_url = resp.url().clone();

                    if eff == 407 && !egress.is_direct() && pos + 1 < n {
                        egress.report_http_failure(407);
                        last_err = anyhow!("{host_lbl} proxy returned HTTP 407");
                        continue;
                    }

                    match read_body_capped(resp, self.max_bytes).await {
                        Ok(bytes) => {
                            if (200..300).contains(&eff) || self.accept_statuses.contains(&eff) {
                                egress.report_success();
                            } else if matches!(eff, 403 | 407 | 429 | 502 | 503)
                                && !egress.is_direct()
                            {
                                egress.report_host_cooldown_only(eff);
                            }
                            return Ok(BufferedScrapeResponse {
                                status,
                                effective_status: eff,
                                headers,
                                final_url,
                                bytes,
                                proxy_label: egress.proxy_label(),
                            });
                        }
                        Err(BodyReadError::TooLarge(max)) => {
                            egress.report_success();
                            return Err(anyhow!("response larger than {max} bytes"));
                        }
                        Err(BodyReadError::Transport(e)) => {
                            let reason = net_error_reason(&e, self.timeout_ms);
                            if egress.is_direct() {
                                return Err(anyhow!("{host_lbl} {reason}"));
                            }
                            egress.report_error(&e);
                            last_err = anyhow!("{host_lbl} {reason}");
                            if pos + 1 >= n {
                                return Err(last_err);
                            }
                        }
                    }
                }
                Err(e) => {
                    let reason = net_error_reason(&e, self.timeout_ms);
                    if egress.is_direct() {
                        return Err(anyhow!("{host_lbl} {reason}"));
                    }
                    egress.report_error(&e);
                    last_err = anyhow!("{host_lbl} {reason}");
                }
            }
        }
        Err(last_err)
    }

    async fn execute_standard(
        &self,
        url_str: &str,
        host: &str,
        force_direct: bool,
    ) -> Result<BufferedScrapeResponse> {
        let host_lbl = url_host_label(url_str);
        let backoff = ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(200))
            .with_max_delay(Duration::from_millis(1200))
            .with_max_times(self.retries);

        let fetch_once = || async {
            let targets = if force_direct {
                vec![self.http.direct_egress_target()]
            } else {
                self.http
                    .resolve_egress_targets(host)
                    .await
                    .map_err(|e| (false, e))?
            };
            let n = targets.len();
            let mut last_err = (false, anyhow!("{host_lbl} request failed"));

            for (pos, target) in targets.into_iter().enumerate() {
                let egress = self.http.make_egress_handle(target, host.to_string());
                let egress_lbl = egress
                    .proxy_label()
                    .unwrap_or_else(|| "direct".to_string());
                match self.build_reqwest(egress.client(), url_str).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        let code = status.as_u16();
                        let eff = effective_status_code(&resp);
                        let accepted =
                            status.is_success() || self.accept_statuses.contains(&code);
                        if accepted && eff != 429 {
                            let headers = resp.headers().clone();
                            let final_url = resp.url().clone();
                            match read_body_capped(resp, self.max_bytes).await {
                                Ok(bytes) => {
                                    egress.report_success();
                                    return Ok(BufferedScrapeResponse {
                                        status,
                                        effective_status: eff,
                                        headers,
                                        final_url,
                                        bytes,
                                        proxy_label: egress.proxy_label(),
                                    });
                                }
                                Err(BodyReadError::TooLarge(max)) => {
                                    egress.report_success();
                                    return Err((
                                        false,
                                        anyhow!("response larger than {max} bytes"),
                                    ));
                                }
                                Err(BodyReadError::Transport(e)) => {
                                    let reason = net_error_reason(&e, self.timeout_ms);
                                    if !egress.is_direct() {
                                        egress.report_error(&e);
                                        if pos + 1 < n {
                                            last_err = (true, anyhow!("{host_lbl} {reason}"));
                                            continue;
                                        }
                                    }
                                    return Err((true, anyhow!("{host_lbl} {reason}")));
                                }
                            }
                        }

                        let code = eff;
                        if code == 429 {
                            if let Some(ra) = resp
                                .headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.trim().parse::<u64>().ok())
                            {
                                tokio::time::sleep(Duration::from_secs(ra.min(4))).await;
                            }
                        }
                        // Never rotate across dynamic `ProxyPool` exit IPs to evade an origin 429.
                        if code == 429 && self.http.proxy_pool().is_some() && !force_direct {
                            if !egress.is_direct() {
                                egress.report_host_cooldown_only(429);
                            }
                            return Err((false, anyhow!("{host_lbl} returned HTTP 429")));
                        }
                        if !egress.is_direct()
                            && matches!(code, 403 | 407 | 429 | 502 | 503)
                        {
                            egress.report_http_failure(code);
                            if pos + 1 < n {
                                tracing::debug!(
                                    host = %host_lbl,
                                    code,
                                    egress = %egress_lbl,
                                    "scrape framework failover (HTTP status)"
                                );
                                last_err = (true, anyhow!("{host_lbl} returned HTTP {code}"));
                                continue;
                            }
                        }
                        let retryable = code == 429 || status.is_server_error();
                        return Err((retryable, anyhow!("{host_lbl} returned HTTP {code}")));
                    }
                    Err(e) => {
                        let reason = net_error_reason(&e, self.timeout_ms);
                        if !egress.is_direct() {
                            egress.report_error(&e);
                            if pos + 1 < n {
                                tracing::debug!(
                                    host = %host_lbl,
                                    reason = %reason,
                                    egress = %egress_lbl,
                                    "scrape framework failover (transport)"
                                );
                                last_err = (true, anyhow!("{host_lbl} {reason}"));
                                continue;
                            }
                        }
                        return Err((true, anyhow!("{host_lbl} {reason}")));
                    }
                }
            }
            Err(last_err)
        };

        fetch_once
            .retry(backoff)
            .when(|(retryable, _)| *retryable)
            .await
            .map_err(|(_, e)| e)
    }

    /// Sends headers across resolved egress targets and returns a streaming [`ScrapeResponse`].
    pub async fn send_streaming(self) -> Result<ScrapeResponse> {
        let final_url = self.effective_url()?;
        let host = Url::parse(&final_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        if self.http.is_offline_blocked() && !HttpClient::is_loopback_host(&host) {
            return Err(anyhow!(
                "offline test mode: external HTTP blocked for {final_url}"
            ));
        }

        let _permit = if self.host_concurrency {
            let sem = self.http.host_semaphore(&final_url);
            Some(
                sem.acquire_owned()
                    .await
                    .map_err(|_| anyhow!("Host semaphore closed"))?,
            )
        } else {
            None
        };

        let host_lbl = url_host_label(&final_url);
        let targets = if self.direct || self.has_credential_header() {
            vec![self.http.direct_egress_target()]
        } else {
            self.http.resolve_egress_targets(&host).await?
        };
        let n = targets.len();
        let mut last_err = anyhow!("{host_lbl} request failed");

        for (pos, target) in targets.into_iter().enumerate() {
            let egress = self.http.make_egress_handle(target, host.clone());
            match self.build_reqwest(egress.client(), &final_url).send().await {
                Ok(resp) => {
                    let code = effective_status_code(&resp);
                    let failover_http = match self.mode {
                        ScrapeMode::Polite => code == 407,
                        ScrapeMode::Standard => matches!(code, 403 | 407 | 502 | 503),
                    };
                    if !egress.is_direct() && failover_http && pos + 1 < n {
                        egress.report_http_failure(code);
                        last_err = anyhow!("{host_lbl} returned HTTP {code}");
                        continue;
                    }
                    return Ok(ScrapeResponse {
                        response: resp,
                        egress,
                    });
                }
                Err(e) => {
                    let reason = net_error_reason(&e, self.timeout_ms);
                    egress.report_error(&e);
                    if egress.is_direct() || pos + 1 >= n {
                        return Err(anyhow!("{host_lbl} {reason}"));
                    }
                    last_err = anyhow!("{host_lbl} {reason}");
                }
            }
        }
        Err(last_err)
    }

    /// Executes as a polite single-origin-shot request returning `(effective_status, body_text)`.
    pub async fn send_status_text(self) -> Result<(u16, String)> {
        let resp = self.polite().send_buffered().await?;
        Ok((resp.status_code(), resp.into_text()))
    }

    /// Executes the request and returns the raw response [`Bytes`].
    pub async fn bytes(self) -> Result<Bytes> {
        Ok(self.send_buffered().await?.into_bytes())
    }

    /// Executes the request and returns the decoded response text, using [`HttpClient`]'s
    /// singleflight L1 (`moka`) + disk TTL cache when `cache_ttl_secs > 0` on a standard `GET`.
    pub async fn text(self) -> Result<String> {
        let cacheable_get = self.method == Method::GET
            && self.mode == ScrapeMode::Standard
            && self.cache_ttl_secs > 0
            && self.body.is_empty()
            && self.accept_statuses.is_empty();

        if !cacheable_get {
            return Ok(self.send_buffered().await?.into_text());
        }

        let final_url = self.effective_url()?;
        let ttl_secs = self.cache_ttl_secs;
        let cache_key = format!("GET:{final_url}");

        if !self.http.is_bypassing_cache_reads() && self.headers.is_empty() {
            if let Some(hit) = self.http.read_cache(&cache_key, ttl_secs).await {
                return Ok(hit);
            }
            let http_cloned = self.http.clone();
            let ck = cache_key.clone();
            let url_owned = final_url.clone();
            let timeout_ms = self.timeout_ms;
            let retries = self.retries;
            let max_bytes = self.max_bytes;
            let host_concurrency = self.host_concurrency;
            let direct = self.direct;

            let res = self
                .http
                .l1_cache()
                .try_get_with(cache_key, async move {
                    if let Some(dir) = http_cloned.disk_cache_dir() {
                        let p = dir.join(format!("{}.cache", HttpClient::cache_hash(&ck)));
                        if let Ok(meta) = tokio::fs::metadata(&p).await {
                            if let Ok(modified) = meta.modified() {
                                let age = SystemTime::now()
                                    .duration_since(modified)
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(ttl_secs + 1);
                                if age <= ttl_secs {
                                    if let Ok(txt) = tokio::fs::read_to_string(&p).await {
                                        return Ok::<(i64, String), anyhow::Error>((
                                            now_unix_secs() + ttl_secs,
                                            txt,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    let mut inner = ScrapeRequest::new(&http_cloned, url_owned)
                        .timeout_ms(timeout_ms)
                        .retries(retries)
                        .host_concurrency(host_concurrency)
                        .direct(direct);
                    if let Some(mb) = max_bytes {
                        inner = inner.max_bytes(mb);
                    }
                    let txt = inner.send_buffered().await?.into_text();
                    if let Some(dir) = http_cloned.disk_cache_dir() {
                        let p = dir.join(format!("{}.cache", HttpClient::cache_hash(&ck)));
                        let _ = tokio::fs::write(p, &txt).await;
                    }
                    Ok((now_unix_secs() + ttl_secs, txt))
                })
                .await
                .map_err(|e| anyhow!("{}", e))?;
            return Ok(res.1);
        }

        if let Some(hit) = self.http.read_cache(&cache_key, ttl_secs).await {
            return Ok(hit);
        }
        let http_ref = self.http;
        let mut uncached = self;
        uncached.url = final_url;
        uncached.query.clear();
        uncached.cache_ttl_secs = 0;
        let txt = uncached.send_buffered().await?.into_text();
        http_ref.write_cache(&cache_key, &txt, ttl_secs).await;
        Ok(txt)
    }

    /// Executes the request and deserializes the response body as `T` (with char-safe preview on
    /// non-JSON error pages).
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        let url_for_err = self.url.clone();
        let txt = self.text().await?;
        parse_json_with_preview(&txt, &url_for_err)
    }

    /// Executes the request and parses the response body as [`serde_json::Value`].
    pub async fn json_value(self) -> Result<Value> {
        self.json::<Value>().await
    }
}

enum BodyReadError {
    TooLarge(usize),
    Transport(reqwest::Error),
}

async fn read_body_capped(
    resp: reqwest::Response,
    max_bytes: Option<usize>,
) -> std::result::Result<Bytes, BodyReadError> {
    let Some(max) = max_bytes else {
        return resp.bytes().await.map_err(BodyReadError::Transport);
    };
    if resp.content_length().map(|n| n as usize > max).unwrap_or(false) {
        return Err(BodyReadError::TooLarge(max));
    }
    let mut out: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BodyReadError::Transport)?;
        if out.len() + chunk.len() > max {
            return Err(BodyReadError::TooLarge(max));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

fn effective_status_code(resp: &reqwest::Response) -> u16 {
    let rate_limited_redirect = resp
        .url()
        .host_str()
        .map(|h| h.to_ascii_lowercase().starts_with("ratelimited."))
        .unwrap_or(false);
    if rate_limited_redirect {
        429
    } else {
        resp.status().as_u16()
    }
}

fn parse_json_with_preview<T: DeserializeOwned>(txt: &str, url_str: &str) -> Result<T> {
    serde_json::from_str::<T>(txt).map_err(|e| {
        // Char-safe preview: byte-slicing a body at a fixed offset can split a UTF-8 sequence.
        let preview: String = txt.chars().take(80).collect();
        anyhow!(
            "{} returned an unexpected (non-JSON) response: {} ({})",
            url_host_label(url_str),
            e,
            preview.split_whitespace().collect::<Vec<_>>().join(" ")
        )
    })
}
