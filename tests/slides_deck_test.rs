mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use open_estate::ai::slides::{
    build_deterministic_walkthrough_slides, ensure_walkthrough_slides_on_report,
    generate_llm_walkthrough_slides, generate_llm_walkthrough_slides_with_client,
    generate_slides_for_report_json, generate_slides_for_report_json_with_client,
    scrub_walkthrough_deck_value, walkthrough_slides_schema, WALKTHROUGH_SLIDE_IDS,
};
use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::render_report_html;
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

fn synth_secondary_slides_report() -> Value {
    let mut rep = common::synth_report_secondary();
    rep["facts"]["avms"] = json!([
        { "source": "Zillow", "value": 2_390_000, "asOf": "2026-09" },
        { "source": "Redfin", "value": 2_350_000, "asOf": "2026-09" }
    ]);
    rep["modules"]["civic"]["data"]["crime"] = Value::Null;
    rep
}

fn synth_primary_slides_report() -> Value {
    let mut rep = common::synth_report_primary();
    rep["modules"]["civic"]["data"]["crime"] = json!({
        "agency": { "name": "Metro Area Police Department" },
        "latest": {
            "violent": { "ratePer100k": 505.3 },
            "property": { "ratePer100k": 2403.6 }
        }
    });
    rep
}

fn assert_no_forbidden_substrings(json_val: &Value, context: &str) {
    let serialized = serde_json::to_string(json_val).expect("serialize deck");
    let low = serialized.to_ascii_lowercase();
    let forbidden = [
        concat!("pro", "vider"),
        concat!("pro", "viders"),
        concat!("eff", "ort"),
        concat!("gpt-custom-", "primary"),
        concat!("gpt-custom-", "fallback"),
        concat!("fallback", "used"),
        concat!("home", "lens"),
        concat!("ai", "model"),
        concat!("deterministic-", "fallback"),
        concat!("rust-sqlx-", "sqlite"),
        concat!("openai_api_", "key"),
        concat!("max ", "reasoning"),
        "claude-",
        "gemini-",
    ];
    for tok in forbidden {
        assert!(
            !low.contains(tok),
            "{context}: found forbidden substring `{tok}` in: {serialized}"
        );
    }
    assert!(
        !open_estate::shared::has_retired_brand(&serialized),
        "{context}: retired brand leaked in serialized deck"
    );
}

fn assert_valid_deck_schema(deck: &Value, expected_mode: &str) {
    assert!(
        deck.get("generatedAt")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "missing generatedAt"
    );
    assert!(
        deck.get("deckTitle")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "missing deckTitle"
    );
    assert!(
        deck.get("deckSubtitle")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "missing deckSubtitle"
    );
    assert_eq!(
        deck.get("synthesisMode").and_then(|v| v.as_str()),
        Some(expected_mode),
        "unexpected synthesisMode"
    );

    let slides = deck
        .get("slides")
        .and_then(|v| v.as_array())
        .expect("slides array");
    assert_eq!(slides.len(), 8, "deck must contain exactly 8 slides");

    let expected_sections = [
        "overview",
        "county",
        "value",
        "payment",
        "house3d",
        "risk",
        "schools",
        "inspection",
    ];

    for (idx, slide) in slides.iter().enumerate() {
        assert_eq!(
            slide.get("slideNumber").and_then(|v| v.as_u64()),
            Some((idx + 1) as u64)
        );
        assert_eq!(
            slide.get("id").and_then(|v| v.as_str()),
            Some(WALKTHROUGH_SLIDE_IDS[idx])
        );
        assert_eq!(
            slide.get("visualKind").and_then(|v| v.as_str()),
            Some(WALKTHROUGH_SLIDE_IDS[idx])
        );
        assert_eq!(
            slide.get("targetSection").and_then(|v| v.as_str()),
            Some(expected_sections[idx])
        );

        for k in ["tag", "title", "subtitle", "watchout", "speakerNotes"] {
            assert!(
                slide
                    .get(k)
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false),
                "slide {} missing non-empty `{k}`",
                WALKTHROUGH_SLIDE_IDS[idx]
            );
        }

        let hs = slide.get("heroStat").expect("heroStat");
        assert!(hs
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        assert!(hs
            .get("label")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        let tone = hs.get("tone").and_then(|v| v.as_str()).unwrap_or("");
        assert!(matches!(tone, "good" | "info" | "warn"));

        let pts = slide
            .get("criticalPoints")
            .and_then(|v| v.as_array())
            .expect("criticalPoints array");
        assert!(
            pts.len() >= 3,
            "slide {} must have >= 3 criticalPoints",
            idx + 1
        );

        for lang in ["zh", "es"] {
            let loc = slide.get(lang).expect("localized object");
            for k in [
                "tag",
                "title",
                "subtitle",
                "heroStatLabel",
                "watchout",
                "speakerNotes",
            ] {
                assert!(
                    loc.get(k)
                        .and_then(|v| v.as_str())
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false),
                    "slide {} missing `{lang}.{k}`",
                    WALKTHROUGH_SLIDE_IDS[idx]
                );
            }
            let lpts = loc
                .get("criticalPoints")
                .and_then(|v| v.as_array())
                .expect("localized criticalPoints");
            assert!(lpts.len() >= 3);
        }

        // Ensure Chinese localization never splices raw Latin compass shortcuts like 坐N朝S
        let zh_str = serde_json::to_string(&slide["zh"]).unwrap_or_default();
        assert!(!zh_str.contains("坐N") && !zh_str.contains("朝S"));
    }
}

