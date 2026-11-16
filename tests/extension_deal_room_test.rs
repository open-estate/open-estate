// Integration tests for:
// 1. Chrome / Edge / Brave Manifest V3 Extension bundle (`extension/`) & centralized `BRAND_CONFIG`
// 2. Fast Extension Summary API (`/api/extension/summary`) — cached 0ms HIT on synthetic reports,
//    CRE cap-rate mode, uncached 202 singleflight job enqueue, and WebExtension CORS preflight + origin enforcement
// 3. Side-by-Side "Deal Room" Comparison (`/api/deal-room/compare` & `/deal-room`) — multi-property
//    row winner calculation on synthetic property reports, boundary validation, XSS escaping, and per-user history isolation

mod common;

use axum::http::StatusCode;
use open_estate::db::DbStore;
use open_estate::guardrails::{HttpDefenseShield, SECURITY_RESPONSE_HEADERS};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use open_estate::web::summary::summarize_report;
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

struct TestServerHandle {
    base_url: String,
    host_header: String,
    state: AppState,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServerHandle {
    async fn start() -> Self {
        Self::start_with_env(Vec::new()).await
    }

    async fn start_with_env(extra_env: Vec<(&str, &str)>) -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-ext-dealroom-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        let http_cache_dir = temp_dir.join("http_cache");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
        std::fs::create_dir_all(&http_cache_dir).expect("create temp http cache dir");

        let db_path = temp_dir.join("test.sqlite");
        let mut overrides: Vec<(&str, String)> = vec![
            ("SQLITE_DB_PATH", db_path.to_string_lossy().into_owned()),
            ("SQLITE_PATH", db_path.to_string_lossy().into_owned()),
            ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
            ("SYNC_INTERVAL_MIN", "0".to_string()),
        ];
        for (k, v) in extra_env {
            overrides.push((k, v.to_string()));
        }
        let env = EnvConfig::from_process_and_dotenv(Some(Path::new("."))).with_overrides(overrides);

        let http = HttpClient::from_env(&env, Some(http_cache_dir));
        let db = DbStore::connect(&env).await.expect("connect test sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let state = AppState::with_components(
            env,
            http,
            db,
            HttpDefenseShield::default(),
            reports_dir,
            Some(temp_dir.join("datasets")),
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
        assert_eq!(
            actual, expected,
            "security header `{name}` mismatch: got `{actual}`, expected `{expected}`"
        );
    }
}

#[test]
fn test_extension_manifest_v3_and_portal_bundle_integrity() {
    let ext_dir = Path::new("extension");
    assert!(ext_dir.is_dir(), "extension/ directory must exist");

    let manifest_raw =
        std::fs::read_to_string(ext_dir.join("manifest.json")).expect("read manifest.json");
    let manifest: Value =
        serde_json::from_str(&manifest_raw).expect("manifest.json must be valid JSON");

    assert_eq!(manifest["manifest_version"], 3, "must be Manifest V3");
    let m_name = manifest["name"].as_str().unwrap_or("");
    assert!(
        m_name.contains("Open Estate"),
        "manifest name should identify Open Estate, got {m_name}"
    );
    assert!(ext_dir.join("vendor/i18next.min.js").is_file());
    assert!(ext_dir.join("i18n.js").is_file());
    assert!(ext_dir.join("locales/en.json").is_file());
    assert!(ext_dir.join("locales/zh-CN.json").is_file());

    let perms: Vec<&str> = manifest["permissions"]
        .as_array()
        .expect("permissions array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        perms,
        vec!["storage", "contextMenus"],
        "extension must request minimal least-privilege permissions only"
    );

    let host_perms: Vec<&str> = manifest["host_permissions"]
        .as_array()
        .expect("host_permissions array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for required_local in ["http://127.0.0.1/*", "http://localhost/*"] {
        assert!(
            host_perms.contains(&required_local),
            "host_permissions missing local self-hosted backend `{required_local}`"
        );
    }
    assert!(
        !host_perms.contains(&"<all_urls>"),
        "must not request <all_urls>"
    );
    assert!(
        manifest.get("content_scripts").is_none(),
        "ToS-clean companion extension must not inject portal content_scripts"
    );

    assert_eq!(
        manifest["background"]["service_worker"].as_str(),
        Some("background.js")
    );
    assert_eq!(
        manifest["action"]["default_popup"].as_str(),
        Some("popup.html")
    );

    for rel in [
        "brand.js",
        "background.js",
        "i18n.js",
        "popup.html",
        "popup.js",
        "README.md",
        "icons/icon.svg",
        "locales/en.json",
        "locales/zh-CN.json",
        "locales/es.json",
        "locales/resources.js",
    ] {
        let p = ext_dir.join(rel);
        let meta = std::fs::metadata(&p).unwrap_or_else(|_| panic!("missing extension/{rel}"));
        assert!(meta.len() > 100, "extension/{rel} should not be empty");
    }

    let ext_en: Value = serde_json::from_str(
        &std::fs::read_to_string(ext_dir.join("locales/en.json")).expect("read ext en.json"),
    )
    .expect("parse ext en.json");
    let ext_zh: Value = serde_json::from_str(
        &std::fs::read_to_string(ext_dir.join("locales/zh-CN.json")).expect("read ext zh-CN.json"),
    )
    .expect("parse ext zh-CN.json");
    let ext_es: Value = serde_json::from_str(
        &std::fs::read_to_string(ext_dir.join("locales/es.json")).expect("read ext es.json"),
    )
    .expect("parse ext es.json");
    assert_eq!(ext_en["overlay"]["fengShuiLabel"], "堪舆朝向");
    assert_eq!(ext_zh["overlay"]["fengShuiLabel"], "堪舆朝向");
    assert_eq!(ext_es["overlay"]["fengShuiLabel"], "堪舆朝向");
    for section in ["popup", "overlay"] {
        let en_keys: std::collections::BTreeSet<_> =
            ext_en[section].as_object().unwrap().keys().collect();
        let zh_keys: std::collections::BTreeSet<_> =
            ext_zh[section].as_object().unwrap().keys().collect();
        let es_keys: std::collections::BTreeSet<_> =
            ext_es[section].as_object().unwrap().keys().collect();
        assert_eq!(
            en_keys, zh_keys,
            "extension locales {section} key parity en vs zh-CN"
        );
        assert_eq!(
            en_keys, es_keys,
            "extension locales {section} key parity en vs es"
        );
    }

    for sz in ["16", "32", "48", "128"] {
        let icon_rel = manifest["icons"][sz]
            .as_str()
            .unwrap_or_else(|| panic!("manifest icons.{sz} missing"));
        let bytes =
            std::fs::read(ext_dir.join(icon_rel)).unwrap_or_else(|_| panic!("read {icon_rel}"));
        assert!(
            bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "{icon_rel} must have valid PNG magic bytes"
        );
    }

    let brand_js = std::fs::read_to_string(ext_dir.join("brand.js")).expect("read brand.js");
    assert!(brand_js.contains("BRAND_CONFIG"));
    assert!(brand_js.contains("Open Estate"));
    assert!(brand_js.contains("http://localhost:8080"));

    let bg_js = std::fs::read_to_string(ext_dir.join("background.js")).expect("read background.js");
    for token in [
        "/api/extension/summary",
        "/api/report/enqueue",
        "/api/deal-room/compare",
        "contextMenus",
    ] {
        assert!(
            bg_js.contains(token),
            "background.js missing expected companion feature token `{token}`"
        );
    }
}

#[tokio::test]
async fn test_extension_summary_cached_uncached_cre_and_cors_preflight() {
    let srv = TestServerHandle::start_with_env(vec![(
        "ALLOWED_EXTENSION_IDS",
        "abcdefghijklmnopabcdefghijklmnop",
    )])
    .await;
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .build()
        .expect("reqwest client");

    let ext_origin = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
    let slug = common::SYNTH_PRIMARY_SLUG;
    let address = common::SYNTH_PRIMARY_ADDRESS;
    let rep = common::synth_report_primary();
    let expected_overall = rep["summary"]["scores"]["overall"].as_i64().unwrap();
    let html = render_report_html(&rep);
    srv.state
        .db
        .put_cached_report(slug, &rep, Some(&html))
        .await
        .expect("seed synthetic cached report");

    // 1. CORS Preflight `OPTIONS /api/extension/summary` from Chrome Extension origin
    let opt_resp = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/api/extension/summary", srv.base_url),
        )
        .header("Origin", ext_origin)
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "Content-Type, Authorization, X-OpenEstate-Client-Id",
        )
        .send()
        .await
        .expect("send OPTIONS preflight");

    assert_eq!(opt_resp.status(), StatusCode::NO_CONTENT);
    assert_security_headers(opt_resp.headers());
    assert_eq!(
        opt_resp
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(ext_origin)
    );
    let allow_methods = opt_resp
        .headers()
        .get("access-control-allow-methods")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(allow_methods.contains("POST") && allow_methods.contains("GET"));

    // 2. Instant cached summary HIT via `POST /api/extension/summary` with asking price below synthetic fair value ($1,815,000)
    let hit_resp = client
        .post(format!("{}/api/extension/summary", srv.base_url))
        .header("Origin", ext_origin)
        .json(&json!({
            "address": address,
            "askingPrice": 1_650_000,
            "portal": "companion"
        }))
        .send()
        .await
        .expect("POST /api/extension/summary cached");

    assert_eq!(hit_resp.status(), StatusCode::OK);
    assert_security_headers(hit_resp.headers());
    assert_eq!(
        hit_resp
            .headers()
            .get("x-openestate-cache")
            .and_then(|v| v.to_str().ok()),
        Some("HIT")
    );
    assert_eq!(
        hit_resp
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(ext_origin)
    );

    let hit_body: Value = hit_resp.json().await.expect("parse summary JSON");
    assert_eq!(hit_body["ok"], true);
    assert_eq!(hit_body["status"], "ready");
    assert_eq!(hit_body["cached"], true);
    assert_eq!(hit_body["slug"], slug);
    assert_eq!(hit_body["overallScore"], expected_overall);
    assert_eq!(hit_body["fairValue"]["mid"], 2_005_000);
    assert_eq!(hit_body["askingDelta"]["askingPrice"], 1_650_000);
    assert_eq!(hit_body["askingDelta"]["tone"], "good");
    assert!(
        hit_body["askingDelta"]["deltaPct"].as_f64().unwrap_or(0.0) < 0.0,
        "asking $1.65M vs synthetic fair value $1.815M should report negative asking premium"
    );
    assert!(
        hit_body["trueMonthlyCost"]["total"].as_i64().unwrap_or(0) > 8_000,
        "expected monthly cost > $8,000, got {}",
        hit_body["trueMonthlyCost"]["total"]
    );
    assert!(hit_body["trueMonthlyCost"]["reassessmentNoteEn"]
        .as_str()
        .unwrap_or("")
        .contains("Prop 13"));
    assert_eq!(hit_body["fengShui"]["available"], true);
    assert!(hit_body["fengShui"]["pillEn"]
        .as_str()
        .unwrap_or("")
        .contains("Facing South"));
    assert!(hit_body["fengShui"]["pillZh"]
        .as_str()
        .unwrap_or("")
        .contains("坐N向S"));
    assert!(hit_body["urls"]["inspectionUrl"]
        .as_str()
        .unwrap_or("")
        .ends_with("#inspection"));
    assert!(hit_body["urls"]["dealRoomUrl"]
        .as_str()
        .unwrap_or("")
        .contains("/deal-room?slugs="));
    assert!(hit_body["zhLabels"]["saveDealRoom"]
        .as_str()
        .unwrap_or("")
        .contains("比房室"));
    assert!(hit_body["esLabels"]["saveDealRoom"]
        .as_str()
        .unwrap_or("")
        .contains("Deal Room"));
    assert!(hit_body["trueMonthlyCost"]["reassessmentNoteEs"]
        .as_str()
        .unwrap_or("")
        .contains("Prop 13"));

    // 3. Commercial (CRE) mode via `GET /api/extension/summary`
    let cre_resp = client
        .get(format!(
            "{}/api/extension/summary?slug={}&portal=cre&isCommercial=true&askingPrice=2800000&noi=182000&assetType=Retail%20Strip",
            srv.base_url, slug
        ))
        .header("Origin", ext_origin)
        .send()
        .await
        .expect("GET CRE summary");
    assert_eq!(cre_resp.status(), StatusCode::OK);
    let cre_body: Value = cre_resp.json().await.expect("parse CRE JSON");
    assert_eq!(cre_body["isCommercial"], true);
    assert_eq!(cre_body["creMetrics"]["capRatePct"], 6.5);
    assert!(cre_body["creMetrics"]["badgeEn"]
        .as_str()
        .unwrap_or("")
        .contains("Retail Strip · 6.50% Cap"));
    assert!(cre_body["creMetrics"]["badgeEs"]
        .as_str()
        .unwrap_or("")
        .contains("Activo comercial (Retail Strip) · 6.50% Cap"));

    // 4. Synthetic uncached address -> `202 Accepted` singleflight background build (`status: "building"`)
    let uncached_addr = "410 Synthcoralgables Blvd, Coral Gables, FL 33134";
    let build_resp_1 = client
        .post(format!("{}/api/extension/summary", srv.base_url))
        .header("Origin", ext_origin)
        .json(&json!({
            "address": uncached_addr,
            "ai": false
        }))
        .send()
        .await
        .expect("POST uncached extension summary #1");
    assert_eq!(build_resp_1.status(), StatusCode::ACCEPTED);
    assert_security_headers(build_resp_1.headers());
    let build_body_1: Value = build_resp_1.json().await.expect("parse 202 JSON #1");
    assert_eq!(build_body_1["ok"], true);
    assert_eq!(build_body_1["status"], "building");
    assert_eq!(build_body_1["cached"], false);
    let job_id_1 = build_body_1["jobId"]
        .as_str()
        .expect("non-empty jobId")
        .to_string();
    assert!(!job_id_1.is_empty());
    assert!(build_body_1["pollUrl"]
        .as_str()
        .unwrap_or("")
        .contains(&format!("/api/job/{job_id_1}")));

    // Concurrent singleflight requests for a fresh uncached address deduplicate onto the same active job
    let sf_uncached_addr = "412 Synthcoralgables Blvd, Coral Gables, FL 33134";
    let (sf_1, sf_2) = tokio::join!(
        client
            .post(format!("{}/api/extension/summary", srv.base_url))
            .header("Origin", ext_origin)
            .json(&json!({ "address": sf_uncached_addr, "ai": false }))
            .send(),
        client
            .post(format!("{}/api/extension/summary", srv.base_url))
            .header("Origin", ext_origin)
            .json(&json!({ "address": sf_uncached_addr, "ai": false }))
            .send(),
    );
    let sf_1 = sf_1.expect("concurrent singleflight #1");
    let sf_2 = sf_2.expect("concurrent singleflight #2");
    assert!(sf_1.status() == StatusCode::ACCEPTED || sf_1.status() == StatusCode::OK);
    assert!(sf_2.status() == StatusCode::ACCEPTED || sf_2.status() == StatusCode::OK);
    if sf_1.status() == StatusCode::ACCEPTED && sf_2.status() == StatusCode::ACCEPTED {
        let sf_json_1: Value = sf_1.json().await.expect("sf json #1");
        let sf_json_2: Value = sf_2.json().await.expect("sf json #2");
        assert_eq!(sf_json_1["jobId"], sf_json_2["jobId"]);
    }

    // 5. `POST /api/report/enqueue` allows `chrome-extension://` origin while blocking hostile web origins
    let enq_ext = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Origin", ext_origin)
        .json(&json!({ "address": address, "ai": false }))
        .send()
        .await
        .expect("extension enqueue");
    assert_eq!(enq_ext.status(), StatusCode::OK);
    assert_eq!(
        enq_ext
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(ext_origin)
    );

    let enq_evil = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Origin", "https://attacker.evil.example")
        .json(&json!({ "address": address, "ai": false }))
        .send()
        .await
        .expect("evil cross-origin enqueue");
    assert_eq!(enq_evil.status(), StatusCode::FORBIDDEN);
    let evil_json: Value = enq_evil.json().await.expect("evil error json");
    assert_eq!(evil_json["code"], "CROSS_ORIGIN_BLOCKED");

    // Un-allowlisted extension origin must NOT receive CORS headers or bypass same-origin
    let unallowed_ext = "chrome-extension://zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
    let unallowed_enq = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Origin", unallowed_ext)
        .json(&json!({ "address": address, "ai": false }))
        .send()
        .await
        .expect("un-allowlisted extension enqueue");
    assert_eq!(unallowed_enq.status(), StatusCode::FORBIDDEN);
    assert!(unallowed_enq
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    // 6. Extension CORS must NEVER reflect arbitrary web origins or emit `*` when `Origin` is omitted
    let evil_origin = "https://attacker.evil.example";
    let opt_evil_sum = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/api/extension/summary", srv.base_url),
        )
        .header("Origin", evil_origin)
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .expect("OPTIONS evil origin /api/extension/summary");
    assert_eq!(opt_evil_sum.status(), StatusCode::NO_CONTENT);
    assert!(opt_evil_sum
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    let post_evil_sum = client
        .post(format!("{}/api/extension/summary", srv.base_url))
        .header("Origin", evil_origin)
        .json(&json!({ "address": address }))
        .send()
        .await
        .expect("POST evil origin /api/extension/summary");
    assert_eq!(post_evil_sum.status(), StatusCode::OK);
    assert!(post_evil_sum
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    let post_no_origin_sum = client
        .post(format!("{}/api/extension/summary", srv.base_url))
        .json(&json!({ "address": address }))
        .send()
        .await
        .expect("POST no origin /api/extension/summary");
    assert_eq!(post_no_origin_sum.status(), StatusCode::OK);
    assert!(post_no_origin_sum
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    let opt_evil_dr = client
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/api/deal-room/compare", srv.base_url),
        )
        .header("Origin", evil_origin)
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .expect("OPTIONS evil origin /api/deal-room/compare");
    assert_eq!(opt_evil_dr.status(), StatusCode::NO_CONTENT);
    assert!(opt_evil_dr
        .headers()
        .get("access-control-allow-origin")
        .is_none());
}

#[tokio::test]
async fn test_deal_room_side_by_side_comparison_winners_xss_and_print_dossier() {
    let srv = TestServerHandle::start_with_env(vec![(
        "ALLOWED_EXTENSION_IDS",
        "abcdefghijklmnopabcdefghijklmnop",
    )])
    .await;
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .expect("reqwest client");

    let slug_primary = common::SYNTH_PRIMARY_SLUG;
    let rep_primary = common::synth_report_primary();

    let slug_secondary = common::SYNTH_SECONDARY_SLUG;
    let mut rep_secondary = common::synth_report_secondary();
    rep_secondary["modules"]["environment"]["data"]["scores"]["overall"] = json!(62);
    rep_secondary["summary"] = summarize_report(&rep_secondary);
    normalize_stored_report(&mut rep_secondary);

    let mut rep_third = common::synth_report_from_holdout("synth-ca-95118-mid-sfr");
    let slug_third = rep_third["meta"]["slug"].as_str().unwrap().to_string();
    rep_third["summary"]["verdict"] =
        json!("Santa Clara SFR <script>alert('xss')</script> <img src=x onerror=alert(1)>");

    for (s, r) in [
        (slug_primary, &rep_primary),
        (slug_secondary, &rep_secondary),
        (slug_third.as_str(), &rep_third),
    ] {
        let html = render_report_html(r);
        srv.state
            .db
            .put_cached_report(s, r, Some(&html))
            .await
            .expect("seed synthetic deal room report");
    }

    // 1. JSON Deal Room Comparison (`POST /api/deal-room/compare`)
    let cmp_resp = client
        .post(format!("{}/api/deal-room/compare", srv.base_url))
        .header(
            "Origin",
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
        )
        .json(&json!({
            "slugs": [slug_primary, slug_secondary]
        }))
        .send()
        .await
        .expect("POST /api/deal-room/compare");

    assert_eq!(cmp_resp.status(), StatusCode::OK);
    assert_security_headers(cmp_resp.headers());
    let cmp_json: Value = cmp_resp.json().await.expect("parse compare JSON");
    assert_eq!(cmp_json["ok"], true);
    assert_eq!(cmp_json["count"], 2);

    let winners = &cmp_json["winners"];
    assert_eq!(winners["aiVerdict"]["winnerSlug"], slug_primary);
    assert_eq!(winners["lowestMonthlyCost"]["winnerSlug"], slug_primary);
    assert_eq!(winners["safestNaturalHazards"]["winnerSlug"], slug_primary);
    assert_eq!(winners["cleanestEnvironment"]["winnerSlug"], slug_primary);
    assert_eq!(winners["fengShui"]["winnerSlug"], slug_primary);

    // 2. Boundary validation: fewer than 2 slugs or more than 10 slugs -> `400 Bad Request`
    let single_resp = client
        .post(format!("{}/api/deal-room/compare", srv.base_url))
        .json(&json!({ "slugs": [slug_primary] }))
        .send()
        .await
        .expect("single slug compare");
    assert_eq!(single_resp.status(), StatusCode::BAD_REQUEST);
    let single_err: Value = single_resp.json().await.expect("single err");
    assert_eq!(single_err["code"], "INVALID_DEAL_ROOM_COUNT");

    let eleven_slugs: Vec<String> = (1..=11).map(|i| format!("slug-bound-check-{i}")).collect();
    let too_many_resp = client
        .post(format!("{}/api/deal-room/compare", srv.base_url))
        .json(&json!({ "slugs": eleven_slugs }))
        .send()
        .await
        .expect("eleven slugs compare");
    assert_eq!(too_many_resp.status(), StatusCode::BAD_REQUEST);

    // 3. Interactive HTML Deal Room Matrix (`GET /deal-room?slugs=...`) + HTML XSS Escaping
    let html_resp = client
        .get(format!(
            "{}/deal-room?slugs={},{},{}",
            srv.base_url, slug_primary, slug_secondary, slug_third
        ))
        .send()
        .await
        .expect("GET /deal-room HTML");

    assert_eq!(html_resp.status(), StatusCode::OK);
    assert_security_headers(html_resp.headers());
    let deal_html = html_resp.text().await.expect("read deal-room HTML");

    assert!(deal_html.contains("Deal Room"));
    assert!(deal_html.contains("🏆 "));
    assert!(deal_html.contains("csvBtn"));
    assert!(deal_html.contains("printBtn"));
    assert!(deal_html.contains("100 Synthparcelalpha Ln"));
    assert!(deal_html.contains("200 Synthparcelbeta Ave"));
    assert!(deal_html.contains("420 Synthcohort03 St"));

    assert!(
        !deal_html.contains("<script>alert('xss')</script>"),
        "raw <script> tag must never appear unescaped in Deal Room HTML"
    );
    assert!(
        !deal_html.contains("<img src=x onerror=alert(1)>"),
        "raw <img onerror> payload must never appear unescaped in Deal Room HTML"
    );
    assert!(
        deal_html.contains("&lt;script&gt;alert("),
        "escaped text should remain visible safely"
    );

    let list_resp = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .send()
        .await
        .expect("GET /api/deal-room");
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json: Value = list_resp.json().await.expect("parse /api/deal-room");
    assert_eq!(list_json["availableCount"].as_u64(), Some(0));

    let scoped_ext_id = "ext-dealroom-scoped-1234";
    let ext_pick = client
        .get(format!(
            "{}/api/extension/summary?slug={}",
            srv.base_url, slug_primary
        ))
        .header("X-OpenEstate-Client-Id", scoped_ext_id)
        .send()
        .await
        .expect("GET /api/extension/summary for slug_primary");
    assert_eq!(ext_pick.status(), StatusCode::OK);

    let scoped_list = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header("X-OpenEstate-Client-Id", scoped_ext_id)
        .send()
        .await
        .expect("GET /api/deal-room with X-OpenEstate-Client-Id");
    assert_eq!(scoped_list.status(), StatusCode::OK);
    let scoped_json: Value = scoped_list
        .json()
        .await
        .expect("parse scoped /api/deal-room");
    assert_eq!(scoped_json["availableCount"].as_u64(), Some(1));
    assert_eq!(scoped_json["available"][0]["slug"], slug_primary);

    let zh_html = client
        .get(format!(
            "{}/deal-room?slugs={},{}&lang=zh",
            srv.base_url, slug_primary, slug_secondary
        ))
        .send()
        .await
        .expect("GET /deal-room lang=zh")
        .text()
        .await
        .expect("zh HTML");
    assert!(zh_html.contains("lang=\"zh-CN\""));
    assert!(zh_html.contains("比房室") || zh_html.contains("综合评分冠军"));

    let hostile_slug_resp = client
        .get(format!(
            "{}/deal-room?slugs=<script>alert(1)</script>",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET hostile slug");
    assert_eq!(hostile_slug_resp.status(), StatusCode::BAD_REQUEST);

    // 4. Verify rendered single-property report includes Deal Room & Executive Print/PDF toolbar UX
    let rep_page = client
        .get(format!("{}/r/{}/", srv.base_url, slug_primary))
        .send()
        .await
        .expect("GET rendered report HTML");
    assert_eq!(rep_page.status(), StatusCode::OK);
    let rep_html = rep_page.text().await.expect("report HTML body");
    assert!(rep_html.contains("id=\"dealRoomBtn\""));
    assert!(rep_html.contains("Compare in Deal Room"));
    assert!(rep_html.contains("id=\"printPdfBtn\""));
    assert!(rep_html.contains("Print / Save PDF"));
    assert!(rep_html.contains("id=\"printDossierHdr\""));
    assert!(rep_html.contains("@page{size:letter portrait"));
}

#[tokio::test]
async fn test_extension_summary_report_schema_and_edge_cases() {
    let srv = TestServerHandle::start_with_env(vec![(
        "ALLOWED_EXTENSION_IDS",
        "12345678-1234-1234-1234-123456789abc",
    )])
    .await;
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .expect("reqwest client");

    let moz_origin = "moz-extension://12345678-1234-1234-1234-123456789abc";

    let slug = common::SYNTH_SECONDARY_SLUG;
    let mut synth_sec = common::synth_report_secondary();
    synth_sec["meta"]["aiPending"] = json!(true);
    let html = render_report_html(&synth_sec);
    srv.state
        .db
        .put_cached_report(slug, &synth_sec, Some(&html))
        .await
        .expect("seed synthetic secondary report");

    let hit = client
        .get(format!(
            "{}/api/extension/summary?slug={}&ai=false",
            srv.base_url, slug
        ))
        .header("Origin", moz_origin)
        .send()
        .await
        .expect("GET moz-extension summary");
    assert_eq!(hit.status(), StatusCode::OK);
    assert_eq!(
        hit.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some(moz_origin)
    );
    let body: Value = hit.json().await.expect("json");
    assert_eq!(body["aiPending"], true);
    let pi = body["trueMonthlyCost"]["pi"].as_i64().unwrap_or(0);
    let total = body["trueMonthlyCost"]["total"].as_i64().unwrap_or(0);
    assert!(pi > 9_000, "expected P&I > $9,000, got {pi}");
    assert!(total > pi, "expected total monthly cost ({total}) > P&I ({pi})");
    assert!(body["trueMonthlyCost"]["reassessmentNoteEn"]
        .as_str()
        .unwrap_or("")
        .contains("Prop 13"));

    let missing_req = client
        .get(format!("{}/api/extension/summary", srv.base_url))
        .send()
        .await
        .expect("GET missing address");
    assert_eq!(missing_req.status(), StatusCode::BAD_REQUEST);

    let nf_slug = client
        .get(format!(
            "{}/api/extension/summary?slug=200-synthparcelbeta-uncached-00000",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET nonexistent slug");
    assert_eq!(nf_slug.status(), StatusCode::NOT_FOUND);

    let no_enq = client
        .post(format!("{}/api/extension/summary", srv.base_url))
        .json(&json!({
            "address": "420 Synthrowhouse St, Philadelphia, PA 19147",
            "enqueue": false
        }))
        .send()
        .await
        .expect("POST enqueue=false");
    assert_eq!(no_enq.status(), StatusCode::ACCEPTED);
    let no_enq_json: Value = no_enq.json().await.expect("no_enq json");
    assert_eq!(no_enq_json["status"], "uncached");

    let miss_cmp = client
        .post(format!("{}/api/deal-room/compare", srv.base_url))
        .json(&json!({ "slugs": ["missing-one-123", "missing-two-456"] }))
        .send()
        .await
        .expect("POST missing deal room slugs");
    assert_eq!(miss_cmp.status(), StatusCode::NOT_FOUND);
    let miss_cmp_json: Value = miss_cmp.json().await.expect("miss_cmp json");
    assert_eq!(miss_cmp_json["code"], "DEAL_ROOM_REPORTS_MISSING");

    for js_file in ["background.js", "popup.js"] {
        let txt = std::fs::read_to_string(Path::new("extension").join(js_file))
            .unwrap_or_else(|_| panic!("read extension/{js_file}"));
        assert!(
            !open_estate::shared::has_retired_brand(&txt)
                && !txt.contains("Open Estate")
                && !txt.contains("慧居智研"),
            "extension/{js_file} must not hardcode brand strings outside BRAND_CONFIG in brand.js"
        );
    }
}

fn extract_set_cookie_pair(resp: &reqwest::Response, name: &str) -> Option<String> {
    for val in resp.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(s) = val.to_str() {
            if let Some(kv) = s.split(';').next() {
                let kv = kv.trim();
                if let Some((k, _)) = kv.split_once('=') {
                    if k.trim() == name {
                        return Some(kv.to_string());
                    }
                }
            }
        }
    }
    None
}

#[tokio::test]
async fn test_deal_room_quick_add_per_user_search_history_isolation() {
    let srv = TestServerHandle::start().await;
    let client = Client::builder().no_proxy().build().expect("client");

    let slug_alice_1 = common::SYNTH_PRIMARY_SLUG.to_string();
    let addr_alice_1 = common::SYNTH_PRIMARY_ADDRESS.to_string();
    let rep_alice_1 = common::synth_report_primary();

    let slug_alice_2 = common::SYNTH_SECONDARY_SLUG.to_string();
    let addr_alice_2 = common::SYNTH_SECONDARY_ADDRESS.to_string();
    let rep_alice_2 = common::synth_report_secondary();

    let rep_bob_1 = common::synth_report_from_holdout("synth-il-60201-vintage-sfr");
    let slug_bob_1 = rep_bob_1["meta"]["slug"].as_str().unwrap().to_string();
    let addr_bob_1 = rep_bob_1["meta"]["displayAddress"].as_str().unwrap().to_string();
    let rep_anon_1 = common::synth_report_from_holdout("synth-wa-98103-adu-sfr");
    let slug_anon_1 = rep_anon_1["meta"]["slug"].as_str().unwrap().to_string();
    let addr_anon_1 = rep_anon_1["meta"]["displayAddress"].as_str().unwrap().to_string();

    for (slug, rep) in [
        (&slug_alice_1, &rep_alice_1),
        (&slug_alice_2, &rep_alice_2),
        (&slug_bob_1, &rep_bob_1),
        (&slug_anon_1, &rep_anon_1),
    ] {
        let html = render_report_html(rep);
        srv.state
            .db
            .put_cached_report(slug, rep, Some(&html))
            .await
            .expect("seed synthetic cached report");
    }

    let empty_api = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .send()
        .await
        .expect("GET /api/deal-room unauthenticated");
    assert_eq!(empty_api.status(), StatusCode::OK);
    assert_eq!(
        empty_api
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("private, no-store")
    );
    let empty_json: Value = empty_api.json().await.expect("parse empty /api/deal-room");
    assert_eq!(empty_json["availableCount"].as_u64(), Some(0));

    let empty_html_resp = client
        .get(format!("{}/deal-room?lang=zh", srv.base_url))
        .send()
        .await
        .expect("GET /deal-room?lang=zh unauthenticated");
    assert_eq!(empty_html_resp.status(), StatusCode::OK);
    assert_eq!(
        empty_html_resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("private, no-store")
    );
    let empty_html = empty_html_resp.text().await.expect("empty deal-room html");
    assert!(
        !empty_html.contains(&addr_alice_1)
            && !empty_html.contains(&addr_alice_2)
            && !empty_html.contains(&addr_bob_1)
            && !empty_html.contains(&addr_anon_1),
        "Fresh /deal-room must never expose globally cached reports of other users"
    );

    let home_resp = client
        .get(format!("{}/", srv.base_url))
        .send()
        .await
        .expect("GET /");
    assert_eq!(home_resp.status(), StatusCode::OK);
    let anon_vid_cookie = extract_set_cookie_pair(&home_resp, "open_estate_vid")
        .expect("GET / must mint open_estate_vid");

    let anon_view = client
        .get(format!("{}/r/{}/", srv.base_url, slug_anon_1))
        .header(reqwest::header::COOKIE, &anon_vid_cookie)
        .send()
        .await
        .expect("anon GET /r/anon_1/");
    assert_eq!(anon_view.status(), StatusCode::OK);

    let alice_home = client
        .get(format!("{}/", srv.base_url))
        .send()
        .await
        .expect("Alice GET /");
    let alice_session_cookie =
        extract_set_cookie_pair(&alice_home, "open_estate_vid").expect("Alice vid cookie");
    let bob_home = client
        .get(format!("{}/", srv.base_url))
        .send()
        .await
        .expect("Bob GET /");
    let bob_session_cookie =
        extract_set_cookie_pair(&bob_home, "open_estate_vid").expect("Bob vid cookie");

    let alice_enq = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Host", &srv.host_header)
        .header("Origin", &srv.base_url)
        .header(
            reqwest::header::COOKIE,
            format!("{alice_session_cookie}; {anon_vid_cookie}"),
        )
        .json(&json!({ "address": addr_alice_1 }))
        .send()
        .await
        .expect("Alice enqueue alice_1");
    assert!(alice_enq.status() == StatusCode::OK || alice_enq.status() == StatusCode::ACCEPTED);

    let alice_view_2 = client
        .get(format!("{}/r/{}/", srv.base_url, slug_alice_2))
        .header(reqwest::header::COOKIE, &alice_session_cookie)
        .send()
        .await
        .expect("Alice view alice_2");
    assert_eq!(alice_view_2.status(), StatusCode::OK);

    let bob_view = client
        .get(format!("{}/r/{}/", srv.base_url, slug_bob_1))
        .header(
            reqwest::header::COOKIE,
            format!("{bob_session_cookie}; {anon_vid_cookie}"),
        )
        .send()
        .await
        .expect("Bob view bob_1");
    assert_eq!(bob_view.status(), StatusCode::OK);

    let alice_list: Value = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header(reqwest::header::COOKIE, &alice_session_cookie)
        .send()
        .await
        .expect("Alice /api/deal-room")
        .json()
        .await
        .expect("Alice list json");
    assert_eq!(alice_list["availableCount"].as_u64(), Some(2));
    let alice_slugs: Vec<&str> = alice_list["available"]
        .as_array()
        .expect("available array")
        .iter()
        .filter_map(|v| v["slug"].as_str())
        .collect();
    assert!(alice_slugs.contains(&slug_alice_1.as_str()));
    assert!(alice_slugs.contains(&slug_alice_2.as_str()));
    assert!(!alice_slugs.contains(&slug_bob_1.as_str()));
    assert!(!alice_slugs.contains(&slug_anon_1.as_str()));

    let alice_html = client
        .get(format!(
            "{}/deal-room?slugs={}&lang=zh",
            srv.base_url, slug_alice_1
        ))
        .header(reqwest::header::COOKIE, &alice_session_cookie)
        .send()
        .await
        .expect("Alice /deal-room?lang=zh")
        .text()
        .await
        .expect("Alice deal-room html");
    assert!(alice_html.contains("一键加入已分析房产对比"));
    assert!(alice_html.contains("const allowLocalRecent = true;"));
    assert!(alice_html.contains(&format!("data-slug=\"{slug_alice_2}\"")));
    assert!(
        !alice_html.contains(&addr_bob_1) && !alice_html.contains(&addr_anon_1),
        "Alice's Deal Room must never include Bob's or anonymous browser searches"
    );

    let bob_html = client
        .get(format!("{}/deal-room?lang=zh", srv.base_url))
        .header(
            reqwest::header::COOKIE,
            format!("{bob_session_cookie}; {anon_vid_cookie}"),
        )
        .send()
        .await
        .expect("Bob /deal-room?lang=zh")
        .text()
        .await
        .expect("Bob deal-room html");
    assert!(bob_html.contains("一键加入已分析房产对比"));
    assert!(bob_html.contains(&addr_bob_1));
    assert!(
        !bob_html.contains(&addr_alice_1)
            && !bob_html.contains(&addr_alice_2)
            && !bob_html.contains(&addr_anon_1),
        "Bob's Deal Room must never include Alice's or anonymous searches"
    );

    let anon_html = client
        .get(format!("{}/deal-room?lang=zh", srv.base_url))
        .header(reqwest::header::COOKIE, &anon_vid_cookie)
        .send()
        .await
        .expect("Anon /deal-room?lang=zh")
        .text()
        .await
        .expect("Anon deal-room html");
    assert!(anon_html.contains(&addr_anon_1));
    assert!(
        !anon_html.contains(&addr_alice_1)
            && !anon_html.contains(&addr_alice_2)
            && !anon_html.contains(&addr_bob_1),
        "Anonymous visitor must never see Alice's or Bob's searches"
    );

    let ext_c1 = "ext-client-isolation-111";
    let ext_c2 = "ext-client-isolation-222";
    let ext_pick_b = client
        .get(format!(
            "{}/api/extension/summary?slug={}",
            srv.base_url, slug_bob_1
        ))
        .header("X-OpenEstate-Client-Id", ext_c1)
        .send()
        .await
        .expect("ext_c1 summary");
    assert_eq!(ext_pick_b.status(), StatusCode::OK);

    let ext1_list: Value = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header("X-OpenEstate-Client-Id", ext_c1)
        .send()
        .await
        .expect("ext_c1 deal room")
        .json()
        .await
        .expect("ext_c1 json");
    assert_eq!(ext1_list["availableCount"].as_u64(), Some(1));
    assert_eq!(ext1_list["available"][0]["slug"], slug_bob_1);

    let ext2_list: Value = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header("X-OpenEstate-Client-Id", ext_c2)
        .send()
        .await
        .expect("ext_c2 deal room")
        .json()
        .await
        .expect("ext_c2 json");
    assert_eq!(ext2_list["availableCount"].as_u64(), Some(0));

    let rm_resp = client
        .post(format!("{}/api/deal-room/history/remove", srv.base_url))
        .header("Host", &srv.host_header)
        .header("Origin", &srv.base_url)
        .header(reqwest::header::COOKIE, &alice_session_cookie)
        .json(&json!({ "slug": slug_alice_1 }))
        .send()
        .await
        .expect("Alice remove slug_alice_1");
    assert_eq!(rm_resp.status(), StatusCode::OK);
    let rm_json: Value = rm_resp.json().await.expect("remove json");
    assert_eq!(rm_json["ok"], true);

    let alice_after_rm: Value = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header(reqwest::header::COOKIE, &alice_session_cookie)
        .send()
        .await
        .expect("Alice list after remove")
        .json()
        .await
        .expect("Alice after remove json");
    assert_eq!(alice_after_rm["availableCount"].as_u64(), Some(1));
    assert_eq!(alice_after_rm["available"][0]["slug"], slug_alice_2);

    let clr_resp = client
        .post(format!("{}/api/deal-room/history/clear", srv.base_url))
        .header("Host", &srv.host_header)
        .header("Origin", &srv.base_url)
        .header(reqwest::header::COOKIE, &alice_session_cookie)
        .json(&json!({}))
        .send()
        .await
        .expect("Alice clear history");
    assert_eq!(clr_resp.status(), StatusCode::OK);
    let clr_json: Value = clr_resp.json().await.expect("clear json");
    assert_eq!(clr_json["ok"], true);

    let alice_after_clr: Value = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header(reqwest::header::COOKIE, &alice_session_cookie)
        .send()
        .await
        .expect("Alice list after clear")
        .json()
        .await
        .expect("Alice after clear json");
    assert_eq!(alice_after_clr["availableCount"].as_u64(), Some(0));

    let bob_still: Value = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header(reqwest::header::COOKIE, &bob_session_cookie)
        .send()
        .await
        .expect("Bob list still intact")
        .json()
        .await
        .expect("Bob json");
    assert_eq!(bob_still["availableCount"].as_u64(), Some(1));
    assert_eq!(bob_still["available"][0]["slug"], slug_bob_1);

    let anon_still: Value = client
        .get(format!("{}/api/deal-room", srv.base_url))
        .header(reqwest::header::COOKIE, &anon_vid_cookie)
        .send()
        .await
        .expect("Anon list still intact")
        .json()
        .await
        .expect("Anon json");
    assert_eq!(anon_still["availableCount"].as_u64(), Some(1));
    assert_eq!(anon_still["available"][0]["slug"], slug_anon_1);
}
