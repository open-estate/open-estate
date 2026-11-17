// End-to-end tests for OSRM outage fallback cooldown, `cached_reports.osrm_degraded` migration &
// tracking, targeted `recompute_report_access`, `RoutingClient::probe_regions`, background recovery
// watcher, `POST /api/admin/reports/refresh-osrm`, and `/api/ops/telemetry` OSRM reporting.

mod common;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, Method, Request, StatusCode},
    routing::get,
    Json, Router,
};
use open_estate::db::{report_has_osrm_router_down, DbBackend, DbStore};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::modules::access::run_access_with_env;
use open_estate::modules::commute::{
    is_router_in_cooldown, RoutingClient, RoutingConfig, ENV_OSRM_FAILURE_COOLDOWN_SECS,
    ENV_OSRM_REGIONS_JSON, ENV_OSRM_TIMEOUT_MS, OSRM_ATTRIBUTION,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::orchestrator::{recompute_report_access, RecomputeAccessStatus};
use open_estate::web::render::render_report_html_with_brand;
use open_estate::web::server::{build_router, spawn_startup_background_tasks, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
struct MockOsrmState {
    up: Arc<AtomicBool>,
    table_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    table_calls: Arc<AtomicUsize>,
    probe_calls: Arc<AtomicUsize>,
    version: String,
}

impl MockOsrmState {
    fn new(initially_up: bool, version: &str) -> Self {
        Self {
            up: Arc::new(AtomicBool::new(initially_up)),
            table_delay_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            table_calls: Arc::new(AtomicUsize::new(0)),
            probe_calls: Arc::new(AtomicUsize::new(0)),
            version: version.to_string(),
        }
    }
}

async fn spawn_mock_osrm(state: MockOsrmState) -> String {
    let app = Router::new()
        .route(
            "/nearest/v1/driving/:coord",
            get(|State(s): State<MockOsrmState>| async move {
                s.probe_calls.fetch_add(1, Ordering::SeqCst);
                if s.up.load(Ordering::SeqCst) {
                    (
                        StatusCode::OK,
                        Json(json!({
                            "code": "Ok",
                            "waypoints": [{ "location": [-121.89, 37.25], "distance": 4.0 }]
                        })),
                    )
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({ "code": "Error", "message": "osrm down" })),
                    )
                }
            }),
        )
        .route(
            "/table/v1/driving/:coords",
            get(
                |State(s): State<MockOsrmState>,
                 axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| async move {
                    s.table_calls.fetch_add(1, Ordering::SeqCst);
                    let delay = s.table_delay_ms.load(Ordering::SeqCst);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    if !s.up.load(Ordering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({ "code": "Error", "message": "osrm down" })),
                        );
                    }
                    let n_src = q
                        .get("sources")
                        .map(|v| v.split(';').filter(|x| !x.is_empty()).count())
                        .unwrap_or(1);
                    let n_dst = q
                        .get("destinations")
                        .map(|v| v.split(';').filter(|x| !x.is_empty()).count())
                        .unwrap_or(1);
                    let durations: Vec<Vec<f64>> = (0..n_src)
                        .map(|_| (0..n_dst).map(|j| 180.0 + (j as f64) * 45.0).collect())
                        .collect();
                    let distances: Vec<Vec<f64>> = (0..n_src)
                        .map(|_| (0..n_dst).map(|j| 2400.0 + (j as f64) * 500.0).collect())
                        .collect();
                    let sources: Vec<Value> =
                        (0..n_src).map(|_| json!({ "distance": 5.0 })).collect();
                    let destinations: Vec<Value> =
                        (0..n_dst).map(|_| json!({ "distance": 8.0 })).collect();
                    (
                        StatusCode::OK,
                        Json(json!({
                            "code": "Ok",
                            "data_version": s.version,
                            "durations": durations,
                            "distances": distances,
                            "sources": sources,
                            "destinations": destinations,
                        })),
                    )
                },
            ),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn load_primary_fixture() -> Value {
    let mut rep = common::synth_report_primary();
    rep["modules"]["amenities"]["data"]["supermarkets"] = json!([
        { "name": "Synth Fresh Market", "lat": 37.261, "lon": -121.896, "distMi": 0.30 }
    ]);
    if rep["ai"].is_null() {
        rep["ai"] = json!({
            "verdict": "Synthetic balanced parcel verdict.",
            "model": "synth-model"
        });
    }
    rep
}