#[test]
fn test_deterministic_8_slides_secondary_parcel_grounded_and_trilingual() {
    let mut report = synth_secondary_slides_report();
    let deck = build_deterministic_walkthrough_slides(&report);

    assert_valid_deck_schema(&deck, "instant");
    assert_no_forbidden_substrings(&deck, "secondary parcel instant deck");

    let schema = walkthrough_slides_schema();
    let schema_raw = serde_json::to_string(&schema).expect("serialize schema");
    assert!(
        !schema_raw.contains("minItems") && !schema_raw.contains("maxItems"),
        "OpenAI strict JSON schema must not include minItems/maxItems"
    );
    let schema_payload = json!({
        "deckSubtitle": deck["deckSubtitle"],
        "slides": deck["slides"],
    });
    let schema_errs = open_estate::ai::llm::validate_json_schema(&schema, &schema_payload);
    assert!(
        schema_errs.is_empty(),
        "deterministic 8-slide payload failed walkthrough_slides_schema: {schema_errs:?}"
    );

    let slides = deck["slides"].as_array().unwrap();
    let s1 = serde_json::to_string(&slides[0]).unwrap();
    assert!(s1.contains("4 bd"));
    assert!(s1.contains("2,200"));

    let s2 = serde_json::to_string(&slides[1]).unwrap();
    assert!(s2.contains("1958"));
    assert!(s2.contains("000-00-200"));
    assert!(s2.contains("9,200"));
    assert!(s2.contains("65"));
    assert!(s2.contains("142"));

    let s3 = serde_json::to_string(&slides[2]).unwrap();
    assert!(s3.contains("Third-party estimates"));
    assert!(!s3.contains("Zillow"));
    assert!(!s3.contains("Redfin"));

    let s4 = serde_json::to_string(&slides[3]).unwrap();
    assert!(s4.contains("Prop 13"));

    let s5 = serde_json::to_string(&slides[4]).unwrap();
    assert!(s5.contains("1958"));
    assert!(s5.contains("ADU"));

    let s6 = serde_json::to_string(&slides[5]).unwrap();
    assert!(s6.contains("0.58g"));
    assert!(s6.contains("Synthetic Regional Fault Zone"));
    assert!(s6.contains("Zone X"));

    let s7 = serde_json::to_string(&slides[6]).unwrap();
    assert!(s7.contains("Synthetic Elementary School"));
    assert!(s7.contains("Walkability score 70/100"));
    assert!(!s7.contains("Walk Score"));
    assert!(s7.contains("Synth Fresh Market"));

    let s8 = serde_json::to_string(&slides[7]).unwrap();
    assert!(s8.contains("Levers"));

    // Idempotent backfill helper on report
    ensure_walkthrough_slides_on_report(&mut report);
    assert!(report.get("walkthroughSlides").is_some());
    let first = report["walkthroughSlides"].clone();
    ensure_walkthrough_slides_on_report(&mut report);
    assert_eq!(report["walkthroughSlides"], first);
}

