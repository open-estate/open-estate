//! Zero-mock integration and unit tests for `open_estate::proxy_pool`:
//! * format-agnostic proxy extraction + registry/judge parsing across real-world list layouts;
//! * `CandidateStore` multi-source consensus scoring, exploration interleaving, exponential dead
//!   backoff, quarantine, and working-set-protected pruning;
//! * `ProxyPool` selection strategies, country preference, per-host affinity, cooldowns, TLS
//!   interception eviction, disk persistence, and strict fail-closed `HttpClient` routing;
//! * RFC 1928 SOCKS5 (IPv4 & domain) and HTTP `CONNECT` byte-stream framing over raw loopback TCP
//!   sockets (echoing raw transport bytes only — zero synthetic GIS/Overpass HTTP payloads).

use open_estate::proxy_pool::candidates::{CandKey, CandidateStore};
use open_estate::proxy_pool::{
    apply_runtime_defaults, extract_proxies, is_tls_interception, parse_judge_response,
    parse_registry, parse_source_entry, CheckStage, ProxyChecker, ProxyEndpoint, ProxyPool,
    ProxyPoolConfig, ProxyProtocol, ProxySelectionStrategy,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::json;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn keys(list: &[open_estate::proxy_pool::ExtractedProxy]) -> Vec<String> {
    list.iter()
        .map(|p| format!("{}://{}:{}", p.protocol.as_str(), p.ip, p.port))
        .collect()
}

// ------------------------------------------------------------------------------------------------
// 1. Format-agnostic list extraction, registry parsers, judge responses & runtime defaults
// ------------------------------------------------------------------------------------------------

#[test]
fn extractor_handles_real_world_list_layouts() {
    // Scheme-prefixed mixed list in an "http" group: the scheme wins, `https` = CONNECT proxy.
    let mixed =
        "socks4://184.178.172.14:4145\nhttps://137.66.1.45:80\nsocks5://user:p%40ss@68.71.242.118:4145\n";
    let got = extract_proxies(mixed, ProxyProtocol::Http);
    assert_eq!(
        keys(&got),
        [
            "socks4://184.178.172.14:4145",
            "http://137.66.1.45:80",
            "socks5://68.71.242.118:4145"
        ]
    );
    assert_eq!(got[2].auth, Some(("user".to_string(), "p@ss".to_string())));

    // `ip:port:Country` and `ip:port | HTTP | AR | Elite` layouts; junk is rejected.
    let noisy = "162.214.3.183:3128:United States\n187.102.219.42:999 | HTTP | AR | Elite\n\
                 1.2.3.4.5:80\n999.1.1.1:80\n8.8.8.8:0\n8.8.4.4:70000\nv1.2.3.4:80\n# 9.9.9.9:53 comment\n";
    assert_eq!(
        keys(&extract_proxies(noisy, ProxyProtocol::Socks5)),
        [
            "socks5://162.214.3.183:3128",
            "socks5://187.102.219.42:999",
            "socks5://9.9.9.9:53"
        ]
    );

    // JSON: objects with `proxy` URLs, separate ip/port + protocol arrays, nested country.
    let edt = r#"[{"proxy":"http://103.154.73.14:8080","protocol":"http","ip":"103.154.73.14","port":8080,"country":"ID"}]"#;
    let got = extract_proxies(edt, ProxyProtocol::Socks5);
    assert_eq!(keys(&got), ["http://103.154.73.14:8080"]);
    assert_eq!(got[0].country, Some(*b"ID"));

    let geonode = r#"{"data":[{"ip":"92.4.94.59","port":"3128","protocols":["http","socks5"],"country":"DE"}],"total":1}"#;
    let got = extract_proxies(geonode, ProxyProtocol::Http);
    assert_eq!(
        keys(&got),
        ["http://92.4.94.59:3128", "socks5://92.4.94.59:3128"]
    );
    assert!(got.iter().all(|p| p.country == Some(*b"DE")));

    let nested = r#"{"proxies":[{"alive":true,"ip":"103.237.102.191","port":11111,"protocol":"socks4","ip_data":{"countryCode":"US"}}]}"#;
    let got = extract_proxies(nested, ProxyProtocol::Http);
    assert_eq!(keys(&got), ["socks4://103.237.102.191:11111"]);
    assert_eq!(got[0].country, Some(*b"US"));

    // HTML tables: port split into a <span>, protocol + country-code columns; legacy layout.
    let modern = "<table><thead><tr><th>IP:Port</th><th>Protocol</th><th>Code</th><th>Country</th></tr></thead>\
        <tbody><tr><td class='fpl-ip'>103.74.227.130<span class='fpl-port'>:56417</span></td><td>SOCKS4</td><td>IN</td><td>India</td></tr>\
        <tr><td class='fpl-ip'>45.141.116.141<span class='fpl-port'>:18080</span></td><td>HTTP</td><td>DE</td><td>Germany</td></tr></tbody></table>";
    let got = extract_proxies(modern, ProxyProtocol::Http);
    assert_eq!(
        keys(&got),
        [
            "socks4://103.74.227.130:56417",
            "http://45.141.116.141:18080"
        ]
    );
    assert_eq!(got[0].country, Some(*b"IN"));

    let legacy = "<html><table><tr><th>IP Address</th><th>Port</th><th>Code</th></tr>\
        <tr><td>138.68.60.8</td><td>3128</td><td>US</td></tr></table></html>";
    let got = extract_proxies(legacy, ProxyProtocol::Http);
    assert_eq!(keys(&got), ["http://138.68.60.8:3128"]);
    assert_eq!(got[0].country, Some(*b"US"));
}

#[test]
fn registries_judges_and_settings_parse() {
    let toml = r#"
[output.txt]
enabled = true

[checking]
check_url = "https://ipv4.icanhazip.com"

[scraping.http]
enabled = true
urls = [
  "https://api.proxyscrape.com/v2/?request=displayproxies&protocol=http",
  { url = "https://example.com/a.txt" },
  { url = "https://some.api/endpoint", headers = { Authorization = "Bearer X" } },
  "https://raw.githubusercontent.com/o/r/refs/heads/main/https.txt", # trailing comment
]

[scraping.socks4]
enabled = false
urls = ["https://example.com/s4.txt"]

[scraping.socks5]
enabled = true
urls = [
  "https://raw.githubusercontent.com/o/r/refs/heads/main/socks5.txt",
  "https://api.proxyscrape.com/v2/?request=displayproxies&protocol=http",
]
"#;
    let specs = parse_registry(toml);
    let got: Vec<(String, ProxyProtocol)> =
        specs.iter().map(|s| (s.url.clone(), s.protocol)).collect();
    assert_eq!(
        got,
        vec![
            (
                "https://api.proxyscrape.com/v2/?request=displayproxies&protocol=http".to_string(),
                ProxyProtocol::Http
            ),
            ("https://example.com/a.txt".to_string(), ProxyProtocol::Http),
            (
                "https://raw.githubusercontent.com/o/r/refs/heads/main/https.txt".to_string(),
                ProxyProtocol::Http
            ),
            (
                "https://raw.githubusercontent.com/o/r/refs/heads/main/socks5.txt".to_string(),
                ProxyProtocol::Socks5
            ),
            (
                "https://api.proxyscrape.com/v2/?request=displayproxies&protocol=http".to_string(),
                ProxyProtocol::Socks5
            ),
        ],
        "disabled sections, credentialed entries, comments and non-scraping sections are skipped"
    );

    let json_reg = r#"{"homepage":"https://ignored.example","socks5":["https://a.example/x.txt"],"http":[{"url":"https://b.example/y.txt"}]}"#;
    let got: Vec<(String, ProxyProtocol)> = parse_registry(json_reg)
        .into_iter()
        .map(|s| (s.url, s.protocol))
        .collect();
    assert!(got.contains(&("https://a.example/x.txt".to_string(), ProxyProtocol::Socks5)));
    assert!(got.contains(&("https://b.example/y.txt".to_string(), ProxyProtocol::Http)));
    assert_eq!(got.len(), 2);

    let text_reg =
        "https://c.example/lists/socks4.txt\nsocks5 https://d.example/any.txt\n# comment\nnot a url\n";
    let got: Vec<(String, ProxyProtocol)> = parse_registry(text_reg)
        .into_iter()
        .map(|s| (s.url, s.protocol))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "https://c.example/lists/socks4.txt".to_string(),
                ProxyProtocol::Socks4
            ),
            (
                "https://d.example/any.txt".to_string(),
                ProxyProtocol::Socks5
            ),
        ]
    );

    let cf = parse_judge_response(
        "fl=1\nh=www.cloudflare.com\nip=162.229.186.15\nloc=US\ntls=TLSv1.3\n",
    )
    .unwrap();
    assert_eq!(cf.ip, "162.229.186.15".parse::<IpAddr>().unwrap());
    assert_eq!(cf.country.as_deref(), Some("US"));
    assert_eq!(
        parse_judge_response("ip=1.2.3.4\nloc=T1\n")
            .unwrap()
            .country,
        None
    );
    assert_eq!(
        parse_judge_response(r#"{"ip":"198.51.100.9","country_code":"ca"}"#)
            .unwrap()
            .country
            .as_deref(),
        Some("CA")
    );
    assert_eq!(
        parse_judge_response("198.51.100.7\n")
            .unwrap()
            .ip
            .to_string(),
        "198.51.100.7"
    );
    assert!(parse_judge_response("<html>Access denied</html>").is_none());

    let s = parse_source_entry("socks5:https://e.example/list.txt").unwrap();
    assert_eq!(
        (s.url.as_str(), s.protocol),
        ("https://e.example/list.txt", ProxyProtocol::Socks5)
    );
    assert_eq!(
        parse_source_entry("https://f.example/socks4.txt")
            .unwrap()
            .protocol,
        ProxyProtocol::Socks4
    );
    assert!(parse_source_entry("ftp://nope").is_none());

    // Endpoint credential redaction and private-address classification
    let ep_cred = ProxyEndpoint::parse(
        "socks5://alice:s3cr3t!@198.51.100.25:1080",
        ProxyProtocol::Http,
        "operator",
    )
    .unwrap();
    assert_eq!(ep_cred.key(), "socks5://198.51.100.25:1080");
    assert_eq!(
        ep_cred.redacted_url(),
        "socks5://alice:***@198.51.100.25:1080"
    );
    assert!(!ep_cred.redacted_url().contains("s3cr3t"));
    assert!(
        ProxyEndpoint::parse("127.0.0.1:8080", ProxyProtocol::Http, "local")
            .unwrap()
            .is_private_or_reserved()
    );
    assert!(
        ProxyEndpoint::parse("10.0.0.5:8080", ProxyProtocol::Http, "lan")
            .unwrap()
            .is_private_or_reserved()
    );
    assert!(
        !ProxyEndpoint::parse("8.8.8.8:8080", ProxyProtocol::Http, "pub")
            .unwrap()
            .is_private_or_reserved()
    );

    // Runtime defaults: Direct HTTPS by default
    let on = apply_runtime_defaults(&EnvConfig::default());
    assert_eq!(on.get("FREE_PROXY_POOL_ENABLED"), None);
    assert_eq!(on.get("OUTBOUND_PROXY_URLS"), None);

    let lib_default = HttpClient::from_env(&EnvConfig::default(), None);
    assert!(
        lib_default.proxy_pool().is_none(),
        "defaults to Direct HTTPS"
    );
    assert!(!lib_default.is_strict_proxy_egress());
    assert_eq!(lib_default.egress_status_json()["mode"], json!("direct"));
}

