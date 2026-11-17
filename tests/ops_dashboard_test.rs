mod common;

use axum::{
    body::to_bytes,
    http::{Request, StatusCode},
};
use open_estate::db::{BuildQueueJobInput, DbBackend, DbStore};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::search::{upsert_listings_batch, ListingInput};
use open_estate::shared::{now_iso, now_unix_secs, EnvConfig, HttpClient};
use open_estate::web::ops_dashboard::{
    build_connector_catalog, build_ops_telemetry_snapshot, classify_route_bucket,
    render_ops_dashboard_html, sanitize_telemetry_path, OpsTelemetryRegistry, RouteBucket,
    OPS_RECENT_EVENTS_CAP, OPS_ROUTE_LATENCY_RING_CAP,
};
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

const SECONDARY_SLUG: &str = common::SYNTH_SECONDARY_SLUG;
const PRIMARY_SLUG: &str = common::SYNTH_PRIMARY_SLUG;

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

fn load_holdout_home(id: &str) -> Value {
    common::synth_holdout_by_id(id)
}

fn temp_test_dir(prefix: &str) -> PathBuf {
    let n = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "openestate_ops_test_{prefix}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp test dir");
    p
}

async fn make_test_state(extra: &[(&str, &str)]) -> (AppState, PathBuf) {
    let base = temp_test_dir("state");
    let db_path = base.join("ops_test.sqlite");
    let reports_dir = base.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("create reports dir");

    let mut map = HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            reports_dir.to_string_lossy().into_owned(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
    ]);
    for (k, v) in extra {
        map.insert((*k).to_string(), (*v).to_string());
    }
    let env = EnvConfig::from_map(map);
    let http = HttpClient::offline_loopback_only(None);
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");
    let state = AppState::with_components(
        env,
        http,
        db,
        HttpDefenseShield::default(),
        reports_dir,
        None,
    );
    (state, base)
}

#[test]
fn route_bucket_classification_matches_priority_rules() {
    assert_eq!(
        classify_route_bucket(&format!("/r/{SECONDARY_SLUG}")),
        RouteBucket::ReportPage
    );
    assert_eq!(
        classify_route_bucket(&format!("/api/report/{SECONDARY_SLUG}/slides")),
        RouteBucket::ApiSlides
    );
    assert_eq!(
        classify_route_bucket(&format!("/api/report/{SECONDARY_SLUG}/artifact")),
        RouteBucket::ApiArtifact
    );
    assert_eq!(classify_route_bucket("/api/report"), RouteBucket::ApiReport);
    assert_eq!(
        classify_route_bucket(&format!("/api/report/{SECONDARY_SLUG}")),
        RouteBucket::ApiReport
    );
    assert_eq!(classify_route_bucket("/api/ask"), RouteBucket::ApiAsk);
    assert_eq!(classify_route_bucket("/api/translate"), RouteBucket::ApiAsk);
    assert_eq!(
        classify_route_bucket("/api/search/results"),
        RouteBucket::ApiSearch
    );
    assert_eq!(classify_route_bucket("/deal-room"), RouteBucket::DealRoom);
    assert_eq!(
        classify_route_bucket("/api/deal-room/abc"),
        RouteBucket::DealRoom
    );
    assert_eq!(classify_route_bucket("/ops"), RouteBucket::Ops);
    assert_eq!(
        classify_route_bucket("/api/ops/telemetry"),
        RouteBucket::Ops
    );
    assert_eq!(classify_route_bucket("/healthz"), RouteBucket::HealthProbes);
    assert_eq!(classify_route_bucket("/readyz"), RouteBucket::HealthProbes);
    assert_eq!(
        classify_route_bucket("/api/health"),
        RouteBucket::HealthProbes
    );
    assert_eq!(classify_route_bucket("/static/app.css"), RouteBucket::Other);
}

