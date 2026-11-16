//! Comprehensive offline mock-HTTP test suite for all 5 routing matrix extensions
//! (`osrm_routing`, `mapbox_routing`, `locationiq_routing`, `freeroute_routing`,
//! `openrouteservice_routing`), regional OSRM cascade, Tier-2 round-robin & priority load
//! balancing, `<=25` coordinate chunking, 2 RPS rate limiting, mid-batch failover, runtime
//! `ExtensionHub` toggling, `/api/ops/telemetry` non-secret metrics, and honest report citations.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use open_estate::db::{report_has_osrm_router_down, DbStore};
use open_estate::extensions::proto::{
    capability_domain_slug, decode_proto_bytes, extension_envelope_proto::Payload,
    parse_capability_domain, validate_proto_message_type, CapabilityDomain,
    ExtensionEnvelopeProto, ExtensionQueryProto,
};
use open_estate::extensions::ExtensionHub;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::modules::access::run_access_with_env_and_hub;
use open_estate::modules::commute::{
    is_cloud_provider_in_cooldown, is_router_in_cooldown, EstimateReason, LatLon, RoutingClient,
    RoutingConfig, RoutingProviderKind, OSRM_ATTRIBUTION,
};
use open_estate::shared::helpers::scrub_internal_tech_words;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::ops_dashboard::build_ops_telemetry_snapshot;
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    query: Option<String>,
    auth_header: Option<String>,
    content_type: Option<String>,
    body_json: Option<Value>,
}

#[derive(Clone)]
struct MockProviderServer {
    _kind: RoutingProviderKind,
    status_code: Arc<AtomicUsize>,
    hits: Arc<AtomicUsize>,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    far_snap_last_dest: bool,
}

impl MockProviderServer {
    fn new(kind: RoutingProviderKind, status: u16) -> Self {
        Self {
            _kind: kind,
            status_code: Arc::new(AtomicUsize::new(status as usize)),
            hits: Arc::new(AtomicUsize::new(0)),
            captured: Arc::new(Mutex::new(Vec::new())),
            far_snap_last_dest: false,
        }
    }

    fn with_far_snap_last_dest(mut self) -> Self {
        self.far_snap_last_dest = true;
        self
    }

    fn set_status(&self, code: u16) {
        self.status_code.store(code as usize, Ordering::SeqCst);
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn take_captured(&self) -> Vec<CapturedRequest> {
        self.captured.lock().unwrap().clone()
    }
}

fn parse_semicolon_count(raw_query: &str, key: &str) -> usize {
    for pair in raw_query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return v.split(';').filter(|s| !s.is_empty()).count();
            }
        }
    }
    0
}

fn build_matrix_json(
    kind: RoutingProviderKind,
    rows: usize,
    cols: usize,
    base_sec: f64,
    far_snap_last_dest: bool,
) -> Value {
    let durations: Vec<Vec<f64>> = (0..rows)
        .map(|r| {
            (0..cols)
                .map(|c| base_sec + (r * 10 + c) as f64 * 6.0)
                .collect()
        })
        .collect();
    let distances: Vec<Vec<f64>> = (0..rows)
        .map(|r| {
            (0..cols)
                .map(|c| 3218.688 + (r * 10 + c) as f64 * 160.9344)
                .collect()
        })
        .collect();

    match kind {
        RoutingProviderKind::Osrm => {
            let sources: Vec<Value> = (0..rows).map(|_| json!({ "distance": 4.5 })).collect();
            let destinations: Vec<Value> = (0..cols)
                .map(|c| {
                    if far_snap_last_dest && c + 1 == cols {
                        json!({ "distance": 9_500.0 })
                    } else {
                        json!({ "distance": 5.0 })
                    }
                })
                .collect();
            json!({
                "code": "Ok",
                "data_version": "osrm-test-2026",
                "durations": durations,
                "distances": distances,
                "sources": sources,
                "destinations": destinations,
            })
        }
        RoutingProviderKind::Mapbox | RoutingProviderKind::LocationIq => {
            let sources: Vec<Value> = (0..rows).map(|_| json!({ "distance": 6.0 })).collect();
            let destinations: Vec<Value> = (0..cols)
                .map(|c| {
                    if far_snap_last_dest && c + 1 == cols {
                        json!({ "distance": 8_200.0 })
                    } else {
                        json!({ "distance": 6.5 })
                    }
                })
                .collect();
            json!({
                "code": "Ok",
                "durations": durations,
                "distances": distances,
                "sources": sources,
                "destinations": destinations,
            })
        }
        RoutingProviderKind::FreeRoute => {
            let sources: Vec<Value> = (0..rows)
                .map(|_| json!({ "snapped_distance": 5.2 }))
                .collect();
            let destinations: Vec<Value> = (0..cols)
                .map(|c| {
                    if far_snap_last_dest && c + 1 == cols {
                        json!({ "snapped_distance": 7_500.0 })
                    } else {
                        json!({ "snapped_distance": 5.8 })
                    }
                })
                .collect();
            json!({
                "durations": durations,
                "distances": distances,
                "sources": sources,
                "destinations": destinations,
            })
        }
        RoutingProviderKind::OpenRouteService => {
            // ORS v2 intentionally has NO `"code"` field on HTTP 200 OK.
            let sources: Vec<Value> = (0..rows)
                .map(|_| json!({ "location": [-121.8863, 37.3382], "snapped_distance": 3.9 }))
                .collect();
            let destinations: Vec<Value> = (0..cols)
                .map(|c| {
                    if far_snap_last_dest && c + 1 == cols {
                        json!({ "location": [-121.8763, 37.3482], "snapped_distance": 9_100.0 })
                    } else {
                        json!({ "location": [-121.8763, 37.3482], "snapped_distance": 4.1 })
                    }
                })
                .collect();
            json!({
                "durations": durations,
                "distances": distances,
                "sources": sources,
                "destinations": destinations,
                "metadata": { "service": "matrix", "engine": { "version": "8.0" } }
            })
        }
    }
}

