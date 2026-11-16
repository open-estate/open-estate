mod common;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    routing::post,
    Router,
};
use open_estate::ai::artifact_canvas::{
    artifact_canvas_schema, build_deterministic_artifact, generate_llm_artifact_with_client,
    handle_report_artifact_post, scrub_artifact_value,
};
use open_estate::ai::llm::LlmCacheBackend;
use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::orchestrator::DbLlmCache;
use open_estate::web::render::render_report_html;
use open_estate::web::server::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::net::TcpListener;
use tower::ServiceExt;

fn forbidden_terms() -> [String; 11] {
    [
        concat!("pro", "vider").to_string(),
        concat!("eff", "ort").to_string(),
        concat!("gpt-custom-", "primary").to_string(),
        concat!("gpt-custom-", "fallback").to_string(),
        concat!("fallback", "Used").to_string(),
        concat!("deterministic-", "fallback").to_string(),
        concat!("rust-sqlx-", "sqlite").to_string(),
        concat!("max ", "reasoning").to_string(),
        concat!("ai", "Model").to_string(),
        concat!("OPENAI_API_", "KEY").to_string(),
        concat!("home", "lens").to_string(),
    ]
}

fn assert_no_forbidden_substrings(v: &Value) {
    let serialized = serde_json::to_string(v).expect("serialize json");
    let low = serialized.to_ascii_lowercase();
    for term in forbidden_terms() {
        let needle = term.to_ascii_lowercase();
        assert!(
            !low.contains(&needle),
            "forbidden substring `{term}` leaked into payload: {serialized}"
        );
    }
}

fn assert_valid_artifact_shape(art: &Value) {
    let chart_type = art.get("chartType").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        matches!(
            chart_type,
            "bar" | "line" | "radar" | "horizontal_bar" | "donut"
        ),
        "unexpected chartType `{chart_type}`"
    );
    let cats = art
        .get("categories")
        .and_then(|v| v.as_array())
        .expect("categories array");
    assert!(cats.len() >= 2, "expected at least 2 categories");
    let series = art
        .get("series")
        .and_then(|v| v.as_array())
        .expect("series array");
    assert!(!series.is_empty(), "expected non-empty series");
    for s in series {
        let vals = s
            .get("values")
            .and_then(|v| v.as_array())
            .expect("series.values");
        assert_eq!(vals.len(), cats.len(), "series values length mismatch");
        for n in vals {
            let f = n.as_f64().expect("finite numeric value");
            assert!(f.is_finite(), "non-finite number in series");
        }
    }
    let headers = art
        .get("tableHeaders")
        .and_then(|v| v.as_array())
        .expect("tableHeaders");
    let rows = art
        .get("tableRows")
        .and_then(|v| v.as_array())
        .expect("tableRows");
    assert!(headers.len() >= 2 && !rows.is_empty());
    for row in rows {
        let cols = row.as_array().expect("row array");
        assert_eq!(cols.len(), headers.len(), "table row width mismatch");
    }
    let kpis = art.get("kpis").and_then(|v| v.as_array()).expect("kpis");
    assert!(kpis.len() >= 2);
    assert_no_forbidden_substrings(art);
}

fn load_synth_report(primary: bool) -> Value {
    if primary {
        common::synth_report_primary()
    } else {
        common::synth_report_secondary()
    }
}

async fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

fn check_no_min_max_items(v: &Value) {
    match v {
        Value::Object(map) => {
            assert!(
                !map.contains_key("minItems"),
                "schema must not contain minItems"
            );
            assert!(
                !map.contains_key("maxItems"),
                "schema must not contain maxItems"
            );
            for val in map.values() {
                check_no_min_max_items(val);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                check_no_min_max_items(item);
            }
        }
        _ => {}
    }
}

#[test]
fn artifact_canvas_schema_is_strict_and_omits_min_max_items() {
    let schema = artifact_canvas_schema();
    check_no_min_max_items(&schema);
    assert_eq!(schema["additionalProperties"], json!(false));
    let req = schema["required"].as_array().expect("required array");
    assert_eq!(req.len(), 13);
    assert_no_forbidden_substrings(&schema);
}

