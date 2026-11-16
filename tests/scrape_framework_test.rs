//! Integration tests for the unified `open_estate::scrape` framework (`HttpClient::scrape*`)
//! grounded on deterministic egress invariants and live keyless federal open-data endpoints:
//! 1. Proxy-bypass rules (`should_bypass_proxy`, `OUTBOUND_NO_PROXY`, keyed API endpoints, loopback),
//!    proxy credential redaction (`ProxyEndpoint::redacted_url`), and user-safe URL/error scrubbing
//!    (`url_host_label`, `sanitize_user_warning`).
//! 2. Strict fail-closed `ProxyPool` enforcement (`0` direct data-plane leaks when pool is empty),
//!    defense-in-depth credential-header bypass (`Authorization`, `*-Api-Key`), explicit `.direct(true)`
//!    open-data opt-in, `offline_loopback_only` isolation, and `modules::house3d` graceful degradation
//!    to the footprint model on synthetic parcel coordinates.
//! 3. Live keyless federal open-data verification (cleanly skipped only when offline):
//!    - 16-task concurrent singleflight L1 (`moka`) + disk TTL caching & `with_cache_bypass(true)`
//!      against the keyless US Census Geocoder (`geocoding.geo.census.gov`) coordinate geographies endpoint.
//!    - Bounded binary aerial tile download (`.bytes()`, `.direct(true)`, `.max_bytes(cap)`) against
//!      the keyless USGS National Map (`basemap.nationalmap.gov`).

mod common;

use open_estate::modules::house3d::{build_house3d, BuildingIn, Role, SceneInput};
use open_estate::proxy_pool::{
    ProxyEndpoint, ProxyPool, ProxyPoolConfig, ProxyProtocol, ProxySelectionStrategy,
};
use open_estate::scrape::{ScrapeBody, ScrapeMode};
use open_estate::shared::{sanitize_user_warning, url_host_label, EnvConfig, HttpClient};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

fn empty_strict_pool() -> Arc<ProxyPool> {
    let mut cfg = ProxyPoolConfig::default();
    cfg.registry_urls = Vec::new();
    cfg.max_attempts_per_request = 3;
    cfg.recheck_interval = Duration::from_secs(3_600);
    cfg.acquire_timeout = Duration::from_millis(150);
    cfg.selection_strategy = ProxySelectionStrategy::RoundRobin;
    let pool = ProxyPool::new(cfg);
    pool.seed_proxies(Vec::new(), false);
    pool.start();
    pool
}

fn direct_slot_requests(http: &HttpClient) -> u64 {
    let st = http.egress_status_json();
    st["slots"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|s| s["direct"] == true)
                .and_then(|s| s["requests"].as_u64())
        })
        .unwrap_or(0)
}