#[test]
fn test_primary_parcel_crime_and_non_ca_holdout_no_fabrication() {
    let primary = synth_primary_slides_report();
    let primary_deck = build_deterministic_walkthrough_slides(&primary);
    assert_valid_deck_schema(&primary_deck, "instant");
    assert_no_forbidden_substrings(&primary_deck, "primary parcel");

    let s7_primary = serde_json::to_string(&primary_deck["slides"][6]).unwrap();
    assert!(s7_primary.contains("Metro Area Police Department"));
    assert!(s7_primary.contains("505.3"));

    // Secondary parcel with crime: null must NOT fabricate FBI stats
    let secondary = synth_secondary_slides_report();
    let secondary_deck = build_deterministic_walkthrough_slides(&secondary);
    let s7_secondary = serde_json::to_string(&secondary_deck["slides"][6]).unwrap();
    assert!(s7_secondary.contains("not published"));

    // Out-of-state holdout parcel (`synth-il-60201-vintage-sfr`) stripped to geocode + bedroom count only:
    // must never fabricate sqft, lotSqft, AVMs, Prop 13, or price.
    let il_home = common::synth_holdout_by_id("synth-il-60201-vintage-sfr");
    let il_addr = il_home["address"]
        .as_str()
        .expect("IL holdout address");
    let il_beds = il_home["raw"]["facts"]["beds"]
        .as_f64()
        .expect("IL holdout beds")
        .round() as i64;
    let sparse_il = json!({
        "meta": {
            "slug": il_home["id"],
            "displayAddress": il_addr,
        },
        "geo": il_home["raw"]["geo"],
        "facts": { "beds": il_beds }
    });
    let sparse_deck = build_deterministic_walkthrough_slides(&sparse_il);
    assert_valid_deck_schema(&sparse_deck, "instant");
    assert_no_forbidden_substrings(&sparse_deck, "IL sparse holdout report");

    let sparse_str = serde_json::to_string(&sparse_deck).unwrap();
    assert!(sparse_str.contains(il_addr));
    assert!(sparse_str.contains(&format!("{il_beds} bd")));
    assert!(sparse_str.contains("N/A"));
    assert!(!sparse_str.contains("2,200"));
    assert!(!sparse_str.contains("Zestimate"));
    assert!(!sparse_str.contains("Prop 13"));

    // Full non-CA holdout parcel (`synth-fl-33134-remod-sfr`) also omits CA-only Prop 13.
    let fl_home = common::synth_holdout_by_id("synth-fl-33134-remod-sfr");
    let fl_report = json!({
        "meta": {
            "slug": fl_home["id"],
            "displayAddress": fl_home["address"],
        },
        "geo": fl_home["raw"]["geo"],
        "facts": fl_home["raw"]["facts"],
    });
    let fl_deck = build_deterministic_walkthrough_slides(&fl_report);
    assert_valid_deck_schema(&fl_deck, "instant");
    assert_no_forbidden_substrings(&fl_deck, "FL holdout report");
    let fl_s4 = serde_json::to_string(&fl_deck["slides"][3]).unwrap();
    assert!(!fl_s4.contains("Prop 13"));
}

#[test]
fn test_zero_forbidden_substrings_and_html_render_safety() {
    let mut dirty = synth_secondary_slides_report();
    let retired_brand = concat!("Home", "Lens");
    let mod_primary = concat!("gpt-custom-", "primary");
    let mod_backup = concat!("gpt-custom-", "fallback");
    let prov_sg = concat!("pro", "vider");
    let prov_pl = concat!("pro", "viders");
    let eff_key = concat!("eff", "ort");
    let fb_key = concat!("fallback", "Used");

    dirty["facts"]["lotNote"] = json!(format!(
        "{retired_brand} {prov_sg} metric with {eff_key} max and {mod_primary} note"
    ));
    dirty["summary"]["verdict"] = json!(format!("Checked by {mod_backup} with {fb_key} true"));
    dirty["ai"]["negotiation"] = json!([
        format!("Verify {retired_brand} {prov_pl} report and {mod_primary} {eff_key} notes"),
        "Request roof cert"
    ]);
    let mut existing_ws = json!({
        "generatedAt": "2026-11-12T00:00:00Z",
        "deckTitle": format!("{retired_brand} Deck"),
        "deckSubtitle": format!("Powered by {mod_primary}"),
        "synthesisMode": "instant",
        "model": mod_primary,
        "slides": []
    });
    existing_ws
        .as_object_mut()
        .unwrap()
        .insert(eff_key.to_string(), json!("max"));
    existing_ws
        .as_object_mut()
        .unwrap()
        .insert(fb_key.to_string(), json!(false));
    dirty["walkthroughSlides"] = existing_ws;

    ensure_walkthrough_slides_on_report(&mut dirty);
    let deck = dirty["walkthroughSlides"].clone();
    assert_valid_deck_schema(&deck, "instant");
    assert_no_forbidden_substrings(&deck, "scrubbed dirty report");

    let mut tainted = deck.clone();
    tainted["model"] = json!(mod_backup);
    tainted
        .as_object_mut()
        .unwrap()
        .insert(eff_key.to_string(), json!("high"));
    tainted
        .as_object_mut()
        .unwrap()
        .insert(fb_key.to_string(), json!(true));
    let max_reason = concat!("max ", "reasoning");
    tainted["slides"][0]["speakerNotes"] = json!(format!(
        "Note from {retired_brand} {prov_sg} with claude-3-5-sonnet and {max_reason}"
    ));
    scrub_walkthrough_deck_value(&mut tainted);
    assert_no_forbidden_substrings(&tainted, "direct scrubber");

    let html = render_report_html(&dirty);
    assert!(html.contains("walkthroughSlides"));
    assert!(!open_estate::shared::has_retired_brand(&html));
}