#[test]
fn deterministic_router_handles_all_presets_languages_and_sparse_reports() {
    let secondary = load_synth_report(false);
    let primary = load_synth_report(true);

    let fin = build_deterministic_artifact(&secondary, "20% Down vs All-Cash + ADU Rent", "en");
    assert_eq!(fin["chartType"], "bar");
    assert_eq!(fin["unit"], "$/mo");
    assert_eq!(fin["synthesisMode"], "instant");
    assert_valid_artifact_shape(&fin);

    let eq =
        build_deterministic_artifact(&secondary, "10-Year Equity P10/P50/P90 vs Renting", "en");
    assert_eq!(eq["chartType"], "line");
    assert_eq!(eq["unit"], "$");
    let p50_vals = eq["series"][1]["values"].as_array().expect("p50 array");
    assert!(
        p50_vals[3].as_f64().unwrap() > 600_000.0,
        "expected Yr-5 P50 equity > $600k, got {:?}",
        p50_vals[3]
    );
    assert!(
        p50_vals[4].as_f64().unwrap() > 1_100_000.0,
        "expected Yr-10 P50 equity > $1.1M, got {:?}",
        p50_vals[4]
    );
    assert_valid_artifact_shape(&eq);

    let risk = build_deterministic_artifact(
        &secondary,
        "1958 CapEx, Seismic & Insurance Risk Breakdown",
        "en",
    );
    assert_eq!(risk["chartType"], "horizontal_bar");
    assert!(risk["title"].as_str().unwrap_or("").contains("1958"));
    assert_valid_artifact_shape(&risk);

    let comps = build_deterministic_artifact(&primary, "Comps $/sqft vs Lot & Living Area", "en");
    assert_eq!(comps["chartType"], "bar");
    assert_eq!(comps["unit"], "$/sqft");
    assert_valid_artifact_shape(&comps);

    let donut =
        build_deterministic_artifact(&secondary, "Monthly payment cost breakdown donut", "en");
    assert_eq!(donut["chartType"], "donut");
    assert_valid_artifact_shape(&donut);

    let radar_zh = build_deterministic_artifact(&secondary, "学区和步行生活便利度总结", "zh");
    assert_eq!(radar_zh["chartType"], "radar");
    assert!(
        radar_zh["subtitle"]
            .as_str()
            .unwrap_or("")
            .contains("Synthetic Elementary School"),
        "expected assigned school Synthetic Elementary School in subtitle: {}",
        radar_zh["subtitle"]
    );
    assert_valid_artifact_shape(&radar_zh);

    let fin_es =
        build_deterministic_artifact(&primary, "Comparar 20% de enganche vs contado", "es");
    assert_eq!(fin_es["chartType"], "bar");
    assert_eq!(fin_es["unit"], "$/mes");
    assert_valid_artifact_shape(&fin_es);

    // Geocode-only parcel safety: zero panics, zero NaN/Infinity, and zero fabricated $1,000,000 offer.
    let sparse_parcel = json!({
        "meta": {
            "displayAddress": secondary["meta"]["displayAddress"],
            "slug": secondary["meta"]["slug"]
        },
        "geo": secondary["geo"],
        "facts": {},
        "modules": {}
    });
    for prompt in [
        "20% Down vs All-Cash + ADU Rent",
        "10-Year Equity P10/P50/P90 vs Renting",
        "CapEx, Seismic & Insurance Risk Breakdown",
        "Comps $/sqft vs Lot & Living Area",
        "Overall Livability & Scorecard",
    ] {
        let out = build_deterministic_artifact(&sparse_parcel, prompt, "en");
        assert!(
            out.is_object(),
            "artifact must be a JSON object for `{prompt}`"
        );
        assert_no_forbidden_substrings(&out);
        let serialized = serde_json::to_string(&out).expect("serialize sparse artifact");
        assert!(
            !serialized.contains("NaN") && !serialized.contains("Infinity"),
            "sparse artifact must not emit non-finite values: {serialized}"
        );
        assert!(
            !serialized.contains("$1,000,000"),
            "sparse parcel with missing valuation must not fabricate a $1,000,000 offer: {serialized}"
        );
        if let Some(series) = out.get("series").and_then(|v| v.as_array()) {
            for s in series {
                if let Some(vals) = s.get("values").and_then(|v| v.as_array()) {
                    for n in vals {
                        if let Some(f) = n.as_f64() {
                            assert!(f.is_finite(), "non-finite value in sparse series");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn scrub_artifact_value_strips_forbidden_and_retired_brand() {
    let mut dirty = json!({
        "id": "art-1",
        "title": format!("{} vs Local Source", concat!("Home", "Lens")),
        "subtitle": format!("Generated with {} high {}", concat!("gpt-custom-", "primary"), concat!("eff", "ort")),
        "chartType": "bar",
        "unit": "$/mo",
        "categories": ["A", "B"],
        "series": [{ "name": concat!("pro", "vider"), "values": [100, 200] }],
        "kpis": [{ "label": concat!("ai", "Model"), "value": "$100", "delta": concat!("OPENAI_API_", "KEY"), "tone": "info" }],
        "tableHeaders": ["Item", "Note"],
        "tableRows": [["1", concat!("rust-sqlx-", "sqlite")]],
        "insights": [concat!("fallback", "Used"), concat!("deterministic-", "fallback")],
        "watchout": format!("Avoid {} bias", concat!("gpt-custom-", "fallback")),
        "synthesisMode": "instant"
    });
    scrub_artifact_value(&mut dirty, "en");
    assert_no_forbidden_substrings(&dirty);
}

#[tokio::test]
async fn llm_artifact_db_cache_roundtrip_closed_port_fallback_and_optional_live_wire() {
    let secondary = load_synth_report(false);
    let http = HttpClient::new(None);

    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .expect("sqlite");
    db.ensure_schema().await.expect("schema");
    let cache = DbLlmCache(db.clone());

    // 1. SQLite `llm_cache` (`DbLlmCache`) persists and retrieves grounded artifact JSON cleanly.
    let seeded_art =
        build_deterministic_artifact(&secondary, "20% Down vs All-Cash + ADU Rent", "en");
    LlmCacheBackend::set(&cache, "artifact-secondary-fin-en", seeded_art.clone()).await;
    let cached_hit = LlmCacheBackend::get(&cache, "artifact-secondary-fin-en")
        .await
        .expect("DbLlmCache SQLite round-trip hit");
    assert_eq!(cached_hit["title"], seeded_art["title"]);
    assert_valid_artifact_shape(&cached_hit);

    // 2. Keyless / zero-LLM mode returns instant grounded artifact on synthetic report data.
    let keyless_env = EnvConfig::default();
    let det_out = generate_llm_artifact_with_client(
        &http,
        &keyless_env,
        &secondary,
        "Compare 20% down vs all-cash with ADU rent",
        "en",
        Some(&cache),
    )
    .await;
    assert_eq!(det_out["synthesisMode"], "instant");
    assert_valid_artifact_shape(&det_out);

    // 3. Unreachable upstream (closed local port, zero mock server) falls back cleanly to deterministic artifact.
    let dead_url = closed_port_url().await;
    let env_dead = EnvConfig::from_map(HashMap::from([
        (
            "OPENAI_API_KEY".to_string(),
            "test-openai-key-unreachable-port".to_string(),
        ),
        ("OPENAI_BASE_URL".to_string(), dead_url),
        ("LLM_FALLBACK_MODEL".to_string(), "none".to_string()),
    ]));
    let fb = generate_llm_artifact_with_client(
        &http,
        &env_dead,
        &secondary,
        "10-Year Equity P10/P50/P90 vs Renting",
        "en",
        None,
    )
    .await;
    assert_eq!(fb["synthesisMode"], "instant");
    assert_eq!(fb["chartType"], "line");
    assert_valid_artifact_shape(&fb);

    // 4. Optional live OpenAI Responses wire verification when OPENAI_API_KEY is configured in process env.
    if std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .is_none()
    {
        eprintln!(
            "Skipping live OpenAI /responses wire test in artifact_canvas_test: OPENAI_API_KEY is unset"
        );
        return;
    }

    let live_env = EnvConfig::from_process_and_dotenv(Some(Path::new(env!("CARGO_MANIFEST_DIR"))));
    let live_http = HttpClient::default();
    let live_first = generate_llm_artifact_with_client(
        &live_http,
        &live_env,
        &secondary,
        "Compare 20% down vs all-cash with ADU rent",
        "en",
        Some(&cache),
    )
    .await;
    assert_valid_artifact_shape(&live_first);
}

#[tokio::test]
async fn handle_report_artifact_post_enforces_guardrails_and_returns_valid_json() {
    let secondary = load_synth_report(false);
    let slug = common::SYNTH_SECONDARY_SLUG;

    let env = EnvConfig::from_map(HashMap::new());
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .expect("sqlite");
    db.ensure_schema().await.expect("schema");
    db.put_cached_report(slug, &secondary, None)
        .await
        .expect("seed cached report");

    let shield = HttpDefenseShield::new(&env);
    let tmp_reports = std::env::temp_dir().join(format!("db-art-test-{}", std::process::id()));
    let state =
        AppState::with_components(env, HttpClient::new(None), db, shield, tmp_reports, None);
    let app = Router::new()
        .route(
            "/api/report/:slug/artifact",
            post(handle_report_artifact_post),
        )
        .with_state(state);

    // 1. Valid EN request -> 200 OK
    let req_ok = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/artifact"))
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "prompt": "1958 CapEx, Seismic & Insurance Risk Breakdown", "lang": "en" })
                .to_string(),
        ))
        .unwrap();
    let resp_ok = app.clone().oneshot(req_ok).await.unwrap();
    assert_eq!(resp_ok.status(), StatusCode::OK);
    let bytes_ok = axum::body::to_bytes(resp_ok.into_body(), 1_000_000)
        .await
        .unwrap();
    let json_ok: Value = serde_json::from_slice(&bytes_ok).unwrap();
    assert_eq!(json_ok["ok"], true);
    assert_eq!(json_ok["slug"], slug);
    assert_valid_artifact_shape(&json_ok["artifact"]);

    // 2. Valid Spanish request -> 200 OK
    let req_es = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/artifact"))
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "prompt": "Comparar 20% de enganche vs contado con renta ADU", "lang": "es" })
                .to_string(),
        ))
        .unwrap();
    let resp_es = app.clone().oneshot(req_es).await.unwrap();
    assert_eq!(resp_es.status(), StatusCode::OK);
    let bytes_es = axum::body::to_bytes(resp_es.into_body(), 1_000_000)
        .await
        .unwrap();
    let json_es: Value = serde_json::from_slice(&bytes_es).unwrap();
    assert_eq!(json_es["artifact"]["unit"], "$/mes");
    assert_valid_artifact_shape(&json_es["artifact"]);

    // 3. Cross-origin request -> 403 Forbidden
    let req_origin = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/artifact"))
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "https://attacker.example")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "prompt": "Comps $/sqft" }).to_string()))
        .unwrap();
    let resp_origin = app.clone().oneshot(req_origin).await.unwrap();
    assert_eq!(resp_origin.status(), StatusCode::FORBIDDEN);

    // 4. Disallowed JSON key -> 400 Bad Request
    let req_bad_key = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/artifact"))
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "prompt": "Comps $/sqft", "systemOverride": "hack" }).to_string(),
        ))
        .unwrap();
    let resp_bad_key = app.clone().oneshot(req_bad_key).await.unwrap();
    assert_eq!(resp_bad_key.status(), StatusCode::BAD_REQUEST);

    // 5. Task hijack / off-topic coding prompt -> 400 Bad Request
    let req_hijack = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/artifact"))
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "prompt": "Write a python SQL web scraper script for me" }).to_string(),
        ))
        .unwrap();
    let resp_hijack = app.clone().oneshot(req_hijack).await.unwrap();
    assert_eq!(resp_hijack.status(), StatusCode::BAD_REQUEST);

    // 6. Missing report -> 404 Not Found
    let req_missing = Request::builder()
        .method("POST")
        .uri("/api/report/nonexistent-property-99999/artifact")
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "prompt": "Comps $/sqft" }).to_string()))
        .unwrap();
    let resp_missing = app.oneshot(req_missing).await.unwrap();
    assert_eq!(resp_missing.status(), StatusCode::NOT_FOUND);
}