#[test]
fn path_sanitization_strips_queries_tokens_ips_and_pii() {
    let q_stripped = sanitize_telemetry_path("/api/ops/telemetry?token=secret123&email=a@b.com");
    assert_eq!(q_stripped, "/api/ops/telemetry");

    let share_redacted = sanitize_telemetry_path("/share/tok_abc987654/view");
    assert_eq!(share_redacted, "/share/:token");
    assert!(!share_redacted.contains("tok_abc987654"));

    let keys_redacted = sanitize_telemetry_path("/api/keys/secret_key_id_999");
    assert_eq!(keys_redacted, "/api/keys/:id");

    let ip_redacted = sanitize_telemetry_path("/api/debug/198.51.100.42/ping");
    assert!(!ip_redacted.contains("198.51.100.42"));
    assert!(ip_redacted.contains("[redacted-ip]"));

    let jwt_redacted = sanitize_telemetry_path(
        "/api/report/eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sig",
    );
    assert!(!jwt_redacted.contains("eyJhbGci"));
    assert!(jwt_redacted.contains("[redacted]"));

    let key_redacted = sanitize_telemetry_path("/api/search/dbk_live_999888777");
    assert!(!key_redacted.contains("dbk_live_999888777"));
    assert!(key_redacted.contains("[redacted]"));

    let email_redacted = sanitize_telemetry_path("/api/alerts/buyer@open-estate.local/status");
    assert!(!email_redacted.contains("buyer@open-estate.local"));
    assert!(email_redacted.contains("[redacted]"));

    let report_clean = sanitize_telemetry_path(&format!("/api/report/{SECONDARY_SLUG}/slides"));
    assert_eq!(report_clean, format!("/api/report/{SECONDARY_SLUG}/slides"));
}

#[test]
fn registry_bounds_latency_ring_at_256_and_events_at_30() {
    let reg = OpsTelemetryRegistry::new();
    assert_eq!(reg.snapshot().in_flight_requests, 0);

    // Push 44 large outliers (500ms) first, followed by 256 deterministic samples (1..=256 ms)
    // so rolling ring (N=256) evicts every 500ms outlier from P50/P95 calculations.
    for _ in 0..44 {
        reg.record_request("GET", &format!("/api/report/{PRIMARY_SLUG}"), 200, 500_000);
    }
    for ms in 1_u64..=(OPS_ROUTE_LATENCY_RING_CAP as u64) {
        reg.record_request(
            "GET",
            &format!("/api/report/{SECONDARY_SLUG}?secret=hide_me"),
            200,
            ms * 1_000,
        );
    }
    // Add one error request on /api/ask
    reg.record_request("POST", "/api/ask", 502, 42_000);

    let snap = reg.snapshot();
    assert_eq!(snap.in_flight_requests, 0);
    assert_eq!(snap.total_requests, 44 + 256 + 1);
    assert_eq!(snap.status_counts.c2xx, 300);
    assert_eq!(snap.status_counts.c5xx, 1);

    // Recent events capped strictly at 30 and newest-first, with query strings stripped.
    assert_eq!(snap.recent_events.len(), OPS_RECENT_EVENTS_CAP);
    assert_eq!(snap.recent_events[0].path, "/api/ask");
    assert_eq!(snap.recent_events[0].status, 502);
    assert!(snap
        .recent_events
        .iter()
        .all(|ev| !ev.path.contains("hide_me")));

    // Validate rolling P50/P95 for /api/report came from the last 256 samples (1..=256ms),
    // while sample_count stays bounded at 256 and peak max_ms records the 500ms spike.
    let rep_route = snap
        .routes
        .iter()
        .find(|r| r.route == "/api/report")
        .expect("report route stats");
    assert_eq!(rep_route.requests, 300);
    assert_eq!(rep_route.sample_count, OPS_ROUTE_LATENCY_RING_CAP);
    assert!(
        (rep_route.p50_ms - 128.0).abs() <= 2.0,
        "expected rolling P50 near 128ms, got {}",
        rep_route.p50_ms
    );
    assert!(
        (rep_route.p95_ms - 244.0).abs() <= 4.0,
        "expected rolling P95 near 244ms, got {}",
        rep_route.p95_ms
    );
    assert!(
        (rep_route.max_ms - 500.0).abs() < 0.01,
        "expected peak max 500ms, got {}",
        rep_route.max_ms
    );
}