#[tokio::test]
async fn test_keyless_fallback_unreachable_upstream_and_dotenv_hermeticity() {
    let report = synth_secondary_slides_report();

    let empty_env = EnvConfig::default();
    let http = HttpClient::offline_loopback_only(None);
    let instant_deck =
        generate_llm_walkthrough_slides_with_client(&http, &empty_env, &report, None, false).await;
    assert_valid_deck_schema(&instant_deck, "instant");
    assert_no_forbidden_substrings(&instant_deck, "keyless fallback");

    let explicit_instant =
        generate_slides_for_report_json_with_client(&http, &empty_env, report.clone()).await;
    assert_eq!(explicit_instant["ok"], true);
    assert_valid_deck_schema(&explicit_instant["walkthroughSlides"], "instant");
    assert_no_forbidden_substrings(
        &explicit_instant,
        "generate_slides_for_report_json_with_client",
    );

    let api_key_var = concat!("OPENAI_API_", "KEY");
    let dead_env = EnvConfig::from_map(HashMap::from([
        (
            "OPENAI_BASE_URL".to_string(),
            "http://127.0.0.1:9".to_string(),
        ),
        (api_key_var.to_string(), "sk-unreachable-port9".to_string()),
        ("SLIDES_LLM_TIMEOUT_MS".to_string(), "300".to_string()),
        ("SLIDES_TOTAL_TIMEOUT_MS".to_string(), "400".to_string()),
    ]));
    let t0 = std::time::Instant::now();
    let dead_fb =
        generate_llm_walkthrough_slides_with_client(&http, &dead_env, &report, None, true).await;
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "expected rapid fallback on closed port 9"
    );
    assert_valid_deck_schema(&dead_fb, "instant");
    assert_no_forbidden_substrings(&dead_fb, "dead upstream fallback");

    let process_key_present = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();

    if !process_key_present {
        let dotenv_dir = std::env::temp_dir().join(format!("db_slides_dotenv_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dotenv_dir).unwrap();
        std::fs::write(
            dotenv_dir.join(".env"),
            format!("OPENAI_BASE_URL=http://127.0.0.1:9\n{api_key_var}=sk-ambient-dotenv-probe\n"),
        )
        .unwrap();
        assert!(
            EnvConfig::from_process_and_dotenv(Some(&dotenv_dir)).llm_configured(),
            "sanity check: probe .env has full LLM credentials"
        );
        struct CwdGuard(PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        {
            let orig_cwd = std::env::current_dir().unwrap();
            let _guard = CwdGuard(orig_cwd);
            std::env::set_current_dir(&dotenv_dir).unwrap();

            let top_in_dotenv_dir = generate_llm_walkthrough_slides(&report).await;
            assert_valid_deck_schema(&top_in_dotenv_dir, "instant");

            let helper_out = generate_slides_for_report_json(report).await;
            assert_eq!(helper_out["ok"], true);
            assert_valid_deck_schema(&helper_out["walkthroughSlides"], "instant");
            assert_no_forbidden_substrings(&helper_out, "generate_slides_for_report_json");
        }
        let _ = std::fs::remove_dir_all(&dotenv_dir);
    }
}

#[tokio::test]
async fn test_axum_post_report_slides_route_persists_and_guards() {
    let mut report = synth_secondary_slides_report();
    if let Some(obj) = report.as_object_mut() {
        obj.remove("walkthroughSlides");
    }
    let slug = common::SYNTH_SECONDARY_SLUG;

    let tmp_dir = std::env::temp_dir().join(format!("db_slides_test_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("test.db");

    let env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            tmp_dir.to_string_lossy().into_owned(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
    ]));

    let http = HttpClient::offline_loopback_only(None);
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");
    db.put_cached_report(slug, &report, None)
        .await
        .expect("seed report");

    let state = AppState::with_components(
        env.clone(),
        http,
        db.clone(),
        HttpDefenseShield::new(&env),
        tmp_dir.clone(),
        None,
    );
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/slides"))
        .header("host", "127.0.0.1:8080")
        .header("origin", "http://127.0.0.1:8080")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["slug"], slug);
    assert_valid_deck_schema(&body["walkthroughSlides"], "instant");
    assert_no_forbidden_substrings(&body, "POST /api/report/:slug/slides");

    let stored = db
        .get_cached_report(slug)
        .await
        .unwrap()
        .expect("report still cached");
    assert_valid_deck_schema(&stored.report["walkthroughSlides"], "instant");
    assert!(
        stored.rendered_html.is_none(),
        "rendered_html must not be persisted in DB"
    );
    let runtime_html = open_estate::web::render::render_report_html(&stored.report);
    assert!(
        runtime_html.contains("walkthroughSlides"),
        "runtime-rendered HTML must embed walkthroughSlides"
    );

    let mut ai_persisted_report = stored.report.clone();
    ai_persisted_report["walkthroughSlides"]["synthesisMode"] = json!("ai_enhanced");
    ai_persisted_report["walkthroughSlides"]["model"] = json!(concat!("gpt-custom-", "primary"));
    db.put_cached_report(slug, &ai_persisted_report, stored.rendered_html.as_deref())
        .await
        .expect("persist ai_enhanced report");

    let cached_req = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/slides"))
        .header("host", "127.0.0.1:8080")
        .header("origin", "http://127.0.0.1:8080")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let cached_resp = app.clone().oneshot(cached_req).await.unwrap();
    assert_eq!(cached_resp.status(), StatusCode::OK);
    let cached_bytes = axum::body::to_bytes(cached_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let cached_body: Value = serde_json::from_slice(&cached_bytes).unwrap();
    assert_valid_deck_schema(&cached_body["walkthroughSlides"], "ai_enhanced");
    assert_no_forbidden_substrings(&cached_body, "cached ai_enhanced fast-path scrub");

    let refresh_req = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/slides"))
        .header("host", "127.0.0.1:8080")
        .header("origin", "http://127.0.0.1:8080")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"refresh":true}"#))
        .unwrap();
    let refresh_resp = app.clone().oneshot(refresh_req).await.unwrap();
    assert_eq!(refresh_resp.status(), StatusCode::OK);
    let refresh_bytes = axum::body::to_bytes(refresh_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let refresh_body: Value = serde_json::from_slice(&refresh_bytes).unwrap();
    assert_valid_deck_schema(&refresh_body["walkthroughSlides"], "instant");

    let cross_req = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/slides"))
        .header("host", "127.0.0.1:8080")
        .header("origin", "https://evil.example.com")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let cross_resp = app.clone().oneshot(cross_req).await.unwrap();
    assert_eq!(cross_resp.status(), StatusCode::FORBIDDEN);

    let bad_key_req = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/slides"))
        .header("host", "127.0.0.1:8080")
        .header("origin", "http://127.0.0.1:8080")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"promptOverride":"hack"}"#))
        .unwrap();
    let bad_key_resp = app.clone().oneshot(bad_key_req).await.unwrap();
    assert_eq!(bad_key_resp.status(), StatusCode::BAD_REQUEST);

    let nf_req = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{}/slides", common::SYNTH_PRIMARY_SLUG))
        .header("host", "127.0.0.1:8080")
        .header("origin", "http://127.0.0.1:8080")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let nf_resp = app.oneshot(nf_req).await.unwrap();
    assert_eq!(nf_resp.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn test_live_openai_responses_walkthrough_polish_when_keyed() {
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let Some(key) = api_key else {
        eprintln!("skipping live OpenAI walkthrough slides test (OPENAI_API_KEY unset)");
        return;
    };

    let base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let env = EnvConfig::from_map(HashMap::from([
        ("OPENAI_API_KEY".to_string(), key),
        ("OPENAI_BASE_URL".to_string(), base_url),
    ]));
    let http = HttpClient::from_env(&env, None);
    let report = synth_secondary_slides_report();

    let deck = generate_llm_walkthrough_slides_with_client(&http, &env, &report, None, true).await;
    let mode = deck
        .get("synthesisMode")
        .and_then(Value::as_str)
        .unwrap_or("instant");
    assert!(
        matches!(mode, "ai_enhanced" | "instant"),
        "unexpected synthesisMode: {mode}"
    );
    assert_valid_deck_schema(&deck, mode);
    assert_no_forbidden_substrings(&deck, "live OpenAI walkthrough deck");
}