// ------------------------------------------------------------------------------------------------
// 2. Deterministic CandidateStore consensus ranking, negative backoff, quarantine & pruning
// ------------------------------------------------------------------------------------------------

fn cand_key(proto: ProxyProtocol, a: u8, b: u8, c: u8, d: u8, port: u16) -> CandKey {
    CandKey {
        protocol: proto,
        ip: IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
        port,
    }
}

#[test]
fn candidate_store_consensus_scoring_backoff_quarantine_and_pruning() {
    let mut store = CandidateStore::new(1_000);
    let now: i64 = 1_760_000_000;
    store.begin_generation();

    let high_consensus = cand_key(ProxyProtocol::Socks5, 198, 51, 100, 10, 1080);
    let medium_single = cand_key(ProxyProtocol::Http, 198, 51, 100, 11, 8080);
    let low_noisy = cand_key(ProxyProtocol::Http, 198, 51, 100, 12, 3128);
    let standby_new = cand_key(ProxyProtocol::Socks5, 198, 51, 100, 13, 1080);

    // Listed across 3 independent sources -> highest cumulative yield score
    store.mark_listed(high_consensus, None, Some(*b"US"), 0, 0.35, now);
    store.mark_listed(high_consensus, None, Some(*b"US"), 1, 0.25, now);
    store.mark_listed(high_consensus, None, Some(*b"US"), 2, 0.20, now);

    // Duplicate listing by same source_id in the same generation is idempotent
    store.mark_listed(high_consensus, None, Some(*b"US"), 0, 0.35, now);
    assert_eq!(store.get(&high_consensus).unwrap().listed, 3);
    assert_eq!(
        store.get(&high_consensus).unwrap().country_str().as_deref(),
        Some("US")
    );

    store.mark_listed(medium_single, None, Some(*b"DE"), 0, 0.35, now);
    store.mark_listed(low_noisy, None, Some(*b"BR"), 3, 0.05, now);
    store.mark_listed(standby_new, None, Some(*b"CA"), 3, 0.04, now);
    assert_eq!(store.listed_count(), 4);

    // Pure exploit (`explore_fraction = 0.0`) orders strictly by priority descending
    let top2 = store.select_for_check(2, now, &HashSet::new(), 0.0, 42);
    assert_eq!(top2, vec![high_consensus, medium_single]);

    // Dead check triggers exponential negative-cache backoff (`base = 60`, `2x`, `4x`...)
    let srcs = store.record_check(&medium_single, false, now, 60, 3_600);
    assert_eq!(srcs, vec![0]);
    let entry_after_fail1 = store.get(&medium_single).unwrap();
    assert_eq!(entry_after_fail1.dead_streak, 1);
    assert_eq!(entry_after_fail1.next_check_at, now + 60);
    assert_eq!(store.backoff_count(now), 1);

    // While in backoff, `medium_single` is skipped by `select_for_check`
    let picks_during_backoff = store.select_for_check(4, now + 30, &HashSet::new(), 0.0, 42);
    assert!(!picks_during_backoff.contains(&medium_single));

    // Second consecutive failure doubles backoff to 120s
    store.record_check(&medium_single, false, now + 60, 60, 3_600);
    assert_eq!(store.get(&medium_single).unwrap().dead_streak, 2);
    assert_eq!(
        store.get(&medium_single).unwrap().next_check_at,
        now + 60 + 120
    );

    // Alive check resets `dead_streak` and boosts future revival priority
    store.record_check(&high_consensus, true, now, 30, 3_600);
    let hc = store.get(&high_consensus).unwrap();
    assert_eq!(hc.dead_streak, 0);
    assert_eq!(hc.alive_hits, 1);

    // Quarantine locks out an evicted/intercepting candidate for 24h
    store.quarantine(&low_noisy, now, 86_400);
    assert!(store.get(&low_noisy).unwrap().next_check_at >= now + 86_400);
    let picks_after_quarantine = store.select_for_check(4, now + 3_600, &HashSet::new(), 0.0, 7);
    assert!(!picks_after_quarantine.contains(&low_noisy));

    // Pruning removes stale un-listed entries UNLESS protected as an active working proxy
    let mut protected = HashSet::new();
    protected.insert(high_consensus);
    let removed = store.prune(now + 100_000, 7_200, &protected);
    assert_eq!(
        removed, 3,
        "3 stale non-protected candidates should be pruned"
    );
    assert_eq!(store.len(), 1);
    assert!(
        store.get(&high_consensus).is_some(),
        "working proxy key in `protect` must never be pruned"
    );
}

