//! Phase 2 integration tests: Match API (`catalog`, `areas`, `profile`, `score`),
//! browser ProfileStore & 60s Quiz HTML/JS, Report `Your Match` card, Fair Housing & language invariance,
//! and `node --check` validation of all extracted `<script>` blocks.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::scoring::catalog::{Dimension, StyleFamily};
use open_estate::search::{upsert_listings_batch, ListingInput};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::render_report_html;
use open_estate::web::server::{build_router, AppState};
use open_estate::web::ui::render_home_html;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use tower::ServiceExt;

const PRIMARY_SLUG: &str = common::SYNTH_PRIMARY_SLUG;

fn synth_primary_for_match() -> Value {
    let mut rep = common::synth_report_primary();
    rep["modules"]["fengshui"]["data"]["quickScreens"]["tJunction"]["hit"] = json!(false);
    rep
}

fn synth_secondary_for_match() -> Value {
    let mut rep = common::synth_report_secondary();
    rep["facts"]["architecturalStyle"] = json!("Craftsman");
    rep["modules"]["fengshui"]["data"]["quickScreens"]["tJunction"]["hit"] = json!(true);
    if let Some(schools) = rep
        .pointer_mut("/modules/civic/data/schools/assignedOrNearest")
        .and_then(Value::as_array_mut)
    {
        for s in schools {
            s["statePercentile"] = json!(52.0);
        }
    }
    rep
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn test_app_state() -> (AppState, TempDirGuard) {
    let dir = std::env::temp_dir().join(format!("openestate-p2-{}", uuid::Uuid::new_v4()));
    let reports_dir = dir.join("reports");
    std::fs::create_dir_all(&reports_dir).unwrap();
    let env = EnvConfig::default().with_overrides([
        ("SQLITE_DB_PATH", ":memory:".to_string()),
        ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
        ("SYNC_INTERVAL_MIN", "0".to_string()),
    ]);
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .expect("sqlite :memory:");
    db.ensure_schema().await.expect("ensure_schema");
    let http = HttpClient::from_env(&env, None);
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(env, http, db, shield, reports_dir, None);
    (state, TempDirGuard(dir))
}

async fn read_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .expect("body bytes");
    let val: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "invalid JSON ({status}): {e}\n{}",
            String::from_utf8_lossy(&bytes)
        )
    });
    (status, val)
}