#[test]
fn proxy_bypass_credential_redaction_and_warning_sanitization_are_deterministic() {
    let env = EnvConfig::from_map(HashMap::from([
        (
            "OPENAI_BASE_URL".to_string(),
            "https://gateway.enterprise.internal/v1".to_string(),
        ),
        (
            "OSRM_BASE_URL".to_string(),
            "http://osrm.routing.svc:5000".to_string(),
        ),
        (
            "OUTBOUND_NO_PROXY".to_string(),
            "partner.data.org, .internal.corp".to_string(),
        ),
    ]));
    let http = HttpClient::from_env(&env, None);

    // Keyed API hosts, configured internal base URLs, and loopback/RFC1918 addresses bypass proxies.
    for host in [
        "api.openai.com",
        "api.gateway.attomdata.com",
        "places.googleapis.com",
        "gateway.enterprise.internal",
        "osrm.routing.svc",
        "partner.data.org",
        "api.internal.corp",
        "localhost",
        "127.0.0.1",
        "::1",
        "10.42.0.5",
        "192.168.1.20",
    ] {
        assert!(
            http.should_bypass_proxy(host),
            "expected `{host}` to bypass third-party proxies"
        );
    }

    // Public keyless scrape & GIS hosts do NOT bypass configured proxies.
    for public_host in [
        "geocoding.geo.census.gov",
        "basemap.nationalmap.gov",
        "overpass-api.de",
        "hazards.fema.gov",
    ] {
        assert!(
            !http.should_bypass_proxy(public_host),
            "expected public host `{public_host}` to use scrape egress policy"
        );
    }

    // ProxyEndpoint credential redaction never leaks passwords into logs or status JSON.
    let ep = ProxyEndpoint::parse(
        "http://scraper_user:s3cr3tPass!@198.51.100.42:8080",
        ProxyProtocol::Http,
        "operator-seed",
    )
    .expect("parse proxy endpoint");
    assert_eq!(ep.key(), "http://198.51.100.42:8080");
    assert_eq!(
        ep.redacted_url(),
        "http://scraper_user:***@198.51.100.42:8080"
    );
    assert!(!ep.redacted_url().contains("s3cr3tPass"));

    // User-facing URL & error sanitization strips paths, query params, coordinates, and API keys.
    let leaky_url =
        "https://api.gateway.attomdata.com/propertyapi/v1.0.0/property/address?address=200+Synthparcelbeta+Ave&apiKey=sk-live-secret-999";
    assert_eq!(url_host_label(leaky_url), "api.gateway.attomdata.com");
    let warning = sanitize_user_warning(&format!(
        "error sending request for url ({leaky_url}): upstream timeout at {leaky_url}."
    ));
    assert!(!warning.contains("sk-live-secret-999"));
    assert!(!warning.contains("200+Synthparcelbeta+Ave"));
    assert!(warning.contains("api.gateway.attomdata.com"));

    // ScrapeBody & cache-bypass / Overpass endpoint configuration sanity.
    assert!(ScrapeBody::Empty.is_empty());
    let bypassed = http.with_cache_bypass(true);
    assert!(bypassed.is_cache_bypassed());
    assert!(!http.is_cache_bypassed());
    let custom_op = http.with_overpass_endpoints(vec![
        "https://overpass-api.de/api/interpreter".to_string(),
        "not-a-valid-url".to_string(),
    ]);
    assert_eq!(
        custom_op.overpass_endpoints(),
        &["https://overpass-api.de/api/interpreter".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_empty_pool_fails_closed_guards_credentials_and_degrades_house3d() {
    let empty_pool = empty_strict_pool();

    // `bypass_loopback = false` so closed port 9 (`127.0.0.1:9`) exercises strict egress vs
    // credential/direct bypass deterministically without spawning any local server.
    let strict_http = HttpClient::with_egress_config(None, 16, 256, "", Vec::new(), false)
        .with_proxy_pool(Arc::clone(&empty_pool), true);
    assert!(strict_http.is_strict_proxy_egress());
    assert!(strict_http.proxy_pool().is_some());
    assert_eq!(direct_slot_requests(&strict_http), 0);

    // 1. Uncredentialed data-plane GET and POST fail closed BEFORE touching the direct slot.
    let get_err = strict_http
        .scrape_get("https://geocoding.geo.census.gov/geocoder/geographies/coordinates")
        .mode(ScrapeMode::Standard)
        .timeout_ms(500)
        .bytes()
        .await;
    assert!(
        get_err.is_err(),
        "strict data-plane GET must fail closed when ProxyPool has no healthy proxy"
    );
    let polite_post_err = strict_http
        .scrape_post("https://overpass-api.de/api/interpreter")
        .polite()
        .form(&[("data", "[out:json];node(1);out;")])
        .timeout_ms(500)
        .send_status_text()
        .await;
    assert!(
        polite_post_err.is_err(),
        "strict Polite POST must also fail closed when ProxyPool is empty"
    );
    assert_eq!(
        direct_slot_requests(&strict_http),
        0,
        "strict data-plane requests leaked 0 calls to direct slot"
    );

    // 2. Defense-in-depth credential guard: requests carrying `Authorization` or `*-Api-Key`
    //    bypass public proxies and route via the `direct` slot even in strict mode (targeting
    //    closed RFC 863 port 9 so no listener is needed and TCP refuses in <5ms).
    let _ = strict_http
        .scrape_post("http://127.0.0.1:9/v1/responses")
        .header("authorization", "Bearer sk-ephemeral-guard-check")
        .json_value_body(&serde_json::json!({ "ping": true }))
        .timeout_ms(300)
        .bytes()
        .await;
    assert_eq!(
        direct_slot_requests(&strict_http),
        1,
        "Authorization header routed straight to direct slot instead of proxy pool"
    );

    let _ = strict_http
        .scrape_get("http://127.0.0.1:9/v1/property")
        .header("x-vendor-api-key", "ephemeral-vendor-key")
        .retries(0)
        .timeout_ms(300)
        .bytes()
        .await;
    assert_eq!(
        direct_slot_requests(&strict_http),
        2,
        "*-api-key header routed straight to direct slot instead of proxy pool"
    );

    // 3. Explicit `.direct(true)` opt-in for keyless public-domain open data routes via direct slot,
    //    while `.direct(false)` still fails closed without touching direct slot.
    let _ = strict_http
        .scrape_get("http://127.0.0.1:9/open-data.laz")
        .direct(true)
        .retries(0)
        .timeout_ms(300)
        .max_bytes(4096)
        .allow_statuses(&[404, 204])
        .bytes()
        .await;
    assert_eq!(direct_slot_requests(&strict_http), 3);

    let closed_again = strict_http
        .scrape_get("http://127.0.0.1:9/open-data.laz")
        .direct(false)
        .timeout_ms(300)
        .bytes()
        .await;
    assert!(closed_again.is_err());
    assert_eq!(direct_slot_requests(&strict_http), 3);

    // 4. `build_house3d` in strict mode with an empty pool on synthetic parcel coordinates
    //    fails closed (`0` additional direct requests) and degrades cleanly to the footprint model.
    let sec = common::synth_report_secondary();
    let strict_env = EnvConfig::from_map(HashMap::from([(
        "PROXY_POOL_STRICT".to_string(),
        "true".to_string(),
    )]));
    let scene_out = build_house3d(
        &strict_http,
        &strict_env,
        None,
        SceneInput {
            lat: sec["geo"]["lat"].as_f64().unwrap_or(37.2820),
            lon: sec["geo"]["lon"].as_f64().unwrap_or(-121.9540),
            buildings: vec![BuildingIn {
                role: Role::Subject,
                ring: vec![(-6.5, -5.2), (6.5, -5.2), (6.5, 5.2), (-6.5, 5.2)],
                label: Some(common::SYNTH_SECONDARY_ADDRESS.to_string()),
                mapped_height_m: None,
                levels: Some(1.0),
            }],
            ..Default::default()
        },
    )
    .await
    .expect("house3d degrades gracefully to footprint when strict egress has no healthy proxy");
    assert!(!scene_out.settled);
    assert_eq!(scene_out.scene["quality"], "footprint");
    assert_eq!(
        direct_slot_requests(&strict_http),
        3,
        "build_house3d under PROXY_POOL_STRICT=true made 0 direct requests"
    );

    // 5. `offline_loopback_only` blocks external non-loopback scrape requests immediately.
    let offline_http = HttpClient::offline_loopback_only(None);
    let ext_err = offline_http
        .scrape_get("https://geocoding.geo.census.gov/geocoder/geographies/coordinates")
        .timeout_ms(500)
        .text()
        .await
        .unwrap_err();
    assert!(
        ext_err.to_string().contains("offline test mode"),
        "unexpected offline error: {ext_err}"
    );

    empty_pool.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_keyless_census_singleflight_cache_and_usgs_tile_caps() {
    let cache_dir =
        std::env::temp_dir().join(format!("openestate-scrape-live-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cache_dir).unwrap();
    let http = HttpClient::with_limits(Some(cache_dir.clone()), 16, 1_024);

    // 1. Fire 16 simultaneous identical GET scrapes against the keyless US Census Geocoder
    //    coordinate geographies endpoint to verify singleflight L1 + disk TTL coalescing.
    let census_url = "https://geocoding.geo.census.gov/geocoder/geographies/coordinates";
    let census_query = [
        ("x", "-121.9540"),
        ("y", "37.2820"),
        ("benchmark", "Public_AR_Current"),
        ("vintage", "Current_Current"),
        ("format", "json"),
    ];
    let barrier = Arc::new(Barrier::new(16));
    let mut handles = Vec::with_capacity(16);
    for _ in 0..16 {
        let h = http.clone();
        let b = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            b.wait().await;
            h.scrape_get(census_url)
                .query(&census_query)
                .timeout_ms(10_000)
                .cache_ttl_secs(600)
                .json_value()
                .await
        }));
    }

    let mut results = Vec::with_capacity(16);
    for task in handles {
        results.push(task.await.expect("join census task"));
    }

    let first = match &results[0] {
        Ok(v) => v.clone(),
        Err(e) => {
            eprintln!("skipping live keyless Census/USGS scrape checks (network unreachable): {e}");
            let _ = std::fs::remove_dir_all(&cache_dir);
            return;
        }
    };

    for res in &results {
        let v: &Value = res.as_ref().expect("all 16 coalesced Census tasks succeed");
        let state_geoid = v
            .pointer("/result/geographies/States/0/GEOID")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(state_geoid, "06", "unexpected Census coordinate state GEOID: {v}");
    }
    assert_eq!(
        direct_slot_requests(&http),
        1,
        "16 concurrent cacheable Census scrapes must coalesce into 1 upstream HTTP request"
    );

    // Verify disk cache persisted at least one response file.
    let disk_entries = std::fs::read_dir(&cache_dir)
        .expect("read cache_dir")
        .count();
    assert!(
        disk_entries >= 1,
        "expected disk cache file in {}",
        cache_dir.display()
    );

    // Subsequent normal call hits L1/disk cache (`direct` requests remain 1), whereas
    // `with_cache_bypass(true)` performs a fresh upstream request (`direct` requests -> 2).
    let cached_again = http
        .scrape_get(census_url)
        .query(&census_query)
        .timeout_ms(10_000)
        .cache_ttl_secs(600)
        .json_value()
        .await
        .expect("cached Census read");
    assert_eq!(cached_again, first);
    assert_eq!(direct_slot_requests(&http), 1);

    let bypassed_val = http
        .with_cache_bypass(true)
        .scrape_get(census_url)
        .query(&census_query)
        .timeout_ms(10_000)
        .cache_ttl_secs(600)
        .json_value()
        .await
        .expect("cache-bypassed Census read");
    assert!(bypassed_val.pointer("/result/geographies").is_some());
    assert_eq!(
        direct_slot_requests(&http),
        2,
        "with_cache_bypass(true) must perform a fresh upstream fetch"
    );

    // 2. Bounded binary download & `.max_bytes(cap)` enforcement against keyless USGS National Map
    //    aerial imagery tile (`z=15, y=12714, x=5282`), routed via `.direct(true)` through a strict
    //    empty pool to verify open-data direct bypass end-to-end.
    let empty_pool = empty_strict_pool();
    let strict_http = HttpClient::with_egress_config(None, 8, 128, "", Vec::new(), true)
        .with_proxy_pool(Arc::clone(&empty_pool), true);
    let usgs_tile_url =
        "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/15/12714/5282";

    match strict_http
        .scrape_get(usgs_tile_url)
        .direct(true)
        .timeout_ms(10_000)
        .max_bytes(512 * 1024)
        .bytes()
        .await
    {
        Ok(tile) => {
            assert!(
                tile.len() > 512,
                "expected non-trivial imagery tile, got {} bytes",
                tile.len()
            );
            let is_jpeg = tile.starts_with(&[0xFF, 0xD8]);
            let is_png = tile.starts_with(b"\x89PNG");
            assert!(
                is_jpeg || is_png,
                "expected JPEG or PNG header from USGS National Map tile, got {:?}",
                &tile[..tile.len().min(8)]
            );

            // Tight `.max_bytes(64)` cap must reject the oversized binary tile cleanly.
            let cap_err = strict_http
                .scrape_get(usgs_tile_url)
                .direct(true)
                .timeout_ms(10_000)
                .max_bytes(64)
                .bytes()
                .await
                .unwrap_err();
            assert!(
                cap_err.to_string().contains("larger than 64 bytes"),
                "unexpected max_bytes error: {cap_err}"
            );
        }
        Err(e) => {
            eprintln!("skipping live USGS National Map binary tile check (unreachable): {e}");
        }
    }

    empty_pool.stop().await;
    let _ = std::fs::remove_dir_all(&cache_dir);
}