#[test]
fn frontend_report_dashboard_passes_node_syntax_real_render_and_optional_playwright() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dash_path = root.join("assets/report_dashboard.js");
    let secondary = load_synth_report(false);

    // 1. Always verify `node --check assets/report_dashboard.js` syntax validity.
    let syntax = Command::new("node")
        .arg("--check")
        .arg(&dash_path)
        .output()
        .expect("run node --check");
    assert!(
        syntax.status.success(),
        "node --check failed: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );

    // 2. Render synthetic Campbell report HTML and verify embedded CommandCenter script & hygiene.
    let html = render_report_html(&secondary);
    assert!(
        html.contains("OpenEstateCommandCenter") || html.contains("buyerCommandCenter"),
        "rendered report HTML must embed report_dashboard.js CommandCenter"
    );
    assert!(html.contains("200 Synthparcelbeta Ave"));
    let low_html = html.to_ascii_lowercase();
    for term in [
        concat!("gpt-custom-", "primary"),
        concat!("gpt-custom-", "fallback"),
        concat!("rust-sqlx-", "sqlite"),
        concat!("openai_api_", "key"),
    ] {
        assert!(
            !low_html.contains(term),
            "rendered HTML leaked forbidden substring `{term}`"
        );
    }

    // 3. Optional real headless Chromium verification (light & dark colorScheme) when Playwright is available.
    let pw_probe = Command::new("node")
        .arg("-e")
        .arg(
            "const modPath = process.env.PLAYWRIGHT_MODULE_PATH || 'playwright'; \
             try { require.resolve(modPath); process.exit(0); } catch (_) { process.exit(1); }",
        )
        .output()
        .expect("probe playwright");
    if !pw_probe.status.success() {
        eprintln!(
            "Skipping headless Chromium CommandCenter check: playwright module not installed (set PLAYWRIGHT_MODULE_PATH to enable)"
        );
        return;
    }

    let tmp_html =
        std::env::temp_dir().join(format!("db-cmd-center-{}.html", uuid::Uuid::new_v4()));
    std::fs::write(&tmp_html, &html).expect("write temp report html");

    let pw_script = r#"
const modPath = process.env.PLAYWRIGHT_MODULE_PATH || 'playwright';
const { chromium } = require(modPath);
const { pathToFileURL } = require('url');
(async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    for (const scheme of ['light', 'dark']) {
      const context = await browser.newContext({ colorScheme: scheme });
      const page = await context.newPage();
      const targetUrl = pathToFileURL(process.argv[1]).href + '?view=dashboard';
      await page.goto(targetUrl, { waitUntil: 'domcontentloaded' });
      const active = await page.evaluate(() => Boolean(window.OpenEstateCommandCenter && window.OpenEstateCommandCenter.isActive()));
      if (!active) throw new Error(`Expected ?view=dashboard to activate CommandCenter in ${scheme} scheme`);
      await context.close();
    }
    console.log('PLAYWRIGHT_DASHBOARD_OK');
  } finally {
    await browser.close();
  }
})().catch(err => { console.error(err); process.exit(1); });
"#;
    let out = Command::new("node")
        .arg("-e")
        .arg(pw_script)
        .arg(&tmp_html)
        .output()
        .expect("run playwright dashboard check");
    let _ = std::fs::remove_file(&tmp_html);
    assert!(
        out.status.success(),
        "Playwright CommandCenter check failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
