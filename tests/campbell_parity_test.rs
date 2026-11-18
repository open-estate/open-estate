#![recursion_limit = "512"]
//! Zero-Feature-Loss Benchmark & Cloud Open Estate 12-Factor Parity Suite
//! (procedural synthetic Campbell 95008 parcel via `common::synth_report_secondary()`).
//!
//! Verifies that the Personal-Research self-hosted Open Estate build (`PUBLIC_DATA_MODE=strict`,
//! Direct HTTPS default, USGS public-domain aerials, ToS-clean companion extension) preserves
//! 100% of analytical modules, trilingual SSR/SPA workspaces (`en` / `zh` / `es`), interactive
//! endpoints, and 12-factor Cloud Open Estate BYOK connectors with zero invented data (`AGENTS.md`).

mod common;

use axum::routing::get;
use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::property::{
    build_data_licensing_block, connector_status, populate_clean_room_market_data,
};
use open_estate::shared::{EnvConfig, HttpClient, SourceItem};
use open_estate::web::ops_dashboard::build_connector_catalog;
use open_estate::web::render::{
    normalize_stored_report, render_report_html, sanitize_report_for_html, REPORT_MOTION_JS,
    REPORT_TEMPLATE_HTML, REPORT_WORKSPACE_JS,
};
use open_estate::web::server::{build_router, AppState};
use reqwest::{header, Client, StatusCode};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::net::TcpListener;
use uuid::Uuid;

const CAMPBELL_SYNTH_SLUG: &str = common::SYNTH_SECONDARY_SLUG;
const CAMPBELL_SYNTH_ADDRESS: &str = common::SYNTH_SECONDARY_ADDRESS;

struct ParityServer {
    base_url: String,
    state: AppState,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ParityServer {
    async fn start() -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("open-estate-campbell-parity-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        let data_dir = temp_dir.join("data");
        std::fs::create_dir_all(&reports_dir).expect("create reports_dir");
        std::fs::create_dir_all(&data_dir).expect("create data_dir");
        let db_path = temp_dir.join("parity.sqlite");

        let mut env_map = HashMap::new();
        env_map.insert(
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        );
        env_map.insert(
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        );
        env_map.insert(
            "REPORTS_DIR".to_string(),
            reports_dir.to_string_lossy().into_owned(),
        );
        env_map.insert("SYNC_INTERVAL_MIN".to_string(), "0".to_string());

        let env = EnvConfig::from_map(env_map);
        let http = HttpClient::offline_loopback_only(None);
        let db = DbStore::connect(&env).await.expect("connect sqlite");
        db.ensure_schema().await.expect("ensure_schema");
        let shield = HttpDefenseShield::new(&env);

        let state = AppState::with_components(env, http, db, shield, reports_dir, Some(data_dir));
        let router = build_router(state.clone()).nest(
            "/__noop",
            axum::Router::new().route("/", get(|| async { "ok" })),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind parity server");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
        });

        Self {
            base_url,
            state,
            temp_dir,
            shutdown_tx: Some(tx),
        }
    }

    async fn seed_campbell_fixture(&self) -> Value {
        let mut report = common::synth_report_secondary();
        normalize_stored_report(&mut report);

        let html = render_report_html(&report);
        let slug_dir = self.state.reports_dir.join(CAMPBELL_SYNTH_SLUG);
        std::fs::create_dir_all(&slug_dir).expect("create slug_dir");
        std::fs::write(
            slug_dir.join("report.json"),
            serde_json::to_string(&report).expect("serialize report"),
        )
        .expect("write report.json");
        std::fs::write(slug_dir.join("index.html"), &html).expect("write index.html");

        self.state
            .db
            .put_cached_report(CAMPBELL_SYNTH_SLUG, &report, Some(&html))
            .await
            .expect("put_cached_report");
        report
    }
}

impl Drop for ParityServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn browser_client() -> Client {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/128.0.0.0 Safari/537.36",
        ),
    );
    Client::builder()
        .default_headers(headers)
        .no_proxy()
        .build()
        .expect("build browser client")
}