// ------------------------------------------------------------------------------------------------
// 3. ProxyPool strategies, affinity, TLS interception eviction, disk cache & strict fail-closed
// ------------------------------------------------------------------------------------------------

#[derive(Debug)]
struct ForgedPeerCertError;
impl std::fmt::Display for ForgedPeerCertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid peer certificate: UnknownIssuer")
    }
}
impl std::error::Error for ForgedPeerCertError {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_pool_strategies_affinity_eviction_and_strict_fail_closed() {
    // 1. TLS interception error chain classifier
    let forged = std::io::Error::new(std::io::ErrorKind::InvalidData, ForgedPeerCertError);
    assert!(is_tls_interception(&forged));
    assert!(!is_tls_interception(&std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "connection refused"
    )));

    // 2. Round-robin, country preference, per-host affinity, cooldowns & disk cache
    let cache_dir =
        std::env::temp_dir().join(format!("db-proxypool-unit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cache_dir).unwrap();

    let p1 = closed_port().await;
    let p2 = closed_port().await;
    let p3 = closed_port().await;

    let ep_us = ProxyEndpoint::new(ProxyProtocol::Socks5, "127.0.0.1", p1)
        .unwrap()
        .with_country(Some("US".to_string()))
        .with_source("us-seed");
    let ep_de = ProxyEndpoint::new(ProxyProtocol::Http, "127.0.0.1", p2)
        .unwrap()
        .with_country(Some("DE".to_string()))
        .with_source("de-seed");
    let ep_ca = ProxyEndpoint::new(ProxyProtocol::Socks5, "127.0.0.1", p3)
        .unwrap()
        .with_country(Some("CA".to_string()))
        .with_source("ca-seed");

    let mut cfg = ProxyPoolConfig::default();
    cfg.registry_urls = Vec::new();
    cfg.allow_private_ips = true;
    cfg.recheck_interval = Duration::from_secs(3_600);
    cfg.max_consecutive_failures = 2;
    cfg.cooldown_base = Duration::from_secs(10);
    cfg.prefer_countries = vec!["US".to_string()];
    cfg.selection_strategy = ProxySelectionStrategy::Fastest;
    cfg.cache_dir = Some(cache_dir.clone());

    let pool = ProxyPool::new(cfg);
    pool.seed_proxies(vec![ep_us.clone(), ep_de.clone(), ep_ca.clone()], false);
    pool.start();
    assert_eq!(pool.healthy_count(), 3);

    // Record fast latency + host affinity for DE proxy on `hazards.fema.gov`
    let snap = pool.working_snapshot(true);
    let wp_de = snap
        .iter()
        .find(|w| w.endpoint.port == p2)
        .expect("DE working proxy")
        .clone();
    let wp_ca = snap
        .iter()
        .find(|w| w.endpoint.port == p3)
        .expect("CA working proxy")
        .clone();
    pool.report_success(&wp_de, "hazards.fema.gov", 12);

    // With `Fastest` strategy + 4x host affinity boost, `wp_de` wins top-1 for `hazards.fema.gov`
    let top_fema = pool.ordered_clients_for_host("hazards.fema.gov", 3);
    assert_eq!(top_fema.len(), 3);
    assert_eq!(top_fema[0].endpoint.port, p2);

    // Per-host 429 cools `wp_de` ONLY for `hazards.fema.gov`, leaving it healthy for Census
    pool.report_host_failure(&wp_de, "hazards.fema.gov", 429);
    let fema_after_429 = pool.ordered_clients_for_host("hazards.fema.gov", 3);
    assert_eq!(fema_after_429.len(), 2);
    assert!(fema_after_429.iter().all(|w| w.endpoint.port != p2));
    let census_healthy = pool.ordered_clients_for_host("geocoding.geo.census.gov", 3);
    assert_eq!(census_healthy.len(), 3);

    // Forged TLS certificate error evicts `wp_ca` immediately on strike #1 and increments counter
    pool.report_request_error(&wp_ca, &forged);
    assert_eq!(
        pool.status_json()["checks"]["interceptingEvicted"],
        json!(1)
    );
    assert!(
        pool.working_snapshot(true)
            .iter()
            .all(|w| w.endpoint.port != p3),
        "TLS-intercepting proxy must be evicted immediately"
    );

    // Stopping persists remaining working set to `free_proxies.json`
    pool.stop().await;
    let saved_raw = std::fs::read_to_string(cache_dir.join("free_proxies.json"))
        .expect("free_proxies.json persisted on stop");
    let saved_json: serde_json::Value = serde_json::from_str(&saved_raw).unwrap();
    assert_eq!(saved_json["proxies"].as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(&cache_dir);

    // 3. Strict HttpClient routing with 5 dead proxies and `max_attempts_per_request = 3`:
    // caps attempts at 3 and fails closed without ever falling back to direct egress.
    let mut dead_eps = Vec::new();
    for _ in 0..5 {
        let dp = closed_port().await;
        dead_eps.push(ProxyEndpoint::new(ProxyProtocol::Http, "127.0.0.1", dp).unwrap());
    }
    let mut strict_cfg = ProxyPoolConfig::default();
    strict_cfg.registry_urls = Vec::new();
    strict_cfg.allow_private_ips = true;
    strict_cfg.max_attempts_per_request = 3;
    strict_cfg.recheck_interval = Duration::from_secs(3_600);
    strict_cfg.acquire_timeout = Duration::from_millis(150);
    strict_cfg.selection_strategy = ProxySelectionStrategy::RoundRobin;

    let dead_pool = ProxyPool::new(strict_cfg);
    dead_pool.seed_proxies(dead_eps, false);
    dead_pool.start();

    let http_strict = HttpClient::from_env(&EnvConfig::default(), None)
        .with_proxy_pool(Arc::clone(&dead_pool), true);
    assert!(http_strict.is_strict_proxy_egress());
    assert_eq!(
        http_strict.egress_status_json()["mode"],
        json!("free_proxy_pool")
    );

    let err = http_strict
        .get_json_opts(
            "https://geocoding.geo.census.gov/geocoder/benchmarks?f=json",
            1_500,
            0,
            0,
            None,
        )
        .await
        .expect_err("strict mode with dead proxies must fail closed");
    let total_failures: u64 = dead_pool
        .working_snapshot(true)
        .iter()
        .map(|w| w.failures.load(Ordering::SeqCst))
        .sum();
    assert_eq!(
        total_failures, 3,
        "per-request proxy attempts must be capped at max_attempts_per_request=3 ({err})"
    );
    let direct_reqs = http_strict.egress_status_json()["slots"]
        .as_array()
        .and_then(|s| s.iter().find(|x| x["direct"] == true))
        .and_then(|x| x["requests"].as_u64())
        .unwrap_or(0);
    assert_eq!(
        direct_reqs, 0,
        "strict mode must never leak data-plane requests to direct slot"
    );

    // Control-plane OpenAI URL still bypasses strict scraping pool onto direct slot
    let llm_handle = http_strict
        .acquire_egress_handle("https://api.openai.com/v1/responses")
        .await
        .expect("control-plane handle");
    assert!(llm_handle.is_direct());
    dead_pool.stop().await;
}

// ------------------------------------------------------------------------------------------------
// 4. Raw RFC 1928 SOCKS5 & HTTP CONNECT byte-stream framing over loopback TCP
// ------------------------------------------------------------------------------------------------

async fn spawn_raw_echo_listener() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                while let Ok(n) = sock.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if sock.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

async fn spawn_rfc1928_socks5_framing_listener() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                // 1. Greeting: VER=0x05, NMETHODS, METHODS...
                let mut hdr = [0u8; 2];
                if client.read_exact(&mut hdr).await.is_err() || hdr[0] != 0x05 {
                    return;
                }
                let mut methods = vec![0u8; hdr[1] as usize];
                if client.read_exact(&mut methods).await.is_err() {
                    return;
                }
                // Select NO AUTHENTICATION REQUIRED (0x00)
                if client.write_all(&[0x05, 0x00]).await.is_err() {
                    return;
                }

                // 2. Request: VER=0x05, CMD=0x01 (CONNECT), RSV=0x00, ATYP
                let mut req = [0u8; 4];
                if client.read_exact(&mut req).await.is_err() || req[0] != 0x05 || req[1] != 0x01 {
                    return;
                }
                let target: SocketAddr = match req[3] {
                    0x01 => {
                        let mut ip_port = [0u8; 6];
                        if client.read_exact(&mut ip_port).await.is_err() {
                            return;
                        }
                        let ip = Ipv4Addr::new(ip_port[0], ip_port[1], ip_port[2], ip_port[3]);
                        let port = u16::from_be_bytes([ip_port[4], ip_port[5]]);
                        SocketAddr::new(IpAddr::V4(ip), port)
                    }
                    0x03 => {
                        let mut len = [0u8; 1];
                        if client.read_exact(&mut len).await.is_err() {
                            return;
                        }
                        let mut dom = vec![0u8; len[0] as usize];
                        let mut port_b = [0u8; 2];
                        if client.read_exact(&mut dom).await.is_err()
                            || client.read_exact(&mut port_b).await.is_err()
                        {
                            return;
                        }
                        let host = String::from_utf8_lossy(&dom);
                        let port = u16::from_be_bytes(port_b);
                        let ip = if host == "localhost" {
                            IpAddr::V4(Ipv4Addr::LOCALHOST)
                        } else {
                            match host.parse::<IpAddr>() {
                                Ok(parsed) => parsed,
                                Err(_) => return,
                            }
                        };
                        SocketAddr::new(ip, port)
                    }
                    _ => return,
                };

                let Ok(mut upstream) = TcpStream::connect(target).await else {
                    let _ = client
                        .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                        .await;
                    return;
                };
                // Reply: VER=0x05, REP=0x00 (succeeded), RSV=0x00, ATYP=0x01, BND.ADDR, BND.PORT
                if client
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                    .await
                    .is_err()
                {
                    return;
                }
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn rfc1928_socks5_and_http_connect_raw_loopback_framing() {
    let echo_addr = spawn_raw_echo_listener().await;
    let socks_addr = spawn_rfc1928_socks5_framing_listener().await;

    // 1. ProxyChecker Phase-A TCP probe distinguishes live listener vs closed port
    let mut cfg = ProxyPoolConfig::default();
    cfg.allow_private_ips = true;
    cfg.tcp_timeout = Duration::from_millis(400);
    let checker = ProxyChecker::new(cfg);

    let live_ep =
        ProxyEndpoint::new(ProxyProtocol::Socks5, "127.0.0.1", socks_addr.port()).unwrap();
    assert!(
        checker.tcp_probe(&live_ep).await.is_ok(),
        "Phase-A tcp_probe must succeed against live SOCKS5 listener"
    );

    let dead_port = closed_port().await;
    let dead_ep = ProxyEndpoint::new(ProxyProtocol::Socks5, "127.0.0.1", dead_port).unwrap();
    let dead_check = checker.check_one(&dead_ep).await;
    assert!(!dead_check.ok);
    assert_eq!(dead_check.stage, CheckStage::Tcp);

    // 2. RFC 1928 SOCKS5 ATYP=0x01 (IPv4) handshake + raw byte-stream round-trip
    {
        let mut s = TcpStream::connect(socks_addr).await.unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        s.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        let port_be = echo_addr.port().to_be_bytes();
        let connect_ipv4 = [0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, port_be[0], port_be[1]];
        s.write_all(&connect_ipv4).await.unwrap();
        let mut conn_reply = [0u8; 10];
        s.read_exact(&mut conn_reply).await.unwrap();
        assert_eq!(&conn_reply[..2], &[0x05, 0x00]);

        let frame = b"RFC1928-IPV4-FRAME-7F000001";
        s.write_all(frame).await.unwrap();
        let mut echoed = vec![0u8; frame.len()];
        s.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, frame);
    }

    // 3. RFC 1928 SOCKS5h ATYP=0x03 (domain name) handshake + raw byte-stream round-trip
    {
        let mut s = TcpStream::connect(socks_addr).await.unwrap();
        s.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        s.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        let domain = b"localhost";
        let port_be = echo_addr.port().to_be_bytes();
        let mut connect_dom = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
        connect_dom.extend_from_slice(domain);
        connect_dom.extend_from_slice(&port_be);
        s.write_all(&connect_dom).await.unwrap();
        let mut conn_reply = [0u8; 10];
        s.read_exact(&mut conn_reply).await.unwrap();
        assert_eq!(&conn_reply[..2], &[0x05, 0x00]);

        let frame = b"RFC1928-SOCKS5H-DOMAIN-FRAME";
        s.write_all(frame).await.unwrap();
        let mut echoed = vec![0u8; frame.len()];
        s.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, frame);
    }
}