fn post_json_req(uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "localhost")
        .header("origin", "http://localhost")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn catalog_returns_en_zh_es_with_identical_weights_10_presets_and_16_styles() {
    let (state, _tmp) = test_app_state().await;
    let router = build_router(state);

    let mut payloads = Vec::new();
    for lang in ["en", "zh-CN", "es"] {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/match/catalog?lang={lang}"))
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, j) = read_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(j["ok"], json!(true));
        assert_eq!(j["lang"], json!(lang));
        assert_eq!(
            j["constraints"],
            json!({ "maxZips": 10, "maxCities": 6, "maxPlaces": 3 })
        );
        assert_eq!(j["dimensions"].as_array().unwrap().len(), Dimension::COUNT);
        assert_eq!(
            j["styleFamilies"].as_array().unwrap().len(),
            StyleFamily::COUNT
        );
        assert_eq!(j["presets"].as_array().unwrap().len(), 10);
        assert_eq!(j["screens"].as_array().unwrap().len(), 7);
        for p in j["presets"].as_array().unwrap() {
            let sum: i64 = p["weights"]
                .as_object()
                .unwrap()
                .values()
                .map(|v| v.as_i64().unwrap())
                .sum();
            assert_eq!(sum, 100, "preset {} sum={sum}", p["key"]);
            assert!(!p["label"].as_str().unwrap().is_empty());
            assert!(!p["description"].as_str().unwrap().is_empty());
        }
        payloads.push(j);
    }

    // Numeric weights, constraints, and defaults must be strictly identical across all languages.
    for lang_idx in 1..payloads.len() {
        assert_eq!(
            payloads[lang_idx]["constraints"],
            payloads[0]["constraints"]
        );
        assert_eq!(payloads[lang_idx]["defaults"], payloads[0]["defaults"]);
        let w0: Vec<_> = payloads[0]["presets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (&p["key"], &p["weights"]))
            .collect();
        let wi: Vec<_> = payloads[lang_idx]["presets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (&p["key"], &p["weights"]))
            .collect();
        assert_eq!(wi, w0);
    }
    assert_ne!(
        payloads[0]["presets"][0]["label"],
        payloads[1]["presets"][0]["label"]
    );
    assert_ne!(
        payloads[0]["presets"][0]["label"],
        payloads[2]["presets"][0]["label"]
    );
}

#[tokio::test]
async fn areas_queries_seeded_listings_and_filters_by_substring() {
    let (state, _tmp) = test_app_state().await;
    let seed = vec![
        ListingInput {
            id: Some("l1".to_string()),
            address: common::SYNTH_PRIMARY_ADDRESS.to_string(),
            city: Some("San Jose".to_string()),
            state: Some("CA".to_string()),
            zip: "95118".to_string(),
            status: Some("for_sale".to_string()),
            price: Some(2_100_000),
            ..Default::default()
        },
        ListingInput {
            id: Some("l2".to_string()),
            address: "104 Synthparcelalpha Ln, San Jose, CA 95118".to_string(),
            city: Some("San Jose".to_string()),
            state: Some("CA".to_string()),
            zip: "95118".to_string(),
            status: Some("sold".to_string()),
            price: Some(2_150_000),
            ..Default::default()
        },
        ListingInput {
            id: Some("l3".to_string()),
            address: common::SYNTH_SECONDARY_ADDRESS.to_string(),
            city: Some("Campbell".to_string()),
            state: Some("CA".to_string()),
            zip: "95008".to_string(),
            status: Some("pending".to_string()),
            price: Some(2_050_000),
            ..Default::default()
        },
        ListingInput {
            id: Some("synth-pa-94303-offmkt".to_string()),
            address: "310 Synthpaloalto Dr, Palo Alto, CA 94303".to_string(),
            city: Some("Palo Alto".to_string()),
            state: Some("CA".to_string()),
            zip: "94303".to_string(),
            status: Some("off-market".to_string()),
            price: Some(2_750_000),
            ..Default::default()
        },
    ];
    upsert_listings_batch(&state.db, None, &EnvConfig::default(), &seed, false)
        .await
        .expect("seed listings");

    let router = build_router(state);

    let (status, all) = read_json(
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/match/areas")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["ok"], json!(true));
    let zips = all["zips"].as_array().unwrap();
    assert_eq!(
        zips.len(),
        2,
        "off_market 94303 should be excluded: {zips:?}"
    );
    assert_eq!(zips[0]["zip"], json!("95118"));
    assert_eq!(zips[0]["count"], json!(2));
    assert_eq!(zips[0]["activeCount"], json!(1));
    assert_eq!(zips[0]["soldCount"], json!(1));
    assert_eq!(zips[1]["zip"], json!("95008"));
    assert_eq!(zips[1]["count"], json!(1));

    let cities = all["cities"].as_array().unwrap();
    assert_eq!(cities.len(), 2);
    assert_eq!(cities[0]["label"], json!("San Jose, CA"));
    assert_eq!(cities[0]["count"], json!(2));
    assert_eq!(cities[1]["label"], json!("Campbell, CA"));
    assert_eq!(cities[1]["count"], json!(1));

    let (st_q, filt) = read_json(
        router
            .oneshot(
                Request::builder()
                    .uri("/api/match/areas?q=campbell")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_q, StatusCode::OK);
    assert_eq!(filt["zips"].as_array().unwrap().len(), 1);
    assert_eq!(filt["zips"][0]["zip"], json!("95008"));
    assert_eq!(filt["cities"].as_array().unwrap().len(), 1);
    assert_eq!(filt["cities"][0]["city"], json!("Campbell"));
}

#[tokio::test]
async fn profile_validates_valid_and_rejects_invalid_and_cross_origin() {
    let (state, _tmp) = test_app_state().await;
    let router = build_router(state);

    // Valid profile
    let valid = json!({
        "version": 1,
        "weights": { "cost": 20, "schools": 30, "commute": 50 },
        "budget": { "monthly": 16000 },
        "downPaymentPct": 25,
        "loanType": "conventional",
        "places": [{ "label": "Work", "lat": 37.3349, "lon": -122.0090, "maxMinutes": 30 }],
        "dealBreakers": ["t_junction"]
    });
    let (st, j) = read_json(
        router
            .clone()
            .oneshot(post_json_req("/api/match/profile", &valid))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(j["ok"], json!(true));
    let wv = j["weightVector"].as_array().unwrap();
    assert_eq!(wv.len(), Dimension::COUNT);
    assert_eq!(wv[Dimension::Cost.index()], json!(20.0));
    assert_eq!(wv[Dimension::Schools.index()], json!(30.0));
    assert_eq!(wv[Dimension::Commute.index()], json!(50.0));
    assert_eq!(j["weight_vector"], j["weightVector"]);

    // Invalid: all weights 0, version 99, missing hoaMaxMonthly
    let invalid = json!({
        "version": 99,
        "weights": { "cost": 0 },
        "dealBreakers": ["hoa_over"]
    });
    let (st_bad, j_bad) = read_json(
        router
            .clone()
            .oneshot(post_json_req("/api/match/profile", &invalid))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_bad, StatusCode::BAD_REQUEST);
    assert_eq!(j_bad["ok"], json!(false));
    assert_eq!(j_bad["code"], json!("INVALID_PROFILE"));
    assert!(j_bad["errors"].as_array().unwrap().len() >= 3);

    // Invalid: out-of-range weight (>100)
    let over_weight = json!({
        "version": 1,
        "weights": { "cost": 150 }
    });
    let (st_over, j_over) = read_json(
        router
            .clone()
            .oneshot(post_json_req("/api/match/profile", &over_weight))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_over, StatusCode::BAD_REQUEST);
    assert_eq!(j_over["code"], json!("INVALID_PROFILE"));

    // Guardrail: cross-origin rejected
    let cross = Request::builder()
        .method("POST")
        .uri("/api/match/profile")
        .header("host", "localhost")
        .header("origin", "https://attacker.example")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&valid).unwrap()))
        .unwrap();
    let (st_cors, j_cors) = read_json(router.oneshot(cross).await.unwrap()).await;
    assert_eq!(st_cors, StatusCode::FORBIDDEN);
    assert_eq!(j_cors["code"], json!("CROSS_ORIGIN_BLOCKED"));
}

#[tokio::test]
async fn score_endpoint_preserves_openestate_score_flips_on_craftsman_flags_tjunction_and_is_invariant(
) {
    let (state, _tmp) = test_app_state().await;
    let primary_rep = synth_primary_for_match();
    let secondary_rep = synth_secondary_for_match();
    let expected_primary_oe = primary_rep["summary"]["scores"]["overall"].clone();
    let expected_secondary_oe = secondary_rep["summary"]["scores"]["overall"].clone();

    // Cache primary in DB so we exercise both `slug` lookup AND inline `report` JSON!
    state
        .db
        .put_cached_report(PRIMARY_SLUG, &primary_rep, None)
        .await
        .expect("cache primary report");

    let router = build_router(state);

    // 1. Balanced profile (primary slug from DB, secondary inline)
    let balanced_profile = json!({
        "version": 1,
        "preset": "balanced",
        "weights": { "cost": 16, "schools": 9, "commute": 13, "convenience": 10, "homefit": 12, "style": 6, "hazards": 9, "crime": 8, "environment": 7, "investment": 8, "fengshui": 2 },
        "budget": { "monthly": 16000 },
        "minBeds": 4, "minBaths": 2, "minSqft": 1900, "minLotSqft": 6000,
        "places": [{ "label": "Apple Park", "lat": 37.3349, "lon": -122.0090, "maxMinutes": 30 }]
    });
    let (st_b, sc_primary_bal) = read_json(
        router
            .clone()
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "slug": PRIMARY_SLUG, "profile": balanced_profile, "lang": "en" }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_b, StatusCode::OK);
    assert_eq!(sc_primary_bal["openEstateScore"], expected_primary_oe);
    assert_eq!(
        sc_primary_bal["explanation"]["commuteDetails"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Preset-only score call (no explicit profile) works deterministically
    let (st_pre, sc_pre) = read_json(
        router
            .clone()
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "slug": PRIMARY_SLUG, "preset": "investor", "lang": "zh-CN" }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_pre, StatusCode::OK);
    assert_eq!(sc_pre["openEstateScore"], expected_primary_oe);
    assert_eq!(sc_pre["explanation"]["lang"], json!("zh-CN"));

    let (st_a, sc_sec_bal) = read_json(
        router
            .clone()
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "report": secondary_rep, "profile": balanced_profile, "lang": "en" }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_a, StatusCode::OK);
    assert_eq!(sc_sec_bal["openEstateScore"], expected_secondary_oe);

    let primary_bal_score = sc_primary_bal["scored"]["matchScore"].as_f64().unwrap();
    let sec_bal_score = sc_sec_bal["scored"]["matchScore"].as_f64().unwrap();
    assert!(
        primary_bal_score > sec_bal_score,
        "Balanced should favor primary ({primary_bal_score}) over secondary ({sec_bal_score})"
    );

    // 2. Design lover who loves Craftsman flips order: secondary > primary
    let craftsman_profile = json!({
        "version": 1,
        "preset": "design_lover",
        "weights": { "cost": 14, "schools": 5, "commute": 10, "convenience": 10, "homefit": 14, "style": 24, "hazards": 6, "crime": 6, "environment": 5, "investment": 4, "fengshui": 2 },
        "budget": { "monthly": 16000 },
        "minBeds": 4, "minBaths": 2, "minSqft": 1900, "minLotSqft": 6000,
        "places": [{ "label": "Apple Park", "lat": 37.3349, "lon": -122.0090, "maxMinutes": 30 }],
        "stylePreferences": { "craftsman": "love" }
    });
    let (_, sc_primary_cr) = read_json(
        router
            .clone()
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "slug": PRIMARY_SLUG, "profile": craftsman_profile }),
            ))
            .await
            .unwrap(),
    )
    .await;
    let (_, sc_sec_cr) = read_json(
        router
            .clone()
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "report": secondary_rep, "profile": craftsman_profile }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(sc_primary_cr["openEstateScore"], expected_primary_oe);
    assert_eq!(sc_sec_cr["openEstateScore"], expected_secondary_oe);
    let primary_cr = sc_primary_cr["scored"]["matchScore"].as_f64().unwrap();
    let sec_cr = sc_sec_cr["scored"]["matchScore"].as_f64().unwrap();
    assert!(
        sec_cr > primary_cr,
        "Design lover + Craftsman should flip order: secondary {sec_cr} > primary {primary_cr}"
    );

    // 3. Feng-shui preset with t_junction deal-breaker flags secondary (and not primary)
    //    AND language invariance across en, zh-CN, es!
    let fengshui_profile = json!({
        "version": 1,
        "preset": "feng_shui",
        "weights": { "cost": 12, "schools": 12, "commute": 7, "convenience": 7, "homefit": 12, "style": 6, "hazards": 8, "crime": 7, "environment": 7, "investment": 7, "fengshui": 15 },
        "budget": { "monthly": 16000 },
        "dealBreakers": ["t_junction"]
    });
    let (_, sc_primary_fs) = read_json(
        router
            .clone()
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "slug": PRIMARY_SLUG, "profile": fengshui_profile, "lang": "en" }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(sc_primary_fs["scored"]["flagged"], json!(false));

    let mut fs_scores = Vec::new();
    let mut fs_gate_texts = Vec::new();
    for lang in ["en", "zh-CN", "es"] {
        let (st_fs, sc_sec_fs) = read_json(
            router
                .clone()
                .oneshot(post_json_req(
                    "/api/match/score",
                    &json!({ "report": secondary_rep, "profile": fengshui_profile, "lang": lang }),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(st_fs, StatusCode::OK);
        assert_eq!(sc_sec_fs["openEstateScore"], expected_secondary_oe);
        assert_eq!(sc_sec_fs["scored"]["flagged"], json!(true));
        let gates = sc_sec_fs["scored"]["gates"].as_array().unwrap();
        assert!(gates.iter().any(|g| g["reason"] == json!("t_junction")));
        fs_scores.push(sc_sec_fs["scored"]["matchScore"].as_f64().unwrap());
        fs_gate_texts.push(gates[0]["text"].as_str().unwrap().to_string());
    }
    assert!((fs_scores[0] - fs_scores[1]).abs() < 1e-12);
    assert!((fs_scores[0] - fs_scores[2]).abs() < 1e-12);
    assert!(fs_gate_texts[1].contains("路冲"));
    assert_ne!(fs_gate_texts[0], fs_gate_texts[1]);
    assert_ne!(fs_gate_texts[0], fs_gate_texts[2]);

    // 4. Fair Housing invariance: poisoned demographic/resident fields cannot alter scores
    let mut poisoned = secondary_rep.clone();
    poisoned["facts"]["demographics"] =
        json!({ "race": "test", "ethnicity": "test", "religion": "test", "medianIncome": 1 });
    poisoned["modules"]["civic"]["data"]["demographics"] =
        json!({ "pctGroupA": 99.0, "medianAge": 22 });
    let (_, sc_sec_poisoned) = read_json(
        router
            .clone()
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "report": poisoned, "profile": balanced_profile, "lang": "en" }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        sc_sec_poisoned["openEstateScore"],
        sc_sec_bal["openEstateScore"]
    );
    assert_eq!(
        sc_sec_poisoned["scored"]["matchScore"],
        sc_sec_bal["scored"]["matchScore"]
    );
    assert_eq!(
        sc_sec_poisoned["scored"]["dimensions"],
        sc_sec_bal["scored"]["dimensions"]
    );

    // 5. Uncached slug returns 404
    let (st_404, j_404) = read_json(
        router
            .oneshot(post_json_req(
                "/api/match/score",
                &json!({ "slug": "999-synthuncached-ave-campbell-ca-95008", "preset": "balanced" }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st_404, StatusCode::NOT_FOUND);
    assert_eq!(j_404["code"], json!("REPORT_NOT_FOUND"));
}

#[test]
fn ui_and_report_template_have_valid_js_and_required_phase2_controls() {
    let home_html = render_home_html(false, "sqlite", "sqlx", false, &[]);
    // Preserve Phase 0/existing home strings + Phase 2 studio markers
    for req in [
        "Instant valuation, natural hazard",
        "Click or Enter to select",
        "US Street Addresses",
        "window.OpenEstateProfileStore",
        "openestate:your_match_profile:v1",
        "30yr_fixed",
        "15yr_fixed",
        "arm_5_1",
        "Official CA public state/federal academic data only",
        "/api/match/catalog",
        "/api/match/areas",
        "/api/match/profile",
        "other_distinctive",
        "sfha_flood",
        "address_digits",
    ] {
        assert!(home_html.contains(req), "home HTML missing `{req}`");
    }

    for sf in StyleFamily::ALL {
        assert!(
            home_html.contains(sf.key()),
            "home HTML missing style family `{}`",
            sf.key()
        );
    }

    let report_html = render_report_html(&synth_primary_for_match());
    for req in [
        "window.OpenEstateProfileStore",
        "openestate:your_match_profile:v1",
        "Your Match",
        "Set Your Preferences",
        "/match#quiz",
        "/api/match/score",
        "Adjust sliders",
        "Sort sections for Your Match",
        "class=\"sec\"",
    ] {
        assert!(report_html.contains(req), "report template missing `{req}`");
    }

    // Validate extracted <script> JS blocks via `node --check`
    fn check_js_blocks(html: &str, label: &str) {
        let mut rest = html;
        let mut count = 0usize;
        while let Some(start) = rest.find("<script") {
            let after_tag = &rest[start..];
            let Some(tag_end) = after_tag.find('>') else {
                break;
            };
            let open_tag = &after_tag[..tag_end + 1];
            let body_start = &after_tag[tag_end + 1..];
            let Some(close_idx) = body_start.find("</script>") else {
                break;
            };
            let body = &body_start[..close_idx];
            rest = &body_start[close_idx + "</script>".len()..];
            if open_tag.contains("src=")
                || open_tag.contains("application/json")
                || body.trim().is_empty()
            {
                continue;
            }
            count += 1;
            let tmp_js = std::env::temp_dir().join(format!("openestate-check-{label}-{count}.js"));
            std::fs::write(&tmp_js, body).expect("write temp js");
            let out = Command::new("node")
                .arg("--check")
                .arg(&tmp_js)
                .output()
                .expect("run node --check");
            let _ = std::fs::remove_file(&tmp_js);
            assert!(
                out.status.success(),
                "{label} script[{count}] failed node --check:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert!(count >= 1, "{label} had no inline <script> blocks");
    }

    check_js_blocks(&home_html, "home");
    check_js_blocks(&report_html, "report");
}