fn check_inline_scripts_with_node(html: &str, label: &str) {
    let Ok(node_ver) = std::process::Command::new("node").arg("--version").output() else {
        return;
    };
    if !node_ver.status.success() {
        return;
    }

    let mut pos = 0usize;
    let mut idx = 0usize;
    while let Some(open_rel) = html[pos..].find("<script") {
        let open_start = pos + open_rel;
        let Some(tag_end_rel) = html[open_start..].find('>') else {
            break;
        };
        let attrs = &html[open_start..open_start + tag_end_rel];
        let body_start = open_start + tag_end_rel + 1;
        let Some(close_rel) = html[body_start..].find("</script>") else {
            break;
        };
        let body = &html[body_start..body_start + close_rel];
        pos = body_start + close_rel + "</script>".len();

        let attrs_lc = attrs.to_ascii_lowercase();
        if attrs_lc.contains("application/json")
            || attrs_lc.contains("src=")
            || body.trim().is_empty()
        {
            continue;
        }
        let tmp = std::env::temp_dir().join(format!(
            "open-estate-campbell-synth-js-{label}-{idx}-{}.js",
            Uuid::new_v4()
        ));
        std::fs::write(&tmp, body).expect("write temp JS");
        let out = std::process::Command::new("node")
            .arg("--check")
            .arg(&tmp)
            .output()
            .expect("run node --check");
        let _ = std::fs::remove_file(&tmp);
        assert!(
            out.status.success(),
            "node --check failed for {label} <script> #{idx}:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        idx += 1;
    }
    assert!(idx > 0, "expected inline <script> blocks in {label}");
}

#[tokio::test]
async fn campbell_synth_full_report_html_json_and_trilingual_parity() {
    let srv = ParityServer::start().await;
    let seeded = srv.seed_campbell_fixture().await;
    let client = browser_client();

    // 1. Sanitized HTML JSON & AGENTS.md Theme / Data Honesty contracts
    let sanitized = sanitize_report_for_html(&seeded);
    assert_eq!(
        sanitized["ai"]["fengShui"]["en"],
        json!(""),
        "English AI Feng Shui prose must always be blanked in rendered report payload"
    );
    assert!(
        sanitized["ai"]["fengShui"]["zh"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "Chinese AI 堪舆风水 reading must remain intact"
    );
    assert!(
        REPORT_MOTION_JS.contains("prefers-color-scheme: dark")
            && REPORT_MOTION_JS.contains("addEventListener('change'"),
        "Video tour must react live to viewer OS prefers-color-scheme per AGENTS.md"
    );
    assert!(
        REPORT_MOTION_JS
            .contains("basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer"),
        "Video tour must use public-domain USGS National Map orthoimagery"
    );
    assert!(
        REPORT_WORKSPACE_JS.contains("usgsExportUrl('USGSImageryOnly'")
            && !REPORT_WORKSPACE_JS.contains("esriExportUrl"),
        "Report workspace gallery fallback must use USGS public-domain export URLs"
    );
    assert!(
        REPORT_TEMPLATE_HTML.contains("id=\"walkthroughModal\""),
        "Walkthrough modal must remain present in report template"
    );

    // 2. Render & validate EN, zh-CN, and ES report HTML pages (`GET /r/:slug/` + `?lang=zh` + `?lang=es`)
    for (suffix, label) in [("", "en"), ("?lang=zh", "zh"), ("?lang=es", "es")] {
        let resp = client
            .get(format!("{}/r/{CAMPBELL_SYNTH_SLUG}/{suffix}", srv.base_url))
            .send()
            .await
            .expect("GET rendered report");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "GET /r/{CAMPBELL_SYNTH_SLUG}/{suffix}"
        );
        let html = resp.text().await.expect("read html");

        assert!(
            html.contains(CAMPBELL_SYNTH_ADDRESS),
            "{label} missing displayAddress"
        );
        assert!(
            html.contains("000-00-200"),
            "{label} missing APN 000-00-200"
        );
        assert!(
            html.contains("200 SYNTHPARCELBETA AVE #B"),
            "{label} missing detached ADU #B"
        );
        assert!(
            html.contains("SYNTH-ADU-2023-200"),
            "{label} missing permit SYNTH-ADU-2023-200"
        );
        assert!(
            html.contains("Synthetic Regional Reservoir Dam"),
            "{label} missing dam inundation NHD detail"
        );
        assert!(
            html.contains("堪舆风水"),
            "{label} missing Chinese-only 堪舆风水 nav"
        );
        assert!(
            html.contains("罗盘二十四山立极图"),
            "{label} missing Luopan 24-mountain chart"
        );
        assert!(
            html.contains("玄空飞星盘"),
            "{label} missing Xuan Kong flying star chart"
        );
        assert!(html.contains("八宅明镜"), "{label} missing Ba Zhai chart");

        for anchor in [
            "id=\"overview\"",
            "id=\"value\"",
            "id=\"diligence\"",
            "id=\"hazards\"",
            "id=\"environment\"",
            "id=\"neighborhood\"",
            "id=\"schools\"",
            "id=\"house3d\"",
            "id=\"fengshui\"",
            "id=\"inspection\"",
            "id=\"cre\"",
        ] {
            assert!(
                html.contains(anchor),
                "{label} rendered report HTML missing section anchor `{anchor}`"
            );
        }

        check_inline_scripts_with_node(&html, label);
    }

    // 3. Full JSON parity on both `/r/:slug/report.json` and `/api/report?slug=...`
    let rj_resp = client
        .get(format!(
            "{}/r/{CAMPBELL_SYNTH_SLUG}/report.json",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET report.json");
    assert_eq!(rj_resp.status(), StatusCode::OK);
    let rj: Value = rj_resp.json().await.expect("parse report.json");

    let api_resp = client
        .get(format!(
            "{}/api/report?slug={CAMPBELL_SYNTH_SLUG}",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET /api/report");
    assert_eq!(api_resp.status(), StatusCode::OK);
    let api_json: Value = api_resp.json().await.expect("parse /api/report");
    assert_eq!(api_json["ok"], json!(true));

    // Exact synthetic parcel facts & metrics (AGENTS.md Data Honesty — zero invented numbers)
    assert_eq!(rj["meta"]["displayAddress"], json!(CAMPBELL_SYNTH_ADDRESS));
    assert_eq!(rj["facts"]["apn"], json!("000-00-200"));
    assert_eq!(rj["facts"]["zoning"], json!("R-1-6"));
    assert_eq!(rj["facts"]["beds"], json!(4));
    assert_eq!(rj["facts"]["baths"], json!(3));
    assert_eq!(rj["facts"]["sqft"], json!(2200));
    assert_eq!(rj["facts"]["grossSqft"], json!(2800));
    assert_eq!(rj["facts"]["lotSqft"], json!(9200));
    assert_eq!(rj["facts"]["yearBuilt"], json!(1958));
    assert_eq!(
        rj.pointer("/facts/regulatoryDisclosures/secondaryUnitAddress"),
        Some(&json!("200 SYNTHPARCELBETA AVE #B"))
    );
    let permits_str = serde_json::to_string(&rj["facts"]["permits"]).unwrap();
    assert!(
        permits_str.contains("SYNTH-ADU-2023-200") && permits_str.contains("SYNTH-ROOF-2022-201")
    );

    // Scorecard radar & verdict match seeded deterministic calculation 1:1
    assert_eq!(rj["summary"]["scores"], seeded["summary"]["scores"]);
    assert_eq!(rj["summary"]["verdict"], seeded["summary"]["verdict"]);
    assert!(
        rj["summary"]["scores"]["overall"]
            .as_i64()
            .unwrap_or(0)
            > 0
    );

    // Valuation, 8 comps, fan scenarios, ownership & CA Prop 13 reassessment
    let val_data = &rj["modules"]["valuation"]["data"];
    assert_eq!(val_data["estimate"], seeded["modules"]["valuation"]["data"]["estimate"]);
    assert!(val_data["estimate"]["value"].as_i64().unwrap_or(0) > 1_800_000);
    assert_eq!(val_data["comps"].as_array().map(|a| a.len()), Some(8));

    assert!(
        val_data["ownership"]["monthly"]["total"]
            .as_f64()
            .unwrap_or(0.0)
            > 9_000.0
    );
}

#[tokio::test]
async fn campbell_synth_interactive_workspace_endpoints_parity() {
    let srv = ParityServer::start().await;
    srv.seed_campbell_fixture().await;
    let client = browser_client();

    // 1. GET /api/buyer-diligence/:slug -> HOA profile + 7-module buyer diligence
    let bd_resp = client
        .get(format!(
            "{}/api/buyer-diligence/{CAMPBELL_SYNTH_SLUG}",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET buyer-diligence");
    assert_eq!(bd_resp.status(), StatusCode::OK);
    let bd: Value = bd_resp.json().await.expect("parse buyer-diligence");
    assert!(bd["hoaProfile"].is_object());
    for module_key in [
        "cashToClose",
        "ongoingCosts",
        "connectivity",
        "utilitiesAndMandates",
        "neighborhoodPipeline",
        "exitAndBreakEven",
        "postClosingAndClimate",
    ] {
        assert!(
            bd["buyerDiligence"][module_key].is_object(),
            "buyerDiligence missing module `{module_key}`"
        );
    }

    // 2. GET /api/house3d/:slug -> 3D property scene (footprint + terrain fallback when offline)
    let h3d_resp = client
        .get(format!(
            "{}/api/house3d/{CAMPBELL_SYNTH_SLUG}",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET house3d");
    assert!(
        h3d_resp.status() == StatusCode::OK || h3d_resp.status() == StatusCode::ACCEPTED,
        "unexpected house3d status {}",
        h3d_resp.status()
    );
    let h3d: Value = h3d_resp.json().await.expect("parse house3d");
    assert!(h3d.is_object());

    // 3. GET /api/media/:slug/manifest -> USGS public-domain imagery, zero proprietary Esri URLs
    let mf_resp = client
        .get(format!(
            "{}/api/media/{CAMPBELL_SYNTH_SLUG}/manifest",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET media manifest");
    assert_eq!(mf_resp.status(), StatusCode::OK);
    let mf_text = mf_resp.text().await.expect("read media manifest");
    assert!(
        !mf_text.contains(concat!("server.", "arcgisonline.com")),
        "media manifest must never emit proprietary Esri URLs"
    );
    let mf: Value = serde_json::from_str(&mf_text).expect("parse media manifest");
    assert!(mf["items"].is_array());

    // 4. POST /api/report/:slug/slides & POST /api/report/:slug/artifact
    let slides_resp = client
        .post(format!(
            "{}/api/report/{CAMPBELL_SYNTH_SLUG}/slides",
            srv.base_url
        ))
        .header(header::ORIGIN, &srv.base_url)
        .json(&json!({ "refresh": false }))
        .send()
        .await
        .expect("POST slides");
    assert_eq!(slides_resp.status(), StatusCode::OK);
    let slides_json: Value = slides_resp.json().await.expect("parse slides");
    assert_eq!(slides_json["ok"], json!(true));
    assert!(slides_json["walkthroughSlides"]["slides"]
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false));

    let artifact_resp = client
        .post(format!(
            "{}/api/report/{CAMPBELL_SYNTH_SLUG}/artifact",
            srv.base_url
        ))
        .header(header::ORIGIN, &srv.base_url)
        .json(&json!({
            "prompt": "Compare Prop 13 tax basis vs monthly ownership cost and ADU rent potential",
            "lang": "en"
        }))
        .send()
        .await
        .expect("POST artifact");
    assert_eq!(artifact_resp.status(), StatusCode::OK);
    let artifact_json: Value = artifact_resp.json().await.expect("parse artifact");
    assert_eq!(artifact_json["ok"], json!(true));
    assert!(artifact_json["artifact"].is_object());

    // 5. POST /api/move-in-quote -> Regional licensed contractor estimator
    let mi_resp = client
        .post(format!("{}/api/move-in-quote", srv.base_url))
        .header(header::ORIGIN, &srv.base_url)
        .json(&json!({
            "slug": CAMPBELL_SYNTH_SLUG,
            "item": "Interior paint refresh"
        }))
        .send()
        .await
        .expect("POST move-in-quote");
    assert_eq!(mi_resp.status(), StatusCode::OK);
    let mi_json: Value = mi_resp.json().await.expect("parse move-in-quote");
    assert_eq!(mi_json["ok"], json!(true));
    assert!(mi_json["quote"]["typicalUsd"].as_i64().unwrap_or(0) > 1_000);

    // 6. POST /api/ask -> Grounded Q&A over synthetic Campbell report data
    let ask_resp = client
        .post(format!("{}/api/ask", srv.base_url))
        .header(header::ORIGIN, &srv.base_url)
        .json(&json!({
            "slug": CAMPBELL_SYNTH_SLUG,
            "question": "What are the flood, dam inundation, and ADU details for 200 Synthparcelbeta Ave?"
        }))
        .send()
        .await
        .expect("POST /api/ask");
    assert_eq!(ask_resp.status(), StatusCode::OK);
    let ask_json: Value = ask_resp.json().await.expect("parse /api/ask");
    assert_eq!(ask_json["ok"], json!(true));
    assert!(ask_json["answer"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));

    // 7. GET /api/extension/summary & GET /deal-room -> ToS-clean browser companion + Deal Room
    let ext_resp = client
        .get(format!(
            "{}/api/extension/summary?address={}",
            srv.base_url,
            urlencoding::encode(CAMPBELL_SYNTH_ADDRESS)
        ))
        .send()
        .await
        .expect("GET extension summary");
    assert_eq!(ext_resp.status(), StatusCode::OK);
    let ext_json: Value = ext_resp.json().await.expect("parse extension summary");
    assert_eq!(ext_json["ok"], json!(true));
    assert_eq!(ext_json["cached"], json!(true));
    assert_eq!(ext_json["slug"], json!(CAMPBELL_SYNTH_SLUG));

    let dr_resp = client
        .get(format!(
            "{}/deal-room?slugs={CAMPBELL_SYNTH_SLUG}",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET deal-room");
    assert_eq!(dr_resp.status(), StatusCode::OK);
    let dr_html = dr_resp.text().await.expect("read deal-room html");
    assert!(dr_html.contains("200 Synthparcelbeta Ave"));
    assert!(dr_html.contains("堪舆风水 · 坐向元运"));

    // 8. POST /api/cre/business-eval is unmounted (`404`) in Personal Edition
    let cre_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header(header::ORIGIN, &srv.base_url)
        .json(&json!({
            "slug": CAMPBELL_SYNTH_SLUG,
            "evalMode": "all",
            "concept": "cafe_qsr",
            "storeSqft": 1600
        }))
        .send()
        .await
        .expect("POST /api/cre/business-eval");
    assert_eq!(cre_resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cloud_open_estate_twelve_factor_connectors_and_clean_room_market_parity() {
    // 1. Default self-hosted Personal-Research posture: PUBLIC_DATA_MODE=strict, Direct HTTPS, OpenAI defaults
    let oss_env = EnvConfig::default();
    assert!(
        oss_env.is_commercial_strict(),
        "Self-hosted default must enforce PUBLIC_DATA_MODE=strict"
    );
    assert_eq!(oss_env.brand_name(), "Open Estate");
    let oss_http = HttpClient::from_env(&oss_env, None);
    assert_eq!(oss_http.egress_status_json()["mode"], json!("direct"));
    let strict_lic = build_data_licensing_block(&oss_env, false);
    assert_eq!(strict_lic["mode"], json!("commercial_clean_room"));
    assert_eq!(strict_lic["non_appraisal_disclaimer"], json!(true));

    // 2. Cloud Open Estate 12-factor configuration: every BYOK connector & infrastructure override
    //    activates cleanly via standard environment variables without code changes or secret leaks.
    let secret_openai = "test-cloud-openai-placeholder-112233";
    let secret_attom = "attom-cloud-test-secret-778899";
    let secret_reso = "reso-cloud-test-bearer-001122";
    let secret_smtp = "smtp-cloud-test-pass-334455";

    let cloud_env = EnvConfig::from_map(HashMap::from([
        ("OPENAI_API_KEY".to_string(), secret_openai.to_string()),
        (
            "OPENAI_BASE_URL".to_string(),
            "https://api.openai.com/v1".to_string(),
        ),
        ("LLM_MODEL".to_string(), "gpt-4.1".to_string()),
        ("LLM_FALLBACK_MODEL".to_string(), "gpt-4o-mini".to_string()),
        ("ATTOM_API_KEY".to_string(), secret_attom.to_string()),
        (
            "RESO_API_URL".to_string(),
            "https://api.mlsgrid.example/v2".to_string(),
        ),
        ("RESO_BEARER_TOKEN".to_string(), secret_reso.to_string()),
        ("SIMPLYRETS_AUTH".to_string(), "user:pass".to_string()),
        (
            "GOOGLE_MAPS_API_KEY".to_string(),
            "gmaps-key-test".to_string(),
        ),
        ("ARCGIS_API_KEY".to_string(), "arcgis-key-test".to_string()),
        (
            "SHOVELS_API_KEY".to_string(),
            "shovels-key-test".to_string(),
        ),
        (
            "ZONEOMICS_API_KEY".to_string(),
            "zoneomics-key-test".to_string(),
        ),
        (
            "GREATSCHOOLS_API_KEY".to_string(),
            "gs-key-test".to_string(),
        ),
        ("SCHOOLDIGGER_APP_ID".to_string(), "sd-app-id".to_string()),
        ("SCHOOLDIGGER_APP_KEY".to_string(), "sd-app-key".to_string()),
        ("SMTP_HOST".to_string(), "smtp.mail.example".to_string()),
        ("SMTP_USER".to_string(), "alerts@open-estate.local".to_string()),
        ("SMTP_PASS".to_string(), secret_smtp.to_string()),
        ("PUBLIC_DATA_MODE".to_string(), "local_csv".to_string()),
        (
            "OSRM_BASE_URL".to_string(),
            "http://osrm.open-estate.internal:5000".to_string(),
        ),
        (
            "OUTBOUND_PROXY_URLS".to_string(),
            "http://egress.open-estate.internal:8888".to_string(),
        ),
        (
            "MYSQL_URL".to_string(),
            "mysql://openestate:pw@mysql.open-estate.internal:3306/openestate".to_string(),
        ),
        ("MAPBOX_ACCESS_TOKEN".to_string(), "mbx-token-test".to_string()),
        ("LOCATIONIQ_API_KEY".to_string(), "liq-key-test".to_string()),
        ("FREEROUTE_API_KEY".to_string(), "fr-key-test".to_string()),
        ("OPENROUTESERVICE_API_KEY".to_string(), "ors-key-test".to_string()),
    ]));

    assert!(!cloud_env.is_commercial_strict());
    assert!(cloud_env.llm_configured());
    assert!(cloud_env.shovels_configured());
    assert!(cloud_env.zoneomics_configured());
    assert!(cloud_env.smtp_configured());

    let connectors = connector_status(&cloud_env);
    for expected_id in [
        "reso",
        "simplyrets",
        "attom",
        "shovels",
        "zoneomics",
        "county_gis",
        "greatschools",
        "schooldigger",
        "google_places",
        "ai_analyst",
    ] {
        let entry = connectors
            .iter()
            .find(|c| c["id"] == expected_id)
            .unwrap_or_else(|| panic!("missing connector `{expected_id}`"));
        assert_eq!(
            entry["active"],
            json!(true),
            "connector `{expected_id}` must activate under Cloud Open Estate env"
        );
    }

    let catalog = build_connector_catalog(&cloud_env);
    assert_eq!(catalog.len(), 13);
    assert!(
        catalog.iter().all(|c| c["configured"] == json!(true)),
        "all 13 Personal OSS ops catalog connectors must report configured=true in full env: {catalog:?}"
    );
    let catalog_str = serde_json::to_string(&catalog).unwrap();
    for secret in [
        secret_openai,
        secret_attom,
        secret_reso,
        secret_smtp,
    ] {
        assert!(
            !catalog_str.contains(secret),
            "connector catalog leaked secret `{secret}`"
        );
    }

    let cloud_http = HttpClient::from_env(&cloud_env, None);
    assert!(!cloud_http.is_strict_proxy_egress());
    assert!(cloud_http.should_bypass_proxy("osrm.open-estate.internal"));

    // 3. Clean-room federal market engine (`PUBLIC_DATA_MODE=strict`) offline parity for Campbell 95008:
    //    verifies trailing 60-month `metroMarket.market_temp.series` array for ECharts rendering!
    let fixture = common::synth_report_secondary();
    let geo = &fixture["geo"];
    let acs = fixture.pointer("/modules/market/data/acs");

    let fred_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind synthetic FRED loopback");
    let fred_addr = fred_listener.local_addr().unwrap();
    let fred_app = axum::Router::new().route(
        "/graph/fredgraph.csv",
        axum::routing::get(|| async {
            let mut csv = String::from("DATE,VALUE\n");
            for yr in 2010..=2026 {
                for mo in 1..=12 {
                    if yr == 2026 && mo > 9 {
                        break;
                    }
                    let idx_m = (yr - 2010) * 12 + (mo - 1);
                    let val = 240.0 * (1.0030_f64).powi(idx_m);
                    csv.push_str(&format!("{yr:04}-{mo:02}-01,{val:.2}\n"));
                }
            }
            ([("content-type", "text/csv; charset=utf-8")], csv)
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(fred_listener, fred_app).await;
    });

    let clean_room_env =
        oss_env.with_overrides([("FRED_BASE_URL", format!("http://{fred_addr}").as_str())]);
    let synth_fhfa: Vec<(String, f64)> = (2005..=2026)
        .flat_map(|yr| {
            [1, 4, 7, 10].into_iter().filter_map(move |mo| {
                if yr == 2026 && mo > 7 {
                    return None;
                }
                let q_idx = (yr - 2005) * 4 + (mo / 3);
                Some((format!("{yr:04}-{mo:02}"), 260.0 * (1.0108_f64).powi(q_idx)))
            })
        })
        .collect();

    let offline_http = HttpClient::offline_loopback_only(None);
    let mut market_data = Map::new();
    let mut market_sources: Vec<SourceItem> = Vec::new();
    populate_clean_room_market_data(
        &offline_http,
        &clean_room_env,
        geo,
        acs,
        Some(&synth_fhfa),
        Some(&synth_fhfa),
        Some(&synth_fhfa),
        Some(6.65),
        &mut market_data,
        &mut market_sources,
    )
    .await;

    let md = Value::Object(market_data);
    assert!(md["metroZhvi"]["latest"].as_f64().unwrap_or(0.0) > 800_000.0);
    assert!(md["usZhvi"]["latest"].as_f64().unwrap_or(0.0) > 250_000.0);
    assert!(md["tiers"]["mid"].as_i64().unwrap_or(0) >= 0);
    assert!(md["bedroomYoY"].is_object());
    assert_eq!(md["rent"]["cleanRoom"], json!(true));
    
    let temp_series = md
        .pointer("/metroMarket/market_temp/series")
        .and_then(Value::as_array)
        .expect("metroMarket.market_temp.series array must be populated");
    assert_eq!(
        temp_series.len(),
        60,
        "metroMarket.market_temp.series must emit trailing 60-month [[date, val], ...] points"
    );
    assert!(temp_series[0][0].is_string() && temp_series[0][1].is_number());

    for series_key in [
        "zip",
        "zipSfr",
        "zipTop",
        "zipBottom",
        "metro",
        "us",
        "zori",
        "metroZori",
    ] {
        assert!(
            md["series"][series_key]
                .as_array()
                .map(|a| a.len() >= 120)
                .unwrap_or(false),
            "clean-room market series `{series_key}` must contain monthly history"
        );
    }
}