async fn spawn_mock_provider(mock: MockProviderServer) -> String {
    let app = Router::new()
        .route(
            "/table/v1/driving/:coords",
            get(
                |State(st): State<MockProviderServer>,
                 Path(coords): Path<String>,
                 RawQuery(q): RawQuery,
                 headers: HeaderMap| async move {
                    st.hits.fetch_add(1, Ordering::SeqCst);
                    let raw_q = q.clone().unwrap_or_default();
                    st.captured.lock().unwrap().push(CapturedRequest {
                        method: "GET".to_string(),
                        path: format!("/table/v1/driving/{coords}"),
                        query: q,
                        auth_header: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        content_type: None,
                        body_json: None,
                    });
                    let code = StatusCode::from_u16(st.status_code.load(Ordering::SeqCst) as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    if !code.is_success() {
                        return (code, Json(json!({ "code": "Error", "message": "upstream down" })));
                    }
                    let rows = parse_semicolon_count(&raw_q, "sources").max(1);
                    let cols = parse_semicolon_count(&raw_q, "destinations").max(1);
                    (
                        StatusCode::OK,
                        Json(build_matrix_json(
                            RoutingProviderKind::Osrm,
                            rows,
                            cols,
                            600.0,
                            st.far_snap_last_dest,
                        )),
                    )
                },
            ),
        )
        .route(
            "/nearest/v1/driving/:coord",
            get(|State(st): State<MockProviderServer>| async move {
                let code = StatusCode::from_u16(st.status_code.load(Ordering::SeqCst) as u16)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if code.is_success() {
                    (StatusCode::OK, Json(json!({ "code": "Ok", "waypoints": [] })))
                } else {
                    (code, Json(json!({ "code": "Error" })))
                }
            }),
        )
        .route(
            "/directions-matrix/v1/mapbox/driving/:coords",
            get(
                |State(st): State<MockProviderServer>,
                 Path(coords): Path<String>,
                 RawQuery(q): RawQuery,
                 headers: HeaderMap| async move {
                    st.hits.fetch_add(1, Ordering::SeqCst);
                    let raw_q = q.clone().unwrap_or_default();
                    st.captured.lock().unwrap().push(CapturedRequest {
                        method: "GET".to_string(),
                        path: format!("/directions-matrix/v1/mapbox/driving/{coords}"),
                        query: q,
                        auth_header: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        content_type: None,
                        body_json: None,
                    });
                    let code = StatusCode::from_u16(st.status_code.load(Ordering::SeqCst) as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    if !code.is_success() {
                        return (
                            code,
                            Json(json!({ "code": "InvalidInput", "message": "rate limited or error" })),
                        );
                    }
                    let rows = parse_semicolon_count(&raw_q, "sources");
                    let cols = parse_semicolon_count(&raw_q, "destinations");
                    if rows == 0 || cols == 0 || rows * cols < 2 {
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            Json(json!({
                                "code": "InvalidInput",
                                "message": "Not enough input sources and destinations given, resulting in only 1 matrix element; minimum number of matrix elements is 2."
                            })),
                        );
                    }
                    let mut resp = build_matrix_json(
                        RoutingProviderKind::Mapbox,
                        rows,
                        cols,
                        600.0,
                        st.far_snap_last_dest,
                    );
                    if rows == 1 && cols == 2 && raw_q.contains("sources=0&") && raw_q.contains("destinations=0;1") {
                        // Mirror real Mapbox self-leg [0][0]=0 vs real origin->dest [0][1] so any accidental
                        // col-0 extraction fails tests immediately.
                        resp["durations"] = json!([[0.0, 600.0]]);
                        resp["distances"] = json!([[0.0, 3218.688]]);
                    }
                    (StatusCode::OK, Json(resp))
                },
            ),
        )
        .route(
            "/v1/matrix/driving/:coords",
            get(
                |State(st): State<MockProviderServer>,
                 Path(coords): Path<String>,
                 RawQuery(q): RawQuery,
                 headers: HeaderMap| async move {
                    st.hits.fetch_add(1, Ordering::SeqCst);
                    let raw_q = q.clone().unwrap_or_default();
                    st.captured.lock().unwrap().push(CapturedRequest {
                        method: "GET".to_string(),
                        path: format!("/v1/matrix/driving/{coords}"),
                        query: q,
                        auth_header: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        content_type: None,
                        body_json: None,
                    });
                    let code = StatusCode::from_u16(st.status_code.load(Ordering::SeqCst) as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    if !code.is_success() {
                        return (code, Json(json!({ "error": "Rate Limited" })));
                    }
                    let rows = parse_semicolon_count(&raw_q, "sources").max(1);
                    let cols = parse_semicolon_count(&raw_q, "destinations").max(1);
                    (
                        StatusCode::OK,
                        Json(build_matrix_json(
                            RoutingProviderKind::LocationIq,
                            rows,
                            cols,
                            600.0,
                            st.far_snap_last_dest,
                        )),
                    )
                },
            ),
        )
        .route(
            "/v1/matrix/driving-car",
            post(
                |State(st): State<MockProviderServer>, headers: HeaderMap, body: Bytes| async move {
                    st.hits.fetch_add(1, Ordering::SeqCst);
                    let parsed_body: Option<Value> = serde_json::from_slice(&body).ok();
                    st.captured.lock().unwrap().push(CapturedRequest {
                        method: "POST".to_string(),
                        path: "/v1/matrix/driving-car".to_string(),
                        query: None,
                        auth_header: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        content_type: headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        body_json: parsed_body.clone(),
                    });
                    let code = StatusCode::from_u16(st.status_code.load(Ordering::SeqCst) as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    if !code.is_success() {
                        return (code, Json(json!({ "error": { "message": "freeroute failure" } })));
                    }
                    let rows = parsed_body
                        .as_ref()
                        .and_then(|b| b["sources"].as_array().map(Vec::len))
                        .unwrap_or(1);
                    let cols = parsed_body
                        .as_ref()
                        .and_then(|b| b["destinations"].as_array().map(Vec::len))
                        .unwrap_or(1);
                    (
                        StatusCode::OK,
                        Json(build_matrix_json(
                            RoutingProviderKind::FreeRoute,
                            rows,
                            cols,
                            600.0,
                            st.far_snap_last_dest,
                        )),
                    )
                },
            ),
        )
        .route(
            "/v2/matrix/driving-car",
            post(
                |State(st): State<MockProviderServer>, headers: HeaderMap, body: Bytes| async move {
                    st.hits.fetch_add(1, Ordering::SeqCst);
                    let parsed_body: Option<Value> = serde_json::from_slice(&body).ok();
                    st.captured.lock().unwrap().push(CapturedRequest {
                        method: "POST".to_string(),
                        path: "/v2/matrix/driving-car".to_string(),
                        query: None,
                        auth_header: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        content_type: headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        body_json: parsed_body.clone(),
                    });
                    let code = StatusCode::from_u16(st.status_code.load(Ordering::SeqCst) as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    if !code.is_success() {
                        return (code, Json(json!({ "error": { "code": 2003, "message": "ors error" } })));
                    }
                    let rows = parsed_body
                        .as_ref()
                        .and_then(|b| b["sources"].as_array().map(Vec::len))
                        .unwrap_or(1);
                    let cols = parsed_body
                        .as_ref()
                        .and_then(|b| b["destinations"].as_array().map(Vec::len))
                        .unwrap_or(1);
                    (
                        StatusCode::OK,
                        Json(build_matrix_json(
                            RoutingProviderKind::OpenRouteService,
                            rows,
                            cols,
                            600.0,
                            st.far_snap_last_dest,
                        )),
                    )
                },
            ),
        )
        .with_state(mock);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

// ------------------------------------------------------------------------------------------
// 1. All 5 Built-in Routing Extensions in `ExtensionHub`, Alias Auth Detection & `test_fetch`
// ------------------------------------------------------------------------------------------

static TEST_SERIAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn builtin_routing_extensions_validate_auth_aliases_and_redacted_test_fetch() {
    let _serial = TEST_SERIAL_LOCK.lock().await;
    assert_eq!(
        parse_capability_domain("routing"),
        Some(CapabilityDomain::DomainRouting)
    );
    assert_eq!(
        parse_capability_domain("drive_routing"),
        Some(CapabilityDomain::DomainRouting)
    );
    assert_eq!(
        parse_capability_domain("routing_matrix"),
        Some(CapabilityDomain::DomainRouting)
    );
    assert_eq!(
        parse_capability_domain("DOMAIN_ROUTING"),
        Some(CapabilityDomain::DomainRouting)
    );
    assert_eq!(
        capability_domain_slug(CapabilityDomain::DomainRouting),
        "routing"
    );
    validate_proto_message_type(
        CapabilityDomain::DomainRouting,
        "openestate.extension.v1.RoutingMatrixProto",
    )
    .expect("RoutingMatrixProto is valid for DomainRouting");

    let hub = ExtensionHub::new_default();
    let expected = [
        ("osrm_routing", 200),
        ("mapbox_routing", 120),
        ("locationiq_routing", 115),
        ("freeroute_routing", 110),
        ("openrouteservice_routing", 105),
    ];

    let empty_env = EnvConfig::from_map(HashMap::new());
    let unconfigured_list = hub.list_extensions_json(Some(&empty_env));

    for (ext_id, prio) in expected {
        assert!(hub.is_enabled(ext_id), "{ext_id} must be enabled by default");
        let detail = hub
            .get_extension_json(ext_id, Some(&empty_env))
            .unwrap_or_else(|| panic!("missing {ext_id}"));
        assert_eq!(detail["domain"], json!("routing"));
        assert_eq!(detail["domainEnum"], json!("DOMAIN_ROUTING"));
        assert_eq!(detail["priority"], json!(prio));
        assert_eq!(
            detail["protoMessageType"],
            json!("openestate.extension.v1.RoutingMatrixProto")
        );
        assert_eq!(
            detail["authConfigured"],
            json!(false),
            "{ext_id} must report authConfigured=false when env vars are unset"
        );
        assert!(
            detail["manifestYaml"]
                .as_str()
                .map(|s| s.contains(ext_id))
                .unwrap_or(false),
            "{ext_id} manifestYaml must be embedded for /admin"
        );
        assert!(
            detail["openapiYaml"]
                .as_str()
                .map(|s| s.contains("openapi:"))
                .unwrap_or(false),
            "{ext_id} openapiYaml must be embedded for /admin"
        );
        assert!(unconfigured_list
            .iter()
            .any(|row| row["extensionId"] == ext_id && row["authConfigured"] == json!(false)));
    }

    // Unconfigured `test_fetch` must return `auth_missing` (`AUTH_MISSING: ...`) for all 5.
    let loopback_http = HttpClient::offline_loopback_only(None);
    let probe_query = ExtensionQueryProto {
        lat: Some(37.3382),
        lon: Some(-121.8863),
        ..Default::default()
    };
    for (ext_id, _) in expected {
        let res = hub
            .test_fetch(ext_id, &probe_query, &loopback_http, &empty_env)
            .await
            .expect("test_fetch returns structured result");
        assert!(!res.ok, "{ext_id} should fail when unconfigured");
        assert_eq!(res.health_status, "auth_missing", "{ext_id}");
        assert!(
            res.error
                .as_deref()
                .unwrap_or("")
                .contains("AUTH_MISSING"),
            "{ext_id} error: {:?}",
            res.error
        );
        assert_eq!(res.http_preview["resolved"], json!(false));
    }

    // Check alias env vars (`OSRM_BASE_URL`, `MAPBOX_API_KEY`, `LOCATIONIQ_ACCESS_TOKEN`, `ORS_API_KEY`)
    // AND live 2-point `test_fetch` with strict secret redaction!
    let osrm_mock = MockProviderServer::new(RoutingProviderKind::Osrm, 200);
    let mbx_mock = MockProviderServer::new(RoutingProviderKind::Mapbox, 200);
    let liq_mock = MockProviderServer::new(RoutingProviderKind::LocationIq, 200);
    let fr_mock = MockProviderServer::new(RoutingProviderKind::FreeRoute, 200);
    let ors_mock = MockProviderServer::new(RoutingProviderKind::OpenRouteService, 200);

    let osrm_url = spawn_mock_provider(osrm_mock.clone()).await;
    let mbx_url = spawn_mock_provider(mbx_mock.clone()).await;
    let liq_url = spawn_mock_provider(liq_mock.clone()).await;
    let fr_url = spawn_mock_provider(fr_mock.clone()).await;
    let ors_url = spawn_mock_provider(ors_mock.clone()).await;

    let secrets = [
        "pk.SENTINEL_MAPBOX_SECRET_991",
        "liq_SENTINEL_SECRET_992",
        "fr_SENTINEL_BEARER_993",
        "ors_SENTINEL_HEADER_994",
    ];
    let alias_env = EnvConfig::from_map(HashMap::from([
        ("OSRM_BASE_URL".to_string(), osrm_url),
        ("MAPBOX_API_KEY".to_string(), secrets[0].to_string()),
        ("MAPBOX_BASE_URL".to_string(), mbx_url),
        (
            "LOCATIONIQ_ACCESS_TOKEN".to_string(),
            secrets[1].to_string(),
        ),
        ("LOCATIONIQ_BASE_URL".to_string(), liq_url),
        ("FREEROUTE_API_KEY".to_string(), secrets[2].to_string()),
        ("FREEROUTE_BASE_URL".to_string(), fr_url),
        ("ORS_API_KEY".to_string(), secrets[3].to_string()),
        ("OPENROUTESERVICE_BASE_URL".to_string(), ors_url),
    ]));

    let configured_list = hub.list_extensions_json(Some(&alias_env));
    let list_serialized = serde_json::to_string(&configured_list).unwrap();
    for s in secrets {
        assert!(
            !list_serialized.contains(s),
            "admin extension list leaked secret {s}"
        );
    }
    for (ext_id, _) in expected {
        assert!(
            configured_list
                .iter()
                .any(|row| row["extensionId"] == ext_id && row["authConfigured"] == json!(true)),
            "{ext_id} must detect alias env var as authConfigured=true"
        );

        let res = hub
            .test_fetch(ext_id, &probe_query, &loopback_http, &alias_env)
            .await
            .expect("configured test_fetch");
        assert!(res.ok, "{ext_id} test_fetch failed: {:?}", res.error);
        assert_eq!(res.health_status, "healthy");
        assert_eq!(res.envelope_count, 1);
        assert!(res.protobuf_byte_len > 0);

        let decoded: ExtensionEnvelopeProto =
            decode_proto_bytes(&res.protobuf_wire_bytes).expect("decode RoutingMatrixProto wire");
        assert_eq!(decoded.domain, CapabilityDomain::DomainRouting as i32);
        match decoded.payload {
            Some(Payload::RoutingMatrix(rm)) => {
                assert_eq!(rm.provider_id, ext_id);
                assert!(rm.healthy);
                assert_eq!(rm.origin_count, 1);
                assert_eq!(rm.destination_count, 1);
                assert!(rm.sample_duration_sec.unwrap_or(0.0) > 0.0);
                assert!(rm.sample_distance_m.unwrap_or(0.0) > 0.0);
                if ext_id == "mapbox_routing" {
                    assert_eq!(rm.sample_duration_sec, Some(600.0));
                    assert_eq!(rm.sample_distance_m, Some(3218.688));
                }
            }
            other => panic!("unexpected payload for {ext_id}: {other:?}"),
        }

        let preview_str = serde_json::to_string(&res.http_preview).unwrap();
        if ext_id == "mapbox_routing" {
            let preview_url = res.http_preview["url"].as_str().unwrap_or("");
            assert!(
                preview_url.contains("sources=0&destinations=0%3B1")
                    || preview_url.contains("sources=0&destinations=0;1"),
                "mapbox 1x1 preview must use 1x2 padded query: {preview_url}"
            );
        }
        let canon_str = serde_json::to_string(&res.canonical_json).unwrap();
        for s in secrets {
            assert!(
                !preview_str.contains(s),
                "{ext_id} http_preview leaked secret {s}: {preview_str}"
            );
            assert!(
                !canon_str.contains(s),
                "{ext_id} canonical_json leaked secret {s}"
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// 2. Exact Wire Formats, Auth Headers/Query, Traffic Multipliers, 2 RPS Rate Limit & >25 Chunking
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn all_four_cloud_providers_honor_exact_wire_specs_chunking_and_rate_limits() {
    let _serial = TEST_SERIAL_LOCK.lock().await;
    let origins = vec![
        LatLon::new(37.33820, -121.88630),
        LatLon::new(37.35210, -121.90120),
    ];
    // 26 unique destinations + 2 origins = 28 coordinates (> 25 limit -> must split into 2 chunks:
    // chunk 1 = 2 sources + 23 destinations = 25 coords; chunk 2 = 2 sources + 3 destinations = 5 coords).
    let dests_26: Vec<LatLon> = (0..26)
        .map(|i| LatLon::new(37.20000 + (i as f64) * 0.005, -121.80000 - (i as f64) * 0.004))
        .collect();

    // --- 2A. Mapbox (`mapbox/driving` 1.0 traffic multiplier + far-snap drift -> OutsideCoverage) ---
    {
        let mock = MockProviderServer::new(RoutingProviderKind::Mapbox, 200).with_far_snap_last_dest();
        let url = spawn_mock_provider(mock.clone()).await;
        let env = EnvConfig::from_map(HashMap::from([
            ("MAPBOX_ACCESS_TOKEN".to_string(), "pk.mbx_wire_tok".to_string()),
            ("MAPBOX_BASE_URL".to_string(), url),
        ]));
        let http = HttpClient::from_env(&env, None);
        let hub = ExtensionHub::new_default();
        let m = RoutingClient::from_http(&http)
            .with_extension_hub(hub)
            .drive_matrix(&origins, &dests_26)
            .await;

        assert_eq!(mock.hit_count(), 2, "2x26 must split into 2 <=25-coord chunks");
        let reqs = mock.take_captured();
        assert_eq!(reqs[0].method, "GET");
        assert!(reqs[0]
            .path
            .starts_with("/directions-matrix/v1/mapbox/driving/-121.88630,37.33820;-121.90120,37.35210;"));
        let coords_0 = reqs[0]
            .path
            .trim_start_matches("/directions-matrix/v1/mapbox/driving/")
            .split(';')
            .count();
        let coords_1 = reqs[1]
            .path
            .trim_start_matches("/directions-matrix/v1/mapbox/driving/")
            .split(';')
            .count();
        assert_eq!(coords_0, 25);
        assert_eq!(coords_1, 5);

        let q0 = reqs[0].query.as_deref().unwrap();
        assert!(q0.contains("sources=0;1"), "{q0}");
        assert!(q0.contains("destinations=2;3;4;"), "{q0}");
        assert!(q0.contains("annotations=duration,distance"), "{q0}");
        assert!(q0.contains("access_token=pk.mbx_wire_tok"), "{q0}");

        // Mapbox `mapbox/driving` uses multiplier 1.0 by default!
        assert!((m.traffic_buffer - 1.0).abs() < 1e-9);
        let c00 = m.cells[0][0].unwrap();
        assert!(!c00.estimated);
        assert!((c00.buffered_min - c00.free_flow_min).abs() < 1e-9);

        // Last destination of each chunk had snap distance > 3000m -> OutsideCoverage!
        let c_last = m.cells[0][25].unwrap();
        assert!(c_last.estimated);
        assert_eq!(c_last.reason, Some(EstimateReason::OutsideCoverage));

        // 1x1 matrix against Mapbox must pass the >=2 element rule using `sources=0&destinations=0;1`,
        // extract col 1 (`600s` -> `10.0 min`, `3218.688m` -> `2.0 mi`), and read snap drift from destinations[1]!
        let mock_1x1_ok = MockProviderServer::new(RoutingProviderKind::Mapbox, 200);
        let url_1x1_ok = spawn_mock_provider(mock_1x1_ok.clone()).await;
        let env_1x1_ok = EnvConfig::from_map(HashMap::from([
            ("MAPBOX_ACCESS_TOKEN".to_string(), "pk.mbx_1x1_tok".to_string()),
            ("MAPBOX_BASE_URL".to_string(), url_1x1_ok),
        ]));
        let m_1x1 = RoutingClient::from_http(&HttpClient::from_env(&env_1x1_ok, None))
            .with_extension_hub(ExtensionHub::new_default())
            .drive_matrix(&origins[..1], &dests_26[..1])
            .await;
        assert_eq!(mock_1x1_ok.hit_count(), 1);
        let reqs_1x1 = mock_1x1_ok.take_captured();
        let q_1x1 = reqs_1x1[0].query.as_deref().unwrap();
        assert!(q_1x1.contains("sources=0&destinations=0;1"), "{q_1x1}");
        let cell_1x1 = m_1x1.cells[0][0].unwrap();
        assert!(!cell_1x1.estimated);
        assert!((cell_1x1.free_flow_min - 10.0).abs() < 1e-6);
        assert!((cell_1x1.distance_mi - 2.0).abs() < 1e-3);

        let m_1x1_snap = RoutingClient::from_http(&http)
            .with_extension_hub(ExtensionHub::new_default())
            .drive_matrix(&origins[..1], &dests_26[..1])
            .await;
        let cell_1x1_snap = m_1x1_snap.cells[0][0].unwrap();
        assert!(cell_1x1_snap.estimated);
        assert_eq!(cell_1x1_snap.reason, Some(EstimateReason::OutsideCoverage));
    }

    // --- 2B. LocationIQ (`GET /v1/matrix/driving/...&key=...`, 1.15 multiplier & 600ms burst-1 limiter) ---
    {
        let mock = MockProviderServer::new(RoutingProviderKind::LocationIq, 200);
        let url = spawn_mock_provider(mock.clone()).await;
        let env = EnvConfig::from_map(HashMap::from([
            ("LOCATIONIQ_API_KEY".to_string(), "liq_wire_key".to_string()),
            ("LOCATIONIQ_BASE_URL".to_string(), url),
        ]));
        let http = HttpClient::from_env(&env, None);
        let hub = ExtensionHub::new_default();
        // 1 origin x 50 destinations -> 3 chunks (1+24=25, 1+24=25, 1+2=3).
        // With burst=1 at 600ms period, chunk 1 fires immediately, chunk 2 waits >=600ms,
        // and chunk 3 waits >=600ms more (>=1100ms total elapsed, <=2 requests per 1.0s window).
        let dests_50: Vec<LatLon> = (0..50)
            .map(|i| LatLon::new(37.10000 + (i as f64) * 0.004, -121.75000 - (i as f64) * 0.003))
            .collect();
        let t0 = Instant::now();
        let m = RoutingClient::from_http(&http)
            .with_extension_hub(hub)
            .drive_matrix(&origins[..1], &dests_50)
            .await;
        let elapsed = t0.elapsed();

        assert_eq!(mock.hit_count(), 3);
        assert!(
            elapsed.as_millis() >= 1100,
            "LocationIQ burst=1 / 600ms limiter must enforce >=1100ms across 3 chunks (elapsed {:?})",
            elapsed
        );
        let reqs = mock.take_captured();
        assert_eq!(reqs[0].method, "GET");
        assert!(reqs[0].path.starts_with("/v1/matrix/driving/"));
        let q0 = reqs[0].query.as_deref().unwrap();
        assert!(q0.contains("sources=0"), "{q0}");
        assert!(q0.contains("annotations=duration,distance"), "{q0}");
        assert!(q0.contains("key=liq_wire_key"), "{q0}");
        assert_eq!(m.routed, 50);
        assert!((m.traffic_buffer - 1.15).abs() < 1e-9);
        let c00 = m.cells[0][0].unwrap();
        assert!((c00.buffered_min - c00.free_flow_min * 1.15).abs() < 1e-9);
    }

    // --- 2C. FreeRoute (`POST /v1/matrix/driving-car`, `Authorization: Bearer ...`, JSON body) ---
    {
        let mock = MockProviderServer::new(RoutingProviderKind::FreeRoute, 200);
        let url = spawn_mock_provider(mock.clone()).await;
        let env = EnvConfig::from_map(HashMap::from([
            ("FREEROUTE_API_KEY".to_string(), "fr_wire_tok".to_string()),
            ("FREEROUTE_BASE_URL".to_string(), url),
        ]));
        let http = HttpClient::from_env(&env, None);
        let hub = ExtensionHub::new_default();
        let m = RoutingClient::from_http(&http)
            .with_extension_hub(hub)
            .drive_matrix(&origins, &dests_26)
            .await;

        assert_eq!(mock.hit_count(), 2);
        let reqs = mock.take_captured();
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/v1/matrix/driving-car");
        assert_eq!(
            reqs[0].auth_header.as_deref(),
            Some("Bearer fr_wire_tok")
        );
        assert!(reqs[0]
            .content_type
            .as_deref()
            .unwrap_or("")
            .contains("application/json"));
        let b0 = reqs[0].body_json.as_ref().expect("JSON body");
        assert_eq!(b0["locations"].as_array().unwrap().len(), 25);
        assert_eq!(b0["sources"], json!([0, 1]));
        assert_eq!(b0["destinations"].as_array().unwrap().len(), 23);
        assert_eq!(b0["metrics"], json!(["duration", "distance"]));
        assert!(b0.get("units").is_none());
        assert_eq!(m.routed, 52);
        assert!((m.traffic_buffer - 1.15).abs() < 1e-9);
    }

    // --- 2D. OpenRouteService (`POST /v2/matrix/driving-car`, raw `Authorization: {key}`, `"units": "m"`, no `"code"`) ---
    {
        let mock = MockProviderServer::new(RoutingProviderKind::OpenRouteService, 200);
        let url = spawn_mock_provider(mock.clone()).await;
        let env = EnvConfig::from_map(HashMap::from([
            (
                "OPENROUTESERVICE_API_KEY".to_string(),
                "ors_wire_raw_key".to_string(),
            ),
            ("OPENROUTESERVICE_BASE_URL".to_string(), url),
        ]));
        let http = HttpClient::from_env(&env, None);
        let hub = ExtensionHub::new_default();
        let m = RoutingClient::from_http(&http)
            .with_extension_hub(hub)
            .drive_matrix(&origins, &dests_26)
            .await;

        assert_eq!(mock.hit_count(), 2);
        let reqs = mock.take_captured();
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/v2/matrix/driving-car");
        assert_eq!(
            reqs[0].auth_header.as_deref(),
            Some("ors_wire_raw_key"),
            "ORS uses raw Authorization key without Bearer prefix"
        );
        let b0 = reqs[0].body_json.as_ref().expect("JSON body");
        assert_eq!(b0["locations"].as_array().unwrap().len(), 25);
        assert_eq!(b0["sources"], json!([0, 1]));
        assert_eq!(b0["metrics"], json!(["duration", "distance"]));
        assert_eq!(b0["units"], json!("m"));
        assert_eq!(m.routed, 52);
        assert!((m.traffic_buffer - 1.15).abs() < 1e-9);
    }
}

// ------------------------------------------------------------------------------------------
// 3. Regional OSRM Cascade (`california` 503 -> containing `us-west` 200 without hitting Tier 2)
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn regional_osrm_cascade_falls_back_from_california_to_us_west_before_cloud() {
    let _serial = TEST_SERIAL_LOCK.lock().await;
    let ca_osrm = MockProviderServer::new(RoutingProviderKind::Osrm, 503);
    let west_osrm = MockProviderServer::new(RoutingProviderKind::Osrm, 200);
    let mbx_cloud = MockProviderServer::new(RoutingProviderKind::Mapbox, 200);

    let ca_url = spawn_mock_provider(ca_osrm.clone()).await;
    let west_url = spawn_mock_provider(west_osrm.clone()).await;
    let mbx_url = spawn_mock_provider(mbx_cloud.clone()).await;

    let regions_json = serde_json::to_string(&json!([
        {
            "name": "us-west",
            "bbox": [-125.0, 31.0, -102.0, 49.5],
            "baseUrl": west_url,
        },
        {
            "name": "california",
            "bbox": [-124.5, 32.5, -114.1, 42.0],
            "baseUrl": ca_url,
        }
    ]))
    .unwrap();

    let env = EnvConfig::from_map(HashMap::from([
        ("OSRM_REGIONS_JSON".to_string(), regions_json),
        ("OSRM_FAILURE_COOLDOWN_SECS".to_string(), "30".to_string()),
        ("MAPBOX_ACCESS_TOKEN".to_string(), "pk.should_not_be_called".to_string()),
        ("MAPBOX_BASE_URL".to_string(), mbx_url),
    ]));
    let http = HttpClient::from_env(&env, None);
    let hub = ExtensionHub::new_default();
    let client = RoutingClient::from_http(&http).with_extension_hub(hub);

    let src = [LatLon::new(37.3101, -121.8911)];
    let dst = [LatLon::new(37.3402, -121.8802)];
    let m1 = client.drive_matrix(&src, &dst).await;

    assert_eq!(ca_osrm.hit_count(), 1, "most specific region tried first");
    assert!(is_router_in_cooldown(&ca_url), "failed 503 region enters cooldown");
    assert_eq!(west_osrm.hit_count(), 1, "cascades immediately to containing us-west");
    assert_eq!(mbx_cloud.hit_count(), 0, "Tier-2 cloud must NOT be hit when us-west succeeds");
    assert_eq!(m1.routed, 1);
    assert_eq!(m1.estimated, 0);
    assert!(m1.used_only_self_hosted_osrm());
    assert!(
        !m1.warnings.iter().any(|w| w.contains("OSRM router unavailable")),
        "warnings must stay clean when secondary OSRM region succeeded: {:?}",
        m1.warnings
    );

    // Second call with fresh CA coords skips cooled `california` and hits `us-west` directly!
    let src2 = [LatLon::new(37.3215, -121.9105)];
    let dst2 = [LatLon::new(37.3612, -121.9255)];
    let m2 = client.drive_matrix(&src2, &dst2).await;
    assert_eq!(ca_osrm.hit_count(), 1, "california skipped while in cooldown");
    assert_eq!(west_osrm.hit_count(), 2);
    assert_eq!(mbx_cloud.hit_count(), 0);
    assert_eq!(m2.routed, 1);
}

// ------------------------------------------------------------------------------------------
// 4. NAS OSRM Down -> Round-Robin & Priority Load Balancing + Proxy Bypass + Offline Guard
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn osrm_down_load_balances_across_all_four_cloud_providers_and_bypasses_dead_proxy() {
    let _serial = TEST_SERIAL_LOCK.lock().await;
    let osrm_down = MockProviderServer::new(RoutingProviderKind::Osrm, 503);
    let mbx = MockProviderServer::new(RoutingProviderKind::Mapbox, 200);
    let liq = MockProviderServer::new(RoutingProviderKind::LocationIq, 200);
    let fr = MockProviderServer::new(RoutingProviderKind::FreeRoute, 200);
    let ors = MockProviderServer::new(RoutingProviderKind::OpenRouteService, 200);

    let osrm_url = spawn_mock_provider(osrm_down.clone()).await;
    let mbx_url = spawn_mock_provider(mbx.clone()).await;
    let liq_url = spawn_mock_provider(liq.clone()).await;
    let fr_url = spawn_mock_provider(fr.clone()).await;
    let ors_url = spawn_mock_provider(ors.clone()).await;
    let dead_proxy = closed_port_url().await;

    let env = EnvConfig::from_map(HashMap::from([
        ("OSRM_BASE_URL".to_string(), osrm_url),
        ("OSRM_FAILURE_COOLDOWN_SECS".to_string(), "60".to_string()),
        ("ROUTING_FALLBACK_STRATEGY".to_string(), "round_robin".to_string()),
        ("MAPBOX_ACCESS_TOKEN".to_string(), "pk.rr_mbx".to_string()),
        ("MAPBOX_BASE_URL".to_string(), mbx_url.clone()),
        ("LOCATIONIQ_API_KEY".to_string(), "liq_rr_key".to_string()),
        ("LOCATIONIQ_BASE_URL".to_string(), liq_url.clone()),
        ("FREEROUTE_API_KEY".to_string(), "fr_rr_key".to_string()),
        ("FREEROUTE_BASE_URL".to_string(), fr_url.clone()),
        ("OPENROUTESERVICE_API_KEY".to_string(), "ors_rr_key".to_string()),
        ("OPENROUTESERVICE_BASE_URL".to_string(), ors_url.clone()),
        // Configure a dead outbound proxy with strict mode to prove API-keyed routing ALWAYS uses direct egress!
        ("OUTBOUND_PROXY_URLS".to_string(), dead_proxy),
        ("PROXY_POOL_STRICT".to_string(), "true".to_string()),
    ]));
    let http = HttpClient::from_env(&env, None);
    for host in [
        "api.mapbox.com",
        "us1.locationiq.com",
        "eu1.locationiq.com",
        "api.maps.freeroute.org",
        "api.openrouteservice.org",
    ] {
        assert!(
            http.should_bypass_proxy(host),
            "{host} must be in proxy bypass list"
        );
    }

    let hub = ExtensionHub::new_default();
    let client = RoutingClient::from_http(&http).with_extension_hub(hub);

    // Fire 8 distinct 1x1 requests -> must distribute 2, 2, 2, 2 across the 4 healthy Tier-2 services!
    for i in 0..8 {
        let s = [LatLon::new(37.3000 + (i as f64) * 0.003, -121.8800)];
        let d = [LatLon::new(37.3400 + (i as f64) * 0.003, -121.9100)];
        let m = client.drive_matrix(&s, &d).await;
        assert_eq!(m.routed, 1, "call {i} must succeed via Tier-2 pool");
        assert_eq!(m.estimated, 0);
        assert!(!m.used_only_self_hosted_osrm());
        assert_eq!(m.attributions().len(), 1);
    }

    assert_eq!(osrm_down.hit_count(), 1, "OSRM tried once then cooled down");
    assert_eq!(mbx.hit_count(), 2);
    assert_eq!(liq.hit_count(), 2);
    assert_eq!(fr.hit_count(), 2);
    assert_eq!(ors.hit_count(), 2);

    // Also verify `ROUTING_FALLBACK_STRATEGY=priority` + `ROUTING_FALLBACK_ORDER`
    let prio_env = EnvConfig::from_map(HashMap::from([
        ("ROUTING_FALLBACK_STRATEGY".to_string(), "priority".to_string()),
        (
            "ROUTING_FALLBACK_ORDER".to_string(),
            "openrouteservice_routing,freeroute_routing".to_string(),
        ),
        ("MAPBOX_ACCESS_TOKEN".to_string(), "pk.rr_mbx".to_string()),
        ("MAPBOX_BASE_URL".to_string(), mbx_url),
        ("FREEROUTE_API_KEY".to_string(), "fr_rr_key".to_string()),
        ("FREEROUTE_BASE_URL".to_string(), fr_url),
        ("OPENROUTESERVICE_API_KEY".to_string(), "ors_rr_key".to_string()),
        ("OPENROUTESERVICE_BASE_URL".to_string(), ors_url),
    ]));
    let prio_client = RoutingClient::from_http(&HttpClient::from_env(&prio_env, None))
        .with_extension_hub(ExtensionHub::new_default());
    for i in 0..3 {
        let s = [LatLon::new(37.4100 + (i as f64) * 0.002, -122.0100)];
        let d = [LatLon::new(37.4300 + (i as f64) * 0.002, -122.0400)];
        let m = prio_client.drive_matrix(&s, &d).await;
        assert_eq!(m.routed, 1);
    }
    assert_eq!(ors.hit_count(), 5, "priority strategy must route all 3 calls to first ordered service");
    assert_eq!(fr.hit_count(), 2);

    // Verify `HttpClient::offline_loopback_only` blocks external cloud URLs with zero network egress.
    let ext_env = EnvConfig::from_map(HashMap::from([(
        "MAPBOX_ACCESS_TOKEN".to_string(),
        "pk.offline_blocked_token".to_string(),
    )]));
    let offline_http = HttpClient::offline_loopback_only(None);
    let offline_client = RoutingClient::new(offline_http, RoutingConfig::from_env(&ext_env))
        .with_extension_hub(ExtensionHub::new_default());
    let m_off = offline_client
        .drive_matrix(&[LatLon::new(37.33, -121.88)], &[LatLon::new(37.35, -121.90)])
        .await;
    assert_eq!(m_off.routed, 0);
    assert_eq!(m_off.estimated, 1);
    assert_eq!(m_off.requests, 0);
}

// ------------------------------------------------------------------------------------------
// 5. Mid-Batch Failover (`429` + `500`), Cooldowns & `/api/ops/telemetry` Secret Hygiene
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn mid_batch_failover_places_429_and_500_in_cooldown_and_redacts_ops_telemetry() {
    let _serial = TEST_SERIAL_LOCK.lock().await;
    let osrm_down = MockProviderServer::new(RoutingProviderKind::Osrm, 503);
    let mbx_429 = MockProviderServer::new(RoutingProviderKind::Mapbox, 429);
    let liq_500 = MockProviderServer::new(RoutingProviderKind::LocationIq, 500);
    let fr_ok = MockProviderServer::new(RoutingProviderKind::FreeRoute, 200);
    let ors_ok = MockProviderServer::new(RoutingProviderKind::OpenRouteService, 200);

    let osrm_url = spawn_mock_provider(osrm_down.clone()).await;
    let mbx_url = spawn_mock_provider(mbx_429.clone()).await;
    let liq_url = spawn_mock_provider(liq_500.clone()).await;
    let fr_url = spawn_mock_provider(fr_ok.clone()).await;
    let ors_url = spawn_mock_provider(ors_ok.clone()).await;

    let sentinel_secrets = [
        "pk.SENTINEL_FAILOVER_MBX_111",
        "liq_SENTINEL_FAILOVER_LIQ_222",
        "fr_SENTINEL_FAILOVER_FR_333",
        "ors_SENTINEL_FAILOVER_ORS_444",
    ];
    let env = EnvConfig::from_map(HashMap::from([
        ("OSRM_BASE_URL".to_string(), osrm_url.clone()),
        ("ROUTING_FALLBACK_STRATEGY".to_string(), "priority".to_string()),
        (
            "ROUTING_FALLBACK_ORDER".to_string(),
            "mapbox_routing,locationiq_routing,freeroute_routing,openrouteservice_routing"
                .to_string(),
        ),
        ("MAPBOX_ACCESS_TOKEN".to_string(), sentinel_secrets[0].to_string()),
        ("MAPBOX_BASE_URL".to_string(), mbx_url.clone()),
        ("LOCATIONIQ_API_KEY".to_string(), sentinel_secrets[1].to_string()),
        ("LOCATIONIQ_BASE_URL".to_string(), liq_url.clone()),
        ("FREEROUTE_API_KEY".to_string(), sentinel_secrets[2].to_string()),
        ("FREEROUTE_BASE_URL".to_string(), fr_url.clone()),
        (
            "OPENROUTESERVICE_API_KEY".to_string(),
            sentinel_secrets[3].to_string(),
        ),
        ("OPENROUTESERVICE_BASE_URL".to_string(), ors_url.clone()),
    ]));

    let http = HttpClient::from_env(&env, None);
    let hub = ExtensionHub::new_default();
    let client = RoutingClient::from_http(&http).with_extension_hub(Arc::clone(&hub));

    let origins = [LatLon::new(37.3382, -121.8863)];
    let dests: Vec<LatLon> = (0..26)
        .map(|i| LatLon::new(37.2500 + (i as f64) * 0.003, -121.8500 - (i as f64) * 0.002))
        .collect();

    // Chunk 1 tries Mapbox (429 -> 120s cooldown) -> LocationIQ (500 -> 25s cooldown) -> FreeRoute (200 OK!).
    // Chunk 2 skips cooled Mapbox & LocationIQ immediately and succeeds directly on FreeRoute!
    let m = client.drive_matrix(&origins, &dests).await;
    assert_eq!(m.routed, 26);
    assert_eq!(m.estimated, 0);
    assert_eq!(mbx_429.hit_count(), 1);
    assert_eq!(liq_500.hit_count(), 1);
    assert_eq!(fr_ok.hit_count(), 2);
    assert!(is_cloud_provider_in_cooldown(
        RoutingProviderKind::Mapbox,
        &mbx_url
    ));
    assert!(is_cloud_provider_in_cooldown(
        RoutingProviderKind::LocationIq,
        &liq_url
    ));
    assert!(
        !m.warnings.iter().any(|w| w.contains("OSRM router unavailable")),
        "successful Tier-2 failover must not mark report degraded: {:?}",
        m.warnings
    );

    // Inspect `/api/ops/telemetry` snapshot: must expose non-secret status of all 5 extensions
    // and preserve existing `osrm.*` keys without leaking any tokens or raw URLs!
    let temp_ops_dir = std::env::temp_dir().join(format!(
        "oe_routing_ops_test_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&temp_ops_dir);
    let reports_dir = temp_ops_dir.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
    let db_path = temp_ops_dir.join("ops.sqlite");
    let mut ops_map = HashMap::from([
        ("OSRM_BASE_URL".to_string(), osrm_url.clone()),
        (
            "ROUTING_FALLBACK_STRATEGY".to_string(),
            "priority".to_string(),
        ),
        (
            "ROUTING_FALLBACK_ORDER".to_string(),
            "mapbox_routing,locationiq_routing,freeroute_routing,openrouteservice_routing"
                .to_string(),
        ),
        (
            "MAPBOX_ACCESS_TOKEN".to_string(),
            sentinel_secrets[0].to_string(),
        ),
        ("MAPBOX_BASE_URL".to_string(), mbx_url.clone()),
        (
            "LOCATIONIQ_API_KEY".to_string(),
            sentinel_secrets[1].to_string(),
        ),
        ("LOCATIONIQ_BASE_URL".to_string(), liq_url.clone()),
        (
            "FREEROUTE_API_KEY".to_string(),
            sentinel_secrets[2].to_string(),
        ),
        ("FREEROUTE_BASE_URL".to_string(), fr_url.clone()),
        (
            "OPENROUTESERVICE_API_KEY".to_string(),
            sentinel_secrets[3].to_string(),
        ),
        ("OPENROUTESERVICE_BASE_URL".to_string(), ors_url.clone()),
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
    let _ = &mut ops_map;
    let ops_env = EnvConfig::from_map(ops_map);
    let db = DbStore::connect(&ops_env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");
    let mut state = AppState::with_components(
        ops_env,
        http.clone(),
        db,
        HttpDefenseShield::default(),
        reports_dir,
        None,
    );
    state.extension_hub = Arc::clone(&hub);
    let _router = build_router(state.clone());

    let snap = build_ops_telemetry_snapshot(&state).await;
    assert!(snap["osrm"].is_object());
    assert!(snap["osrm"].get("enabled").is_some());
    assert!(snap["osrm"].get("degradedReports").is_some());
    assert!(snap["osrm"].get("regions").is_some());
    assert_eq!(snap["osrm"]["fallbackPool"]["configuredFallbacks"], json!(4));
    assert_eq!(snap["osrm"]["fallbackPool"]["healthyFallbacks"], json!(2));

    let pool = snap["routingFallbackPool"]
        .as_array()
        .expect("routingFallbackPool array");
    assert_eq!(pool.len(), 5);
    let mbx_item = pool
        .iter()
        .find(|x| x["extensionId"] == "mapbox_routing")
        .unwrap();
    assert_eq!(mbx_item["configured"], json!(true));
    assert_eq!(mbx_item["inCooldown"], json!(true));
    assert!(mbx_item["requestsFailed"].as_u64().unwrap_or(0) >= 1);
    assert!(mbx_item["failovers"].as_u64().unwrap_or(0) >= 1);

    let serialized_snap = serde_json::to_string(&snap).unwrap();
    let serialized_warnings = serde_json::to_string(&m.warnings).unwrap();
    for secret in sentinel_secrets {
        assert!(
            !serialized_snap.contains(secret),
            "ops telemetry leaked secret {secret}"
        );
        assert!(
            !serialized_warnings.contains(secret),
            "warnings leaked secret {secret}"
        );
    }
    for raw_url in [&osrm_url, &mbx_url, &liq_url, &fr_url, &ors_url] {
        assert!(
            !serialized_snap.contains(raw_url),
            "ops telemetry leaked raw endpoint URL {raw_url}"
        );
    }
    let _ = std::fs::remove_dir_all(&temp_ops_dir);
}

// ------------------------------------------------------------------------------------------
// 6. Disabling a Routing Extension in `ExtensionHub` Excludes It Immediately
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn disabling_routing_extensions_in_hub_immediately_excludes_tier1_or_tier2_providers() {
    let _serial = TEST_SERIAL_LOCK.lock().await;
    let osrm_ok = MockProviderServer::new(RoutingProviderKind::Osrm, 200);
    let mbx_ok = MockProviderServer::new(RoutingProviderKind::Mapbox, 200);
    let fr_ok = MockProviderServer::new(RoutingProviderKind::FreeRoute, 200);

    let osrm_url = spawn_mock_provider(osrm_ok.clone()).await;
    let mbx_url = spawn_mock_provider(mbx_ok.clone()).await;
    let fr_url = spawn_mock_provider(fr_ok.clone()).await;

    let env = EnvConfig::from_map(HashMap::from([
        ("OSRM_BASE_URL".to_string(), osrm_url),
        ("ROUTING_FALLBACK_STRATEGY".to_string(), "round_robin".to_string()),
        ("MAPBOX_ACCESS_TOKEN".to_string(), "pk.toggle_mbx".to_string()),
        ("MAPBOX_BASE_URL".to_string(), mbx_url),
        ("FREEROUTE_API_KEY".to_string(), "fr_toggle_key".to_string()),
        ("FREEROUTE_BASE_URL".to_string(), fr_url),
    ]));
    let http = HttpClient::from_env(&env, None);
    let hub = ExtensionHub::new_default();

    // Disable `osrm_routing` AND `mapbox_routing` -> traffic must bypass healthy OSRM and disabled
    // Mapbox, routing 100% through `freeroute_routing`!
    hub.enable("osrm_routing", false).unwrap();
    hub.enable("mapbox_routing", false).unwrap();

    let client = RoutingClient::from_http(&http).with_extension_hub(Arc::clone(&hub));
    for i in 0..3 {
        let s = [LatLon::new(37.2900 + (i as f64) * 0.002, -121.9200)];
        let d = [LatLon::new(37.3100 + (i as f64) * 0.002, -121.9400)];
        let m = client.drive_matrix(&s, &d).await;
        assert_eq!(m.routed, 1);
        assert_eq!(
            m.attributions()[0].name,
            "FreeRoute Matrix API · © OpenStreetMap"
        );
    }
    assert_eq!(osrm_ok.hit_count(), 0, "disabled osrm_routing must not be called");
    assert_eq!(mbx_ok.hit_count(), 0, "disabled mapbox_routing must not be called");
    assert_eq!(fr_ok.hit_count(), 3);

    // Re-enable `mapbox_routing` -> immediately joins round-robin pool without restart!
    hub.enable("mapbox_routing", true).unwrap();
    for i in 0..2 {
        let s = [LatLon::new(37.3500 + (i as f64) * 0.002, -121.9500)];
        let d = [LatLon::new(37.3700 + (i as f64) * 0.002, -121.9700)];
        let _ = client.drive_matrix(&s, &d).await;
    }
    assert_eq!(mbx_ok.hit_count(), 1);
    assert_eq!(fr_ok.hit_count(), 4);
}

// ------------------------------------------------------------------------------------------
// 7. All Tier-1 + Tier-2 Down -> Haversine `RouterDown` Fallback & Honest `SourceItem` Citations
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn total_outage_falls_back_to_router_down_and_cloud_fallback_cites_honest_source() {
    let _serial = TEST_SERIAL_LOCK.lock().await;
    let osrm_down = MockProviderServer::new(RoutingProviderKind::Osrm, 503);
    let mbx_down = MockProviderServer::new(RoutingProviderKind::Mapbox, 503);
    let ors_var = MockProviderServer::new(RoutingProviderKind::OpenRouteService, 503);

    let osrm_url = spawn_mock_provider(osrm_down).await;
    let mbx_url = spawn_mock_provider(mbx_down).await;
    let ors_url = spawn_mock_provider(ors_var.clone()).await;

    let secret_ors = "ors_SENTINEL_OUTAGE_777";
    let env_all_down = EnvConfig::from_map(HashMap::from([
        ("OSRM_BASE_URL".to_string(), osrm_url.clone()),
        ("MAPBOX_ACCESS_TOKEN".to_string(), "pk.SENTINEL_OUTAGE_666".to_string()),
        ("MAPBOX_BASE_URL".to_string(), mbx_url),
        ("OPENROUTESERVICE_API_KEY".to_string(), secret_ors.to_string()),
        ("OPENROUTESERVICE_BASE_URL".to_string(), ors_url.clone()),
    ]));
    let http_down = HttpClient::from_env(&env_all_down, None);
    let hub = ExtensionHub::new_default();

    let geo = json!({ "lat": 37.2483, "lon": -121.8834, "city": "San Jose", "state": "CA" });
    let city_hall = json!({
        "lat": 37.33768,
        "lon": -121.88544,
        "name": "San Jose City Hall",
        "tags": { "name": "San Jose City Hall" }
    });

    // 7A. All Tier-1 + Tier-2 down -> Haversine `router_down` estimates + `report_has_osrm_router_down == true`
    let out_down = run_access_with_env_and_hub(
        &http_down,
        Some(&env_all_down),
        Some(Arc::clone(&hub)),
        37.2483,
        -121.8834,
        &geo,
        None,
        None,
        Some(&city_hall),
    )
    .await
    .expect("run_access never fails");

    let dest0 = &out_down.data["destinations"][0];
    assert_eq!(dest0["estimated"], json!(true));
    assert_eq!(dest0["estimateReason"], json!("router_down"));
    let report_down = json!({
        "modules": {
            "access": {
                "data": out_down.data,
                "warnings": out_down.warnings,
            }
        }
    });
    assert!(report_has_osrm_router_down(&report_down));
    let down_str = serde_json::to_string(&report_down).unwrap();
    assert!(!down_str.contains(secret_ors));
    assert!(!down_str.contains("pk.SENTINEL_OUTAGE_666"));

    // 7B. Flip OpenRouteService healthy (while OSRM remains 503) -> honest ORS attribution, NOT OSRM!
    ors_var.set_status(200);
    let env_ors_up = EnvConfig::from_map(HashMap::from([
        ("OSRM_BASE_URL".to_string(), osrm_url),
        ("OPENROUTESERVICE_API_KEY".to_string(), secret_ors.to_string()),
        ("OPENROUTESERVICE_BASE_URL".to_string(), ors_url),
        // Zero external cooldown on fresh env check isn't even needed once we clear or use fresh port,
        // so clear ORS cooldown from 7A to simulate recovery:
    ]));
    open_estate::modules::commute::clear_cloud_provider_cooldown(
        RoutingProviderKind::OpenRouteService,
        env_ors_up.openrouteservice_base_url().unwrap(),
    );
    let http_ors_up = HttpClient::from_env(&env_ors_up, None);
    let out_up = run_access_with_env_and_hub(
        &http_ors_up,
        Some(&env_ors_up),
        Some(hub),
        37.2483,
        -121.8834,
        &geo,
        None,
        None,
        Some(&city_hall),
    )
    .await
    .expect("run_access with ORS fallback");

    let up_dest0 = &out_up.data["destinations"][0];
    assert_eq!(up_dest0["estimated"], json!(false));
    assert!(up_dest0["estimateReason"].is_null());
    assert_eq!(out_up.sources.len(), 1);
    assert_eq!(
        out_up.sources[0].name,
        "openrouteservice.org by HeiGIT · © OpenStreetMap"
    );
    assert_ne!(out_up.sources[0].name, OSRM_ATTRIBUTION);
    assert!(
        !out_up.data["note"]
            .as_str()
            .unwrap_or("")
            .contains("self-hosted OSRM"),
        "note must not falsely claim self-hosted OSRM when cloud fallback answered: {}",
        out_up.data["note"]
    );

    let report_up = json!({
        "modules": {
            "access": {
                "data": out_up.data,
                "warnings": out_up.warnings,
            }
        },
        "sources": out_up.sources,
    });
    assert!(
        !report_has_osrm_router_down(&report_up),
        "report answered by Tier-2 fallback must NOT be flagged osrm_degraded"
    );

    // Verify all 5 attributions pass `scrub_internal_tech_words` unchanged!
    for kind in RoutingProviderKind::ALL {
        let item = kind.report_source_item();
        let raw = serde_json::to_string(&item).unwrap();
        assert!(
            !raw.to_ascii_lowercase().contains("provider")
                && !raw.to_ascii_lowercase().contains("effort"),
            "{raw}"
        );
        assert_eq!(
            scrub_internal_tech_words(&raw, "work", "service", "services"),
            raw,
            "{raw}"
        );
    }
}