#[test]
fn connector_catalog_never_exposes_secrets_or_tokens() {
    let secret_token = "tok_ultra_secret_value_9988";
    let secret_attom = "attom_live_key_776655";
    let secret_smtp = "smtp_password_secret_3322";
    let env = EnvConfig::from_map(HashMap::from([
        (
            "RESO_API_URL".to_string(),
            "https://reso.internal.invalid/odata?key=bad".to_string(),
        ),
        ("RESO_BEARER_TOKEN".to_string(), secret_token.to_string()),
        ("ATTOM_API_KEY".to_string(), secret_attom.to_string()),
        ("SMTP_HOST".to_string(), "smtp.example.invalid".to_string()),
        (
            "SMTP_USER".to_string(),
            "mailer@example.invalid".to_string(),
        ),
        ("SMTP_PASS".to_string(), secret_smtp.to_string()),
        ("COMMERCIAL_DATA_MODE".to_string(), "strict".to_string()),
    ]));

    let catalog = build_connector_catalog(&env);
    assert!(catalog.len() >= 15);

    let serialized = serde_json::to_string(&catalog).expect("serialize connectors");
    for forbidden in [
        secret_token,
        secret_attom,
        secret_smtp,
        "reso.internal.invalid",
        "smtp.example.invalid",
        "mailer@example.invalid",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "connector catalog leaked secret: {forbidden}"
        );
    }

    let reso = catalog.iter().find(|c| c["id"] == "reso_idx").unwrap();
    assert_eq!(reso["configured"], json!(true));
    assert_eq!(reso["status"], json!("ready"));

    let zf_mkt = catalog
        .iter()
        .find(|c| c["id"] == concat!("zil", "low_freddie_market"))
        .unwrap();
    assert_eq!(zf_mkt["configured"], json!(false));
    assert_eq!(zf_mkt["status"], json!("clean_room_restricted"));
}

