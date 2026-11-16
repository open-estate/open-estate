// End-to-End Live Ephemeral Axum Server Tests:
// 1. Browser-Close / Unstable-Network Detached Continuation (`50ms` TCP drop -> background finish -> `HIT`)
// 2. DDoS / Cache-Miss Bombing Shield + Cached & In-Flight Reconnect Exemption + Security Headers
// 3. Direct LLM Route Firewall (`403 DIRECT_LLM_BLOCKED`) & Oversized URI Envelope (`414 URI_TOO_LONG`)

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use open_estate::db::DbStore;
use open_estate::guardrails::{HttpDefenseShield, ShieldLimits, SECURITY_RESPONSE_HEADERS};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

struct TestServerHandle {
    base_url: String,
    host_header: String,
    state: AppState,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServerHandle {
    async fn start(shield: HttpDefenseShield) -> Self {
        let temp_dir = std::env::temp_dir().join(format!("openestate-srv-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");

        let db_path = temp_dir.join("test.sqlite");
        let env = EnvConfig::from_process_and_dotenv(Some(Path::new("."))).with_overrides([
            ("SQLITE_DB_PATH", db_path.to_string_lossy().into_owned()),
            ("SQLITE_PATH", db_path.to_string_lossy().into_owned()),
            ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
            ("SYNC_INTERVAL_MIN", "0".to_string()),
        ]);

        let http = HttpClient::from_env(&env, Some(PathBuf::from(".cache/http")));
        let db = DbStore::connect(&env).await.expect("connect test sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let state = AppState::with_components(
            env,
            http,
            db,
            shield,
            reports_dir,
            Some(PathBuf::from(".cache/datasets")),
        );

        let router = build_router(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");
        let host_header = addr.to_string();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        Self {
            base_url,
            host_header,
            state,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn assert_security_headers(headers: &reqwest::header::HeaderMap) {
    for &(name, expected) in SECURITY_RESPONSE_HEADERS {
        let actual = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_else(|| panic!("missing security header `{name}`"));
        if name == "permissions-policy" {
            assert!(
                actual == expected || actual == "geolocation=(self), microphone=(), camera=()",
                "security header `{name}` mismatch: got `{actual}`, expected `{expected}`"
            );
            continue;
        }
        assert_eq!(
            actual, expected,
            "security header `{name}` mismatch: got `{actual}`, expected `{expected}`"
        );
    }
}

async fn census_geocoder_reachable() -> bool {
    let Ok(client) = Client::builder().timeout(Duration::from_secs(5)).build() else {
        return false;
    };
    client
        .get("https://geocoding.geo.census.gov/geocoder/benchmarks?f=json")
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

#[tokio::test]
async fn test_1_browser_close_tcp_drop_detached_continuation() {
    let srv = TestServerHandle::start(HttpDefenseShield::default()).await;
    // Synthetic Santa Clara County SFR paired with US Census coordinate hint
    let address = "420 Synthcohort03 St, San Jose, CA 95118";
    let expected_slug = "420-synthcohort03-st-san-jose-ca-95118";

    // 1. Simulate browser tab closing / Wi-Fi dropping 50ms into `GET /api/report`!
    let drop_client = Client::builder()
        .timeout(Duration::from_millis(50))
        .no_proxy()
        .build()
        .expect("build 50ms timeout client");

    let report_url = format!(
        "{}/api/report?ai=0&lat=37.2530&lon=-121.8920&address={}",
        srv.base_url,
        urlencoding::encode(address)
    );
    let drop_res = drop_client.get(&report_url).send().await;
    if let Err(ref e) = drop_res {
        assert!(
            e.is_timeout() || e.is_connect() || e.is_request(),
            "expected client timeout drop, got {e:?}"
        );
    }
    drop(drop_client);

    let poll_client = Client::builder()
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .build()
        .expect("build poll client");

    if !census_geocoder_reachable().await {
        // Offline environment: verify server stayed healthy after mid-flight TCP drop and detached task registered
        let hz = poll_client
            .get(format!("{}/healthz", srv.base_url))
            .send()
            .await
            .expect("healthz after tcp drop");
        assert_eq!(hz.status(), reqwest::StatusCode::OK);
        eprintln!(
            "skipping live detached build completion poll in test_1_browser_close_tcp_drop_detached_continuation: external Census geocoder unreachable"
        );
        return;
    }

    // 2. Poll `GET /r/<expected_slug>/report.json` until the detached background
    //    task finishes building and caching the report — WITHOUT re-triggering a new build!
    let rj_url = format!("{}/r/{expected_slug}/report.json", srv.base_url);
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut cached_rj: Option<Value> = None;

    while Instant::now() < deadline {
        let resp = poll_client.get(&rj_url).send().await.expect("poll rj");
        if resp.status() == reqwest::StatusCode::OK {
            assert_eq!(
                resp.headers()
                    .get("x-openestate-cache")
                    .and_then(|v| v.to_str().ok()),
                Some("HIT")
            );
            assert_security_headers(resp.headers());
            cached_rj = Some(resp.json::<Value>().await.expect("parse report.json"));
            break;
        }
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    let rj = cached_rj.expect("detached background build must complete and cache report.json");
    assert_eq!(
        rj.pointer("/meta/slug").and_then(|v| v.as_str()),
        Some(expected_slug)
    );
    assert_eq!(
        rj.pointer("/geo/matched").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        rj.pointer("/modules/valuation/data/estimate/value")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            > 100_000.0
    );

    // 3. Re-query `GET /api/report?ai=0&address=...` and verify immediate `200` + `x-openestate-cache: HIT`!
    let hit_resp = poll_client
        .get(&report_url)
        .send()
        .await
        .expect("re-fetch /api/report");
    assert_eq!(hit_resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        hit_resp
            .headers()
            .get("x-openestate-cache")
            .and_then(|v| v.to_str().ok()),
        Some("HIT")
    );
    assert_security_headers(hit_resp.headers());
    let hit_body: Value = hit_resp.json().await.expect("parse hit json");
    assert_eq!(hit_body.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(hit_body.get("cached").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        hit_body.get("slug").and_then(|v| v.as_str()),
        Some(expected_slug)
    );
}

#[tokio::test]
async fn test_2_ddos_bombing_shield_and_reconnect_exemption() {
    let mut limits = ShieldLimits::default();
    limits.report_miss_per_10m = 3;
    limits.report_miss_subnet_per_10m = 10;
    limits.report_miss_max_concurrent = 8;
    let shield = HttpDefenseShield::with_limits(limits);

    let srv = TestServerHandle::start(shield).await;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .no_proxy()
        .build()
        .expect("build client");

    // Seed procedural synthetic report so we can verify cached reads NEVER consume `report_miss` tokens
    let cached_slug = common::SYNTH_PRIMARY_SLUG;
    let cached_addr = common::SYNTH_PRIMARY_ADDRESS;
    let mut synth_primary = common::synth_report_primary();
    normalize_stored_report(&mut synth_primary);
    let synth_primary_html = render_report_html(&synth_primary);
    srv.state
        .db
        .put_cached_report(cached_slug, &synth_primary, Some(&synth_primary_html))
        .await
        .expect("seed synthetic cached report");

    // 1. Perform 6 cached reads across `/api/report` and `/report` from `203.0.113.55`
    //    -> MUST consume 0 `report_miss` tokens!
    for _ in 0..3 {
        let r1 = client
            .get(format!(
                "{}/api/report?ai=0&address={}",
                srv.base_url,
                urlencoding::encode(cached_addr)
            ))
            .header("X-Forwarded-For", "203.0.113.55")
            .send()
            .await
            .unwrap();
        assert_eq!(r1.status(), reqwest::StatusCode::OK);
        assert_eq!(
            r1.headers()
                .get("x-openestate-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );

        let r2 = client
            .get(format!(
                "{}/report?address={}",
                srv.base_url,
                urlencoding::encode(cached_addr)
            ))
            .header("X-Forwarded-For", "203.0.113.55")
            .send()
            .await
            .unwrap();
        assert_eq!(r2.status(), reqwest::StatusCode::FOUND);
        assert_eq!(
            r2.headers()
                .get("x-openestate-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
    }

    // 2. Enqueue an uncached synthetic property via `POST /api/report/enqueue` (consumes token #1 of 3)
    //    and verify that repeated job-status polling & reconnects NEVER consume extra tokens!
    let inflight_addr = common::SYNTH_SECONDARY_ADDRESS;
    let first_enq = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Host", &srv.host_header)
        .header("Origin", &srv.base_url)
        .header("X-Forwarded-For", "203.0.113.55")
        .json(&json!({ "address": inflight_addr, "ai": false }))
        .send()
        .await
        .expect("enqueue uncached property #1");
    assert_eq!(first_enq.status(), reqwest::StatusCode::ACCEPTED);
    let first_enq_json: Value = first_enq.json().await.expect("enqueue #1 json");
    let job_id = first_enq_json["jobId"]
        .as_str()
        .expect("jobId string")
        .to_string();

    for _ in 0..5 {
        let poll_r = client
            .get(format!("{}/api/job/{job_id}", srv.base_url))
            .header("X-Forwarded-For", "203.0.113.55")
            .send()
            .await
            .unwrap();
        assert_eq!(poll_r.status(), reqwest::StatusCode::OK);
    }

    // 3. Consume remaining 2 `report_miss` tokens (#2 and #3 of 3) with uncached synthetic US addresses
    for uncached_synth_addr in [
        "480 Synthcohort09 St, Menlo Park, CA 94025",
        "540 Synthcohort15 St, Seattle, WA 98103",
    ] {
        let resp = client
            .post(format!("{}/api/report/enqueue", srv.base_url))
            .header("Host", &srv.host_header)
            .header("Origin", &srv.base_url)
            .header("X-Forwarded-For", "203.0.113.55")
            .json(&json!({ "address": uncached_synth_addr, "ai": false }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::ACCEPTED,
            "uncached synthetic address `{uncached_synth_addr}` should consume miss token and return 202"
        );
    }

    // 4. 4th distinct uncached address from `203.0.113.55` MUST return `429` with `Retry-After` + security headers!
    let bomb_4 = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Host", &srv.host_header)
        .header("Origin", &srv.base_url)
        .header("X-Forwarded-For", "203.0.113.55")
        .json(&json!({ "address": "570 Synthcohort18 St, Evanston, IL 60201", "ai": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(bomb_4.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = bomb_4
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .expect("429 response must include numeric Retry-After header");
    assert!(retry_after >= 1);
    assert_security_headers(bomb_4.headers());
    let bomb_json: Value = bomb_4.json().await.unwrap();
    assert_eq!(bomb_json.get("ok").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        bomb_json.get("code").and_then(|v| v.as_str()),
        Some("RATE_LIMIT_EXCEEDED")
    );

    // 5. Even while rate-limited on `report_miss`, cached reads AND job status polling STILL succeed!
    let cached_after_429 = client
        .get(format!(
            "{}/api/report?ai=0&address={}",
            srv.base_url,
            urlencoding::encode(cached_addr)
        ))
        .header("X-Forwarded-For", "203.0.113.55")
        .send()
        .await
        .unwrap();
    assert_eq!(cached_after_429.status(), reqwest::StatusCode::OK);

    let job_poll_after_429 = client
        .get(format!("{}/api/job/{job_id}", srv.base_url))
        .header("X-Forwarded-For", "203.0.113.55")
        .send()
        .await
        .unwrap();
    assert_eq!(job_poll_after_429.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn test_3_direct_llm_route_block_and_oversized_uri_and_oneshot() {
    let srv = TestServerHandle::start(HttpDefenseShield::default()).await;
    let client = Client::builder().no_proxy().build().expect("build client");

    // 1. Direct LLM route firewall (`403 DIRECT_LLM_BLOCKED`)
    for route in [
        "/v1/chat/completions",
        "/v1/responses",
        "/openai/v1/models",
        "/api/llm",
    ] {
        let r = client
            .get(format!("{}{route}", srv.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN);
        assert_security_headers(r.headers());
        let body: Value = r.json().await.unwrap();
        assert_eq!(body.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("DIRECT_LLM_BLOCKED")
        );
    }

    // 2. Oversized URI (> 2048 bytes -> `414 URI_TOO_LONG`)
    let long_q = "a".repeat(2500);
    let r_long = client
        .get(format!("{}/?q={long_q}", srv.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(r_long.status(), reqwest::StatusCode::URI_TOO_LONG);
    assert_security_headers(r_long.headers());
    let long_body: Value = r_long.json().await.unwrap();
    assert_eq!(
        long_body.get("code").and_then(|v| v.as_str()),
        Some("URI_TOO_LONG")
    );

    // 3. Verify `GET /` serves clean consumer UI with zero internal tech badges and prefill-only dropdown!
    let r_home = client
        .get(format!("{}/", srv.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(r_home.status(), reqwest::StatusCode::OK);
    assert_security_headers(r_home.headers());
    let home_html = r_home.text().await.unwrap();
    assert!(home_html.contains("Instant valuation, natural hazard"));
    assert!(home_html.contains("Click or Enter to select"));
    assert!(home_html.contains("US Street Addresses"));
    for forbidden in [
        "Property Guardrails enabled",
        "rust-sqlx-sqlite",
        "SQLx:",
        "SMTP alerts ready",
        "MLS connectors ready",
        "gpt-4.1",
        "gpt-4o-mini",
        "View compiled SQLx query",
        "SMTP_HOST/USER/PASS",
    ] {
        assert!(
            !home_html.contains(forbidden),
            "Home page HTML leaked internal tech detail: {forbidden}"
        );
    }

    // 4. Verify `build_router` also works cleanly with `tower::ServiceExt::oneshot` (when `ConnectInfo` is `None`)!
    let router = build_router(srv.state.clone());
    let oneshot_resp = router
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot /healthz");
    assert_eq!(oneshot_resp.status(), StatusCode::OK);
    assert_eq!(
        oneshot_resp
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
}