async fn make_test_state(extra: Vec<(&str, String)>) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("oe-osrm-rec-{}", Uuid::new_v4()));
    let reports_dir = dir.join("reports");
    std::fs::create_dir_all(&reports_dir).unwrap();
    let db_path = dir.join("test.sqlite");
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
        map.insert(k.to_string(), v);
    }
    let env = EnvConfig::from_map(map);
    // Offline loopback-only client guarantees zero calls to Overpass / Nominatim / LLMs!
    let http = HttpClient::offline_loopback_only(None);
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(env, http, db, shield, reports_dir, None);
    (state, dir)
}

#[tokio::test]
async fn schema_migration_adds_osrm_degraded_idempotently_and_finds_legacy_rows() {
    let dir = std::env::temp_dir().join(format!("oe-osrm-mig-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("v3_osrm.sqlite");

    let env = EnvConfig::from_map(HashMap::from([(
        "SQLITE_DB_PATH".to_string(),
        db_path.to_string_lossy().into_owned(),
    )]));
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    // Second call must be completely idempotent on the clean-slate v3 44-table schema.
    db.ensure_schema().await.unwrap();

    let router_down_rep = json!({
        "meta": { "slug": "legacy-down", "displayAddress": "100 Synthdown St" },
        "geo": { "zip": "95118", "lat": 37.245, "lon": -121.885 },
        "modules": {
            "access": {
                "data": {
                    "destinations": [{ "key": "downtown", "estimated": true, "estimateReason": "router_down" }],
                    "note": "Estimated: the routing server did not respond."
                },
                "warnings": []
            }
        }
    });
    let outside_cov_rep = json!({
        "meta": { "slug": "legacy-outside", "displayAddress": "200 Synthfar St" },
        "geo": { "zip": "95118", "lat": 37.246, "lon": -121.886 },
        "modules": {
            "access": {
                "data": {
                    "destinations": [{ "key": "downtown", "estimated": true, "estimateReason": "outside_coverage" }],
                    "note": "Estimated: outside the routing map coverage."
                },
                "warnings": []
            }
        }
    });
    let unset_rep = json!({
        "meta": { "slug": "legacy-unset", "displayAddress": "300 Synthunset St" },
        "geo": { "zip": "95118", "lat": 37.247, "lon": -121.887 },
        "modules": {
            "access": {
                "data": {
                    "destinations": [{ "key": "downtown", "estimated": true, "estimateReason": "router_unset" }],
                    "note": "Estimated: no routing server is configured."
                },
                "warnings": []
            }
        }
    });

    for (slug, rep) in [
        ("legacy-down", &router_down_rep),
        ("legacy-outside", &outside_cov_rep),
        ("legacy-unset", &unset_rep),
    ] {
        db.put_cached_report(slug, rep, None).await.unwrap();
    }

    // Verify clean-slate v3 `cached_reports` + `cached_report_sections` persistence and narrow header scan.
    let headers = db.list_osrm_degraded_report_headers(50).await.unwrap();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].slug, "legacy-down");
    assert!(headers[0].osrm_degraded);
    assert_eq!(headers[0].zip, "95118");

    let access_sec = db
        .get_report_section(
            "legacy-down",
            open_estate::db::proto::ReportSectionKind::ReportSectionAccess,
        )
        .await
        .unwrap()
        .expect("cached_report_sections access section");
    let access_val: Value =
        serde_json::from_slice(&access_sec.section_msgpack_or_utf8).expect("access section json");
    assert_eq!(
        access_val.pointer("/modules/access/data/destinations/0/estimateReason"),
        Some(&json!("router_down"))
    );

    let degraded = db.list_osrm_degraded_reports(50).await.unwrap();
    assert_eq!(degraded.len(), 1);
    assert_eq!(degraded[0].slug, "legacy-down");
    assert!(degraded[0].osrm_degraded);
    assert_eq!(db.count_osrm_degraded_reports().await.unwrap(), 1);

    // Verify `put_cached_report` distinguishes `router_down` from `outside_coverage`/`router_unset`.
    let fresh_outside = json!({
        "meta": { "slug": "fresh-outside", "displayAddress": "400 Synthisland Rd" },
        "geo": { "zip": "95118" },
        "modules": {
            "access": {
                "data": {
                    "destinations": [{ "key": "downtown", "estimated": true, "estimateReason": "outside_coverage" }]
                },
                "warnings": []
            }
        }
    });
    db.put_cached_report("fresh-outside", &fresh_outside, None)
        .await
        .unwrap();
    assert!(!db.get_cached_report("fresh-outside").await.unwrap().unwrap().osrm_degraded);
    assert_eq!(db.list_osrm_degraded_report_headers(50).await.unwrap().len(), 1);
    assert_eq!(db.count_osrm_degraded_reports().await.unwrap(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn osrm_down_fallback_cooldown_and_recompute_recovery_end_to_end() {
    let mock_ca = MockOsrmState::new(false, "ca-2026-11-15");
    // Slow outage simulation (1200ms > 500ms OSRM_TIMEOUT_MS) on the first call,
    // followed by fast-fail cooldown on subsequent calls!
    mock_ca.table_delay_ms.store(1200, Ordering::SeqCst);
    let ca_url = spawn_mock_osrm(mock_ca.clone()).await;

    let regions_json = serde_json::to_string(&json!([
        {
            "name": "california",
            "bbox": [-124.5, 32.5, -114.1, 42.0],
            "baseUrl": ca_url,
        }
    ]))
    .unwrap();

    let (state, dir) = make_test_state(vec![
        (ENV_OSRM_REGIONS_JSON, regions_json),
        (ENV_OSRM_TIMEOUT_MS, "500".to_string()),
        (ENV_OSRM_FAILURE_COOLDOWN_SECS, "30".to_string()),
    ])
    .await;

    let mut rep = load_primary_fixture();
    let slug = common::SYNTH_PRIMARY_SLUG;
    let lat = rep["geo"]["lat"].as_f64().unwrap();
    let lon = rep["geo"]["lon"].as_f64().unwrap();
    let city_hall = json!({
        "lat": 37.33803,
        "lon": -121.88532,
        "name": "San Jose City Hall",
        "tags": { "name": "San Jose City Hall" }
    });

    // 1. OSRM is down (slow timeout): `run_access_with_env` falls back to `router_down` and sets cooldown.
    let down_out = run_access_with_env(
        &state.http,
        Some(&state.env),
        lat,
        lon,
        &rep["geo"],
        Some(&rep["modules"]["amenities"]),
        Some(&rep["modules"]["civic"]),
        Some(&city_hall),
    )
    .await
    .unwrap();
    assert_eq!(mock_ca.table_calls.load(Ordering::SeqCst), 1);
    assert!(is_router_in_cooldown(&ca_url));

    rep["modules"]["access"] = json!({
        "data": down_out.data,
        "warnings": down_out.warnings,
    });
    assert!(report_has_osrm_router_down(&rep));
    assert!(rep["modules"]["access"]["data"]["supermarketDriveTimes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(rep["modules"]["access"]["data"]["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d["estimated"] == true && d["estimateReason"] == "router_down"));

    // Fast-fail second access call during cooldown without waiting for 500ms timeout!
    let t_fast = std::time::Instant::now();
    let fast_out = run_access_with_env(
        &state.http,
        Some(&state.env),
        lat,
        lon,
        &rep["geo"],
        Some(&rep["modules"]["amenities"]),
        Some(&rep["modules"]["civic"]),
        Some(&city_hall),
    )
    .await
    .unwrap();
    assert!(
        t_fast.elapsed() < Duration::from_millis(200),
        "cooldown fast-fail took too long: {:?}",
        t_fast.elapsed()
    );
    assert_eq!(mock_ca.table_calls.load(Ordering::SeqCst), 1);
    assert!(fast_out.data["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d["estimateReason"] == "router_down"));

    // Save degraded report + rendered HTML; also capture original `ai` block.
    let orig_ai = rep["ai"].clone();
    assert!(!orig_ai.is_null(), "fixture should carry an AI block");
    let down_html = render_report_html_with_brand(
        &rep,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    assert!(down_html.contains("router_down"));
    state
        .db
        .put_cached_report(slug, &rep, Some(&down_html))
        .await
        .unwrap();
    assert_eq!(state.db.count_osrm_degraded_reports().await.unwrap(), 1);
    assert!(state.db.get_cached_report(slug).await.unwrap().unwrap().osrm_degraded);

    // `recompute_report_access` while OSRM is still down preserves `osrm_degraded=1`.
    let still_down = recompute_report_access(&state.http, &state.env, &state.db, slug)
        .await
        .unwrap();
    assert_eq!(still_down.status, RecomputeAccessStatus::StillDegraded);
    assert!(still_down.osrm_degraded_after);
    assert_eq!(state.db.count_osrm_degraded_reports().await.unwrap(), 1);

    // 2. OSRM comes back UP -> `probe_regions` clears cooldown -> `recompute_report_access` recovers report!
    mock_ca.table_delay_ms.store(0, Ordering::SeqCst);
    mock_ca.up.store(true, Ordering::SeqCst);

    let routing = RoutingClient::new(state.http.clone(), RoutingConfig::from_env(&state.env));
    let probes = routing.probe_regions().await;
    assert_eq!(probes.len(), 1);
    assert!(probes[0].healthy);
    assert_eq!(probes[0].status_code, Some(200));
    assert!(!is_router_in_cooldown(&ca_url));

    let recovered = recompute_report_access(&state.http, &state.env, &state.db, slug)
        .await
        .unwrap();
    assert_eq!(recovered.status, RecomputeAccessStatus::Updated);
    assert!(recovered.osrm_degraded_before);
    assert!(!recovered.osrm_degraded_after);

    assert_eq!(state.db.count_osrm_degraded_reports().await.unwrap(), 0);
    let rec_after = state.db.get_cached_report(slug).await.unwrap().unwrap();
    assert!(!rec_after.osrm_degraded);
    assert_eq!(rec_after.report["ai"], orig_ai, "existing AI block must be preserved");

    let access_data = &rec_after.report["modules"]["access"]["data"];
    assert_eq!(access_data["dataVersion"], json!("ca-2026-11-15"));
    let dests = access_data["destinations"].as_array().unwrap();
    assert!(!dests.is_empty());
    assert!(
        dests.iter().any(|d| d["key"] == "downtown"),
        "synthesized downtown city hall must be preserved"
    );
    assert!(
        dests
            .iter()
            .all(|d| d["estimated"] == false && d["estimateReason"].is_null()),
        "all destinations should be live-routed: {dests:?}"
    );
    let sm_times = access_data["supermarketDriveTimes"].as_array().unwrap();
    assert!(
        !sm_times.is_empty(),
        "supermarketDriveTimes must be populated after recovery"
    );
    assert!(rec_after.report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == OSRM_ATTRIBUTION));

    assert!(rec_after.rendered_html.is_none());
    let html_after = render_report_html_with_brand(
        &rec_after.report,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    assert!(!html_after.contains("\"router_down\""));
    assert!(html_after.contains("\"estimated\":false"));
    assert!(html_after.contains("supermarketDriveTimes"));

    // Verify 24h refresh cooldown (`report_refresh_log`) was NOT touched!
    if let DbBackend::Sqlite(ref pool) = state.db.backend {
        let log_cnt: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM report_refresh_log")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(log_cnt.0, 0, "recompute_report_access must not touch report_refresh_log");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn admin_refresh_osrm_endpoint_telemetry_and_selective_multi_region_recovery() {
    let mock_ca = MockOsrmState::new(true, "ca-v2");
    let mock_tx = MockOsrmState::new(false, "tx-v2");
    let ca_url = spawn_mock_osrm(mock_ca.clone()).await;
    let tx_url = spawn_mock_osrm(mock_tx.clone()).await;

    let regions_json = serde_json::to_string(&json!([
        {
            "name": "california",
            "bbox": [-124.5, 32.5, -114.1, 42.0],
            "baseUrl": ca_url,
        },
        {
            "name": "texas",
            "bbox": [-106.7, 25.8, -93.5, 36.5],
            "baseUrl": tx_url,
        }
    ]))
    .unwrap();

    let admin_token = "osrm-test-admin-token-2026";
    let (state, dir) = make_test_state(vec![
        (ENV_OSRM_REGIONS_JSON, regions_json),
        ("OPEN_ESTATE_OPS_TOKEN", admin_token.to_string()),
    ])
    .await;

    // Seed 2 degraded reports: CA (healthy region) and Austin TX (unhealthy region).
    let mut rep_ca = load_primary_fixture();
    let ca_slug = common::SYNTH_PRIMARY_SLUG;
    rep_ca["modules"]["access"] = json!({
        "data": {
            "destinations": [{
                "key": "downtown",
                "label": "Downtown (city hall)",
                "name": "San Jose City Hall",
                "lat": 37.33803,
                "lon": -121.88532,
                "driveMin": 15,
                "driveMi": 7.6,
                "estimated": true,
                "estimateReason": "router_down"
            }],
            "supermarketDriveTimes": [],
            "note": "Estimated: the routing server did not respond."
        },
        "warnings": ["OSRM router unavailable: timeout; drive times are labelled estimates"]
    });
    state
        .db
        .put_cached_report(ca_slug, &rep_ca, Some("<html>ca-down</html>"))
        .await
        .unwrap();

    let tx_slug = "100-synthaustin-ave-austin-tx-78701";
    let rep_tx = json!({
        "meta": {
            "slug": tx_slug,
            "displayAddress": "100 Synthaustin Ave, Austin, TX 78701",
            "generatedAt": "2026-11-15T00:00:00Z",
            "aiPending": false
        },
        "geo": {
            "lat": 30.2638,
            "lon": -97.7437,
            "zip": "78701",
            "city": "Austin",
            "state": "TX"
        },
        "facts": { "price": 900000, "beds": 3, "baths": 2.0, "sqft": 1800 },
        "modules": {
            "amenities": { "data": { "supermarkets": [{ "name": "H-E-B", "lat": 30.25, "lon": -97.75 }] } },
            "civic": { "data": {} },
            "access": {
                "data": {
                    "destinations": [{
                        "key": "downtown",
                        "label": "Downtown (city hall)",
                        "name": "Austin City Hall",
                        "lat": 30.2649,
                        "lon": -97.7469,
                        "driveMin": 5,
                        "driveMi": 1.2,
                        "estimated": true,
                        "estimateReason": "router_down"
                    }],
                    "supermarketDriveTimes": [],
                    "note": "Estimated: the routing server did not respond."
                },
                "warnings": []
            }
        }
    });
    state
        .db
        .put_cached_report(tx_slug, &rep_tx, Some("<html>tx-down</html>"))
        .await
        .unwrap();

    let app = build_router(state.clone());

    // 1. Unauthenticated POST /api/admin/reports/refresh-osrm -> 401
    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/reports/refresh-osrm")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // 2. GET /api/ops/telemetry reports OSRM region health and degraded report count
    let tel_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ops/telemetry")
                .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tel_resp.status(), StatusCode::OK);
    let tel: Value =
        serde_json::from_slice(&to_bytes(tel_resp.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(tel["inventory"]["osrmDegradedReports"], json!(2));
    assert_eq!(tel["osrm"]["enabled"], json!(true));
    assert_eq!(tel["osrm"]["healthyRegions"], json!(1));
    assert_eq!(tel["osrm"]["totalRegions"], json!(2));

    // 3. Authenticated POST /api/admin/reports/refresh-osrm recovers CA report and skips TX report
    let refresh_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/reports/refresh-osrm")
                .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refresh_resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(refresh_resp.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["degradedFound"], json!(2));
    assert_eq!(body["updated"], json!(1));
    assert_eq!(body["skippedUnhealthyRegion"], json!(1));
    assert_eq!(body["remainingDegraded"], json!(1));
    assert_eq!(body["updatedSlugs"], json!([ca_slug]));

    assert!(!state.db.get_cached_report(ca_slug).await.unwrap().unwrap().osrm_degraded);
    assert!(state.db.get_cached_report(tx_slug).await.unwrap().unwrap().osrm_degraded);

    // Audit log recorded `reports.refresh_osrm`.
    let logs = state.db.list_admin_audit_logs(10).await.unwrap();
    assert!(logs.iter().any(|l| l.action == "reports.refresh_osrm"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn background_watcher_recovers_degraded_reports_on_down_to_up_in_web_mode() {
    let mock = MockOsrmState::new(false, "ca-bg-v1");
    let base_url = spawn_mock_osrm(mock.clone()).await;
    let regions_json = serde_json::to_string(&json!([
        {
            "name": "california",
            "bbox": [-124.5, 32.5, -114.1, 42.0],
            "baseUrl": base_url,
        }
    ]))
    .unwrap();

    // `RUN_MODE=web` specifically verifies the watcher isn't skipped by the ZIP sync early return.
    let (state, dir) = make_test_state(vec![
        (ENV_OSRM_REGIONS_JSON, regions_json),
        ("RUN_MODE", "web".to_string()),
        ("OSRM_HEALTH_POLL_SECS", "1".to_string()),
    ])
    .await;

    let mut rep = load_primary_fixture();
    let slug = common::SYNTH_PRIMARY_SLUG;
    rep["modules"]["access"] = json!({
        "data": {
            "destinations": [{
                "key": "downtown",
                "label": "Downtown (city hall)",
                "name": "San Jose City Hall",
                "lat": 37.33803,
                "lon": -121.88532,
                "driveMin": 15,
                "driveMi": 7.6,
                "estimated": true,
                "estimateReason": "router_down"
            }],
            "supermarketDriveTimes": [],
            "note": "Estimated: the routing server did not respond."
        },
        "warnings": []
    });
    state
        .db
        .put_cached_report(slug, &rep, Some("<html>bg-down</html>"))
        .await
        .unwrap();
    assert_eq!(state.db.count_osrm_degraded_reports().await.unwrap(), 1);

    spawn_startup_background_tasks(&state, 0);

    // Wait past initial poll while still down -> report remains degraded.
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(state.db.count_osrm_degraded_reports().await.unwrap(), 1);

    // Flip OSRM up -> background watcher detects recovery and clears osrm_degraded!
    mock.up.store(true, Ordering::SeqCst);

    let mut recovered = false;
    for _ in 0..30 {
        if state.db.count_osrm_degraded_reports().await.unwrap() == 0 {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(recovered, "background watcher should recover degraded report once OSRM is up");
    let rec = state.db.get_cached_report(slug).await.unwrap().unwrap();
    assert!(!rec.osrm_degraded);
    assert!(!rec.report["modules"]["access"]["data"]["supermarketDriveTimes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(rec.rendered_html.is_none());
    let runtime_html = render_report_html_with_brand(
        &rec.report,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    assert!(runtime_html.contains("\"estimated\":false"));

    let _ = std::fs::remove_dir_all(&dir);
}