#[tokio::test]
async fn ops_endpoints_return_live_telemetry_and_responsive_html() {
    let (state, temp_dir) = make_test_state(&[
        ("ATTOM_API_KEY", "attom_secret_never_leak_12345"),
        ("SHOVELS_API_KEY", "shv_secret_never_leak_67890"),
    ])
    .await;

    // Seed 2 synthetic cached reports.
    // Populate `ai.verdict` on Secondary from its `summary.verdict` so both `aiReady == true`
    // and `aiReady == false` paths are exercised.
    let mut rep_secondary = common::synth_report_secondary();
    let real_verdict = rep_secondary["summary"]["verdict"]
        .as_str()
        .unwrap_or("Balanced Silicon Valley single-family parcel")
        .to_string();
    rep_secondary["ai"]["verdict"] = json!(real_verdict);

    let rep_primary = common::synth_report_primary();

    state
        .db
        .put_cached_report(SECONDARY_SLUG, &rep_secondary, None)
        .await
        .expect("put secondary report");
    state
        .db
        .put_cached_report(PRIMARY_SLUG, &rep_primary, None)
        .await
        .expect("put primary report");

    // Seed `report_build_queue` statuses (`complete`, `building`, `queued`, `failed`) using synthetic holdout homes
    let sea_adu = load_holdout_home("synth-wa-98103-adu-sfr");
    let bos_condo = load_holdout_home("synth-ma-02139-condo");
    let mia_remod = load_holdout_home("synth-fl-33134-remod-sfr");

    for (slug, addr, status) in [
        (SECONDARY_SLUG, common::SYNTH_SECONDARY_ADDRESS, "complete"),
        (
            "540-synthcohort15-st-seattle-wa-98103",
            sea_adu["address"].as_str().unwrap(),
            "building",
        ),
        (
            "600-synthcohort24-st-cambridge-ma-02139",
            bos_condo["address"].as_str().unwrap(),
            "queued",
        ),
        (
            "620-synthcohort26-blvd-coral-gables-fl-33134",
            mia_remod["address"].as_str().unwrap(),
            "failed",
        ),
    ] {
        state
            .db
            .upsert_build_queue_job(&BuildQueueJobInput {
                slug: slug.to_string(),
                address: addr.to_string(),
                job_id: Some(format!("job_{slug}")),
                status: status.to_string(),
                step: Some("completed".to_string()),
                ai_requested: true,
                increment_attempts: true,
                last_error: None,
            })
            .await
            .expect("upsert queue job");
    }

    // Seed llm_cache (1 active, 1 expired) + watched_zips + subscriptions + user_search_history
    state
        .db
        .put_llm_cache("active_syn_key", &json!({"ok": true}), 3600)
        .await
        .expect("put active llm cache");
    state
        .db
        .put_llm_cache("expired_syn_key", &json!({"ok": true}), -600)
        .await
        .expect("put expired llm cache");

    state
        .db
        .upsert_subscription_normalized(&open_estate::db::SubscriptionRow {
            id: "sub_ops_1".to_string(),
            email: "private-buyer@open-estate.local".to_string(),
            name: Some("Private Buyer".to_string()),
            zips_json: "[\"94303\"]".to_string(),
            cities_json: None,
            requirements_text: "3bd SFH in Palo Alto 94303".to_string(),
            parsed_filters_json: "{}".to_string(),
            notify_on_new: 1,
            notify_on_sold: 0,
            active: 1,
            unsubscribe_token: "unsub_secret_token_never_expose".to_string(),
            last_notified_at: None,
            match_count: 0,
            filter_schema_version: 3,
            plan_tier: None,
            filters_refreshed_at: None,
            frequency: "instant".to_string(),
            digest_pending_json: None,
            created_at: now_iso(),
            updated_at: now_iso(),
        })
        .await
        .expect("upsert v3 subscription");

    if let DbBackend::Sqlite(ref pool) = state.db.backend {
        sqlx::query(
            "INSERT OR REPLACE INTO watched_zips (zip, city, state_code, active, subscriber_count, created_ts) VALUES (?, ?, ?, 1, 2, ?)",
        )
        .bind("94303")
        .bind("Palo Alto")
        .bind("CA")
        .bind(now_unix_secs())
        .execute(pool)
        .await
        .expect("insert watched zip");

        sqlx::query(
            "INSERT INTO user_search_history (actor_key, slug, raw_slug, display_address, updated_ts)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind("actor_room_1")
        .bind(SECONDARY_SLUG)
        .bind(SECONDARY_SLUG)
        .bind(common::SYNTH_SECONDARY_ADDRESS)
        .bind(now_unix_secs())
        .execute(pool)
        .await
        .expect("insert user_search_history");
    }

    // Seed 3 synthetic Palo Alto 94303 homes across for_sale / pending / sold
    let pa_lower = load_holdout_home("synth-ca-94303-lower-sfr");
    let pa_th = load_holdout_home("synth-ca-94303-townhome");
    let pa_upper = load_holdout_home("synth-ca-94303-upper-sfr");

    let seed_listings = vec![
        ListingInput {
            id: Some("528-synthcohort11-dr-palo-alto-ca-94303".to_string()),
            address: pa_lower["address"].as_str().unwrap().to_string(),
            city: Some("Palo Alto".to_string()),
            state: Some("CA".to_string()),
            zip: "94303".to_string(),
            status: Some("for_sale".to_string()),
            price: pa_lower["targetSalePrice"].as_i64(),
            ..Default::default()
        },
        ListingInput {
            id: Some("534-synthcohort13-walk-palo-alto-ca-94303".to_string()),
            address: pa_th["address"].as_str().unwrap().to_string(),
            city: Some("Palo Alto".to_string()),
            state: Some("CA".to_string()),
            zip: "94303".to_string(),
            status: Some("pending".to_string()),
            price: pa_th["targetSalePrice"].as_i64(),
            ..Default::default()
        },
        ListingInput {
            id: Some("530-synthcohort12-rd-palo-alto-ca-94303".to_string()),
            address: pa_upper["address"].as_str().unwrap().to_string(),
            city: Some("Palo Alto".to_string()),
            state: Some("CA".to_string()),
            zip: "94303".to_string(),
            status: Some("sold".to_string()),
            sold_price: pa_upper["targetSalePrice"].as_i64(),
            ..Default::default()
        },
    ];
    upsert_listings_batch(&state.db, None, &state.env, &seed_listings, false)
        .await
        .expect("upsert listings");

    let app = build_router(state.clone());

    let campbell_report_uri = format!("/r/{SECONDARY_SLUG}?token=leaked_query_secret");
    let campbell_report_json_uri = format!("/r/{SECONDARY_SLUG}/report.json");

    // Drive varied traffic through real router middleware (including query secret, 400, & 404)
    for (method, uri, expected) in [
        ("GET", campbell_report_uri.as_str(), StatusCode::OK),
        ("GET", campbell_report_json_uri.as_str(), StatusCode::OK),
        (
            "GET",
            "/api/listings?zip=94303&email=alice@open-estate.local",
            StatusCode::OK,
        ),
        ("POST", "/api/ask", StatusCode::BAD_REQUEST),
        ("GET", "/deal-room", StatusCode::OK),
        ("GET", "/healthz", StatusCode::OK),
        ("GET", "/missing-route-check", StatusCode::NOT_FOUND),
    ] {
        let mut req_builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", "session=cookie_secret_must_never_appear");
        if method == "POST" {
            req_builder = req_builder
                .header("content-type", "application/json")
                .header("sec-fetch-site", "same-origin");
        }
        let req = req_builder.body(axum::body::Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.expect("dispatch route");
        assert_eq!(resp.status(), expected, "unexpected status for {uri}");
    }

    // 1. Validate GET /api/ops/telemetry
    let tel_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ops/telemetry")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .expect("GET /api/ops/telemetry");
    assert_eq!(tel_resp.status(), StatusCode::OK);
    assert_eq!(
        tel_resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store, max-age=0")
    );
    assert_eq!(
        tel_resp
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );

    let tel_bytes = to_bytes(tel_resp.into_body(), 2 * 1024 * 1024)
        .await
        .expect("read telemetry json");
    let tel_str = String::from_utf8(tel_bytes.to_vec()).expect("utf8 json");
    for secret in [
        "rc_secret_never_leak_12345",
        "shv_secret_never_leak_67890",
        "leaked_query_secret",
        "cookie_secret_must_never_appear",
        "alice@open-estate.local",
        "private-buyer@open-estate.local",
        "unsub_secret_token_never_expose",
    ] {
        assert!(
            !tel_str.contains(secret),
            "telemetry JSON leaked sensitive string: {secret}"
        );
    }

    let v: Value = serde_json::from_str(&tel_str).expect("parse telemetry json");
    assert_eq!(v["ok"], json!(true));
    assert!(v["server"]["totalRequests"].as_u64().unwrap_or(0) >= 7);
    assert_eq!(v["database"]["connected"], json!(true));
    assert_eq!(v["database"]["dialect"], json!("sqlite"));
    assert!(v["database"]["totalMb"].as_f64().unwrap_or(0.0) > 0.0);

    assert_eq!(v["inventory"]["reportsTotal"], json!(2));
    assert_eq!(v["inventory"]["reportJobs"]["done"], json!(1));
    assert_eq!(v["inventory"]["reportJobs"]["running"], json!(1));
    assert_eq!(v["inventory"]["reportJobs"]["queued"], json!(1));
    assert_eq!(v["inventory"]["reportJobs"]["error"], json!(1));
    assert_eq!(v["inventory"]["reportJobs"]["total"], json!(4));
    assert_eq!(v["inventory"]["llmCacheEntries"], json!(2));
    assert_eq!(v["inventory"]["llmCacheActiveEntries"], json!(1));
    assert_eq!(v["inventory"]["indexedHomesTotal"], json!(3));
    assert_eq!(v["inventory"]["indexedHomesByStatus"]["forSale"], json!(1));
    assert_eq!(v["inventory"]["indexedHomesByStatus"]["pending"], json!(1));
    assert_eq!(v["inventory"]["indexedHomesByStatus"]["sold"], json!(1));
    assert_eq!(v["inventory"]["watchedZipsActive"], json!(1));
    assert_eq!(v["inventory"]["alertSubscriptionsActive"], json!(1));
    assert_eq!(v["inventory"]["dealRoomsCount"], json!(1));

    let recent = v["inventory"]["recentReports"]
        .as_array()
        .expect("recentReports array");
    assert_eq!(recent.len(), 2);
    let expected_report_url = format!("/r/{SECONDARY_SLUG}");
    let expected_cmd_url = format!("/r/{SECONDARY_SLUG}?view=dashboard");
    assert!(recent.iter().any(|r| r["slug"] == SECONDARY_SLUG
        && r["reportUrl"] == expected_report_url
        && r["commandCenterUrl"] == expected_cmd_url
        && r["aiReady"] == json!(true)));

    if cfg!(target_os = "linux") {
        assert!(
            v["system"]["rssMb"].as_f64().unwrap_or(0.0) > 0.0,
            "Linux /proc/self RSS MB should be positive"
        );
    }
    assert_eq!(v["securityAndEgress"]["guardrailsActive"], json!(true));

    let conns = v["connectors"].as_array().expect("connectors array");
    assert!(conns.len() >= 15);
    assert!(conns
        .iter()
        .any(|c| c["id"] == "attom_data" && c["configured"] == json!(true)));
    assert!(conns
        .iter()
        .any(|c| c["id"] == "shovels_permits" && c["configured"] == json!(true)));

    // 2. Validate GET /ops HTML response
    let page_resp = app
        .oneshot(
            Request::builder()
                .uri("/ops")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .expect("GET /ops");
    assert_eq!(page_resp.status(), StatusCode::OK);
    let ct = page_resp
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/html"), "expected text/html, got {ct}");

    let html_bytes = to_bytes(page_resp.into_body(), 2 * 1024 * 1024)
        .await
        .expect("read /ops html");
    let html = String::from_utf8(html_bytes.to_vec()).expect("utf8 html");
    for required in [
        "Open Report ↗",
        "Buyer Command Center ↗",
        expected_cmd_url.as_str(),
        "← Back to Open Estate",
        "Auto-Refresh (5s)",
        "Refresh Now",
        "U.S. Census Geocoder",
        "County ArcGIS &amp; Assessor Roll",
        "FEMA NFIP Flood &amp; NRI",
        "echarts",
        "data-lang=\"zh\"",
        "data-lang=\"es\"",
        "@media(max-width:680px)",
    ] {
        assert!(
            html.contains(required),
            "/ops HTML missing required control/section: {required}"
        );
    }
    for secret in [
        "rc_secret_never_leak_12345",
        "shv_secret_never_leak_67890",
        "leaked_query_secret",
        "alice@open-estate.local",
        "private-buyer@open-estate.local",
    ] {
        assert!(!html.contains(secret), "/ops HTML leaked secret: {secret}");
    }

    if let Ok(preview_path) = std::env::var("OPS_SAVE_PREVIEW_PATH") {
        let _ = std::fs::write(preview_path, &html);
    }

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn ops_telemetry_handles_missing_tables_gracefully() {
    let (state, temp_dir) = make_test_state(&[]).await;

    // Simulate an older/partial database by dropping several tables cleanly.
    if let DbBackend::Sqlite(ref pool) = state.db.backend {
        for tbl in [
            "report_build_queue",
            "llm_cache",
            "listings",
            "watched_zips",
            "subscriptions",
            "subscription_notifications",
            "client_doc_reviews",
            "user_search_history",
            "report_shares",
            "listing_factors",
            "school_metrics",
            "property_likes",
            "cached_reports",
        ] {
            let sql = format!("DROP TABLE IF EXISTS {tbl}");
            sqlx::query(&sql).execute(pool).await.expect("drop table");
        }
    }

    let snapshot = build_ops_telemetry_snapshot(&state).await;
    assert_eq!(snapshot["ok"], json!(true));
    assert_eq!(snapshot["database"]["connected"], json!(true));
    assert_eq!(snapshot["inventory"]["reportsTotal"], json!(0));
    assert_eq!(snapshot["inventory"]["reportJobs"]["total"], json!(0));
    assert_eq!(snapshot["inventory"]["llmCacheEntries"], json!(0));
    assert_eq!(snapshot["inventory"]["indexedHomesTotal"], json!(0));
    assert_eq!(snapshot["inventory"]["watchedZipsActive"], json!(0));
    assert_eq!(snapshot["inventory"]["dealRoomsCount"], json!(0));
    assert_eq!(
        snapshot["inventory"]["recentReports"]
            .as_array()
            .map(|a| a.len()),
        Some(0)
    );

    let html = render_ops_dashboard_html(&snapshot, "Open Estate");
    assert!(html.contains("No cached reports yet."));

    let _ = std::fs::remove_dir_all(&temp_dir);
}
