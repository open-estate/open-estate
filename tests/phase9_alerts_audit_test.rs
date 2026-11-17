//! Phase 9 Integration & Audit Tests (100% Zero-Mock):
//! 1. `user_profiles` & `match_feedback` schema parity (SQLite + OCI MySQL HeatWave DDL), CRUD, and `migrate_database` parity
//! 2. Account profile (`GET /api/me/profile`, `PUT /api/me/profile`) & feedback (`POST /api/match/feedback`) HTTP handlers
//! 3. Match-ranked alerts (`create_subscription_with_profile`, `evaluate_alerts_for_listings`, `build_match_alert_email`, `build_lettre_message` RFC 5322/8058 MIME inspection — zero `MockSmtpServer`)
//! 4. Fair Housing demographic-poison audit across all 10 presets and 3 languages (`en`, `zh-CN`, `es`)
//! 5. `sync.rs` synthetic benchmark comp exclusion on synthetic `100-synthparcelalpha-ln-san-jose-ca-95118`

#![recursion_limit = "512"]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use open_estate::db::{
    build_match_feedback_upsert, build_user_profile_upsert, migrate_database, mysql_ddl,
    sqlite_ddl, DbStore, MatchFeedbackRow, TABLE_NAMES,
};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::scoring::catalog::{Dimension, Screen};
use open_estate::scoring::engine::CommuteMatrix;
use open_estate::scoring::factors::HomeFactors;
use open_estate::scoring::presets::PRESETS;
use open_estate::scoring::profile::Profile;
use open_estate::search::alerts::{
    build_match_alert_email, create_subscription_with_profile, evaluate_alerts_for_listings,
    fair_housing_audit, fair_housing_audit_reports, poison_report_with_demographics,
    query_listings, suggest_weight_adjustments, AlertMatchedItem, CreateSubscriptionRequest,
    FeedbackAction, FeedbackSample,
};
use open_estate::search::mailer::{build_lettre_message, EmailRequest};
use open_estate::search::matcher::parse_dimension_snapshot;
use open_estate::search::sync::{
    extract_listings_from_report, ingest_report_into_db, is_synthetic_comp_entry,
    upsert_listings_batch, ListingInput,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tower::ServiceExt;
use uuid::Uuid;

fn temp_sqlite_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("openestate-phase9-{tag}-{}.sqlite", Uuid::new_v4()))
}

fn cleanup_sqlite_files(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    let _ = std::fs::remove_file(format!("{}-shm", p.display()));
}

async fn test_app_state() -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("openestate-p9-state-{}", Uuid::new_v4()));
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
    (state, dir)
}

fn profile_test_router(state: AppState) -> Router {
    build_router(state)
}

async fn read_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
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

mod common;

fn fixture_report(slug: &str) -> Value {
    common::synth_report_for_slug(slug)
}

fn holdout_home_ids() -> Vec<String> {
    common::synth_holdout_homes()
        .into_iter()
        .filter_map(|h| h["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn test_1_user_profiles_and_match_feedback_ddl_heatwave_and_migration_parity() {
    assert!(TABLE_NAMES.contains(&"user_profiles"));
    assert!(TABLE_NAMES.contains(&"match_feedback"));
    assert_eq!(mysql_ddl().len(), TABLE_NAMES.len());

    let sqlite_all = sqlite_ddl().join("\n");
    for tbl in ["user_profiles", "match_feedback"] {
        assert!(sqlite_all.contains(&format!("CREATE TABLE IF NOT EXISTS {tbl}")));
        let my_stmt = mysql_ddl()
            .iter()
            .find(|s| s.contains(&format!("CREATE TABLE IF NOT EXISTS {tbl}")))
            .unwrap_or_else(|| panic!("Missing MySQL DDL for {tbl}"));
        assert!(my_stmt.contains("PRIMARY KEY"));
        assert!(!my_stmt.contains("LONGTEXT"));
        assert!(
            my_stmt.contains("ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci")
        );
        assert!(!my_stmt.contains(" JSON "));

        let low = my_stmt.to_ascii_lowercase();
        for forbidden in [
            "race",
            "ethnicity",
            "religion",
            "ancestry",
            "demographic",
            "familial",
        ] {
            assert!(
                !low.contains(forbidden),
                "Prohibited demographic column `{forbidden}` in {tbl} DDL"
            );
        }
    }

    for fn_res in [
        build_user_profile_upsert("mysql"),
        build_match_feedback_upsert("mysql"),
    ] {
        assert!(fn_res.contains("AS new_row ON DUPLICATE KEY UPDATE"));
        assert!(!fn_res.contains("VALUES("));
    }
    assert!(build_user_profile_upsert("sqlite").contains("ON CONFLICT(user_sk) DO UPDATE SET"));
    assert!(build_match_feedback_upsert("sqlite").contains("ON CONFLICT(feedback_sk) DO UPDATE SET"));

    let src_path = temp_sqlite_path("src");
    let dst_path = temp_sqlite_path("dst");
    cleanup_sqlite_files(&src_path);
    cleanup_sqlite_files(&dst_path);

    let src = DbStore::connect_url(&src_path.to_string_lossy(), Some("sqlite"))
        .await
        .unwrap();
    let dst = DbStore::connect_url(&dst_path.to_string_lossy(), Some("sqlite"))
        .await
        .unwrap();
    src.ensure_schema().await.unwrap();
    dst.ensure_schema().await.unwrap();

    let prof = PRESETS[1].to_profile();
    let prof_json = serde_json::to_string(&prof).unwrap();
    let inserted = src
        .upsert_user_profile("buyer-mig-001", &prof_json, 1)
        .await
        .unwrap();
    assert_eq!(inserted.user_id, "buyer-mig-001");
    assert_eq!(inserted.version, 1);

    // Updating preserves original created_at.
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let mut prof2 = prof.clone();
    prof2.min_beds = Some(4.0);
    let prof2_json = serde_json::to_string(&prof2).unwrap();
    let updated = src
        .upsert_user_profile("buyer-mig-001", &prof2_json, 1)
        .await
        .unwrap();
    assert_eq!(updated.created_at, inserted.created_at);

    // Insert > 200 match_feedback rows anchored to real holdout IDs to exercise 200-row streaming batch migration.
    let real_ids = holdout_home_ids();
    assert!(!real_ids.is_empty());
    for i in 0..205usize {
        let action = match i % 4 {
            0 => "save",
            1 => "dismiss",
            2 => "tour",
            _ => "hide",
        };
        let base_id = &real_ids[i % real_ids.len()];
        src.insert_match_feedback(&MatchFeedbackRow {
            id: format!("fb-{i:03}"),
            user_id: "buyer-mig-001".to_string(),
            listing_id: format!("{base_id}-{i:03}"),
            action: action.to_string(),
            dimension_snapshot_json: Some(json!({ "schools": 88.0, "cost": 72.0 }).to_string()),
            reason: Some(format!("Buyer tour note {i}")),
            created_at: format!("2026-11-11T09:{:02}:{:02}Z", i / 60, i % 60),
        })
        .await
        .unwrap();
    }

    let summary = migrate_database(&src, &dst).await.unwrap();
    assert_eq!(summary.tables.len(), TABLE_NAMES.len());
    assert_eq!(summary.tables.get("user_profiles").copied(), Some(1));
    assert_eq!(summary.tables.get("match_feedback").copied(), Some(205));

    let dst_prof = dst
        .get_user_profile("buyer-mig-001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dst_prof.profile_json, prof2_json);
    assert_eq!(dst_prof.created_at, inserted.created_at);

    let dst_fb = dst
        .list_match_feedback_for_user("buyer-mig-001", 300)
        .await
        .unwrap();
    assert_eq!(dst_fb.len(), 205);

    // Idempotent second run.
    let summary2 = migrate_database(&src, &dst).await.unwrap();
    assert_eq!(summary2.tables, summary.tables);

    cleanup_sqlite_files(&src_path);
    cleanup_sqlite_files(&dst_path);
}

#[tokio::test]
async fn test_2_profile_api_handlers_get_put_and_feedback_suggestions() {
    let (state, dir) = test_app_state().await;
    let app = profile_test_router(state.clone());

    // Missing user ID -> 400
    let (st, body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("MISSING_USER_ID"));

    // Invalid user ID -> 400
    let (st, body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("x-user-id", "../bad user!")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_USER_ID"));

    // Unstored valid user ID -> 404 with promptUploadLocalProfile: true
    let (st, body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("x-user-id", "anon_user-95118")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["found"], json!(false));
    assert_eq!(body["promptUploadLocalProfile"], json!(true));
    assert_eq!(body["profile"], Value::Null);

    // Reject PUT with prohibited demographic keys -> 400
    let (st, _) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("content-type", "application/json")
                    .header("x-user-id", "anon_user-95118")
                    .body(Body::from(
                        json!({
                            "version": 1,
                            "weights": { "cost": 50, "schools": 50 },
                            "racePct": 90
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Reject PUT with invalid weights -> 400 INVALID_PROFILE
    let (st, body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("content-type", "application/json")
                    .header("x-user-id", "anon_user-95118")
                    .body(Body::from(
                        json!({
                            "version": 1,
                            "weights": { "cost": 0, "schools": 0 }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_PROFILE"));

    // Valid PUT persists profile
    let mut prof = PRESETS[0].to_profile();
    prof.budget.max_price = Some(2_100_000.0);
    prof.min_beds = Some(4.0);
    prof.weights.insert(Dimension::Cost, 30.0);
    prof.weights.insert(Dimension::Schools, 10.0);
    prof.weights.insert(Dimension::Hazards, 8.0);

    let (st, put_body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("content-type", "application/json")
                    .header("x-user-id", "anon_user-95118")
                    .body(Body::from(serde_json::to_vec(&prof).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(put_body["ok"], json!(true));
    assert_eq!(
        put_body["profile"]["budget"]["maxPrice"],
        json!(2_100_000.0)
    );

    // GET via query `?sessionKey=` returns stored profile
    let (st, get_body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me/profile?sessionKey=anon_user-95118")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(get_body["found"], json!(true));
    assert_eq!(get_body["promptUploadLocalProfile"], json!(false));
    assert_eq!(get_body["profile"]["minBeds"], json!(4.0));

    // Wrapped `{ "profile": ... }` PUT update preserves createdAt and updates profile
    let created_at_orig = put_body["createdAt"].clone();
    let mut prof_updated = prof.clone();
    prof_updated.min_baths = Some(3.0);
    let (st, put2_body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("content-type", "application/json")
                    .header("x-user-id", "anon_user-95118")
                    .body(Body::from(json!({ "profile": prof_updated }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(put2_body["createdAt"], created_at_orig);
    assert_eq!(put2_body["profile"]["minBaths"], json!(3.0));

    // Verify `X-Open Estate-User-Id` header works identically to `X-User-Id`
    let (st, db_hdr_body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("x-openestate-user-id", "anon_user-95118")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(db_hdr_body["found"], json!(true));
    assert_eq!(db_hdr_body["profile"]["minBaths"], json!(3.0));

    // Verify `X-Session-Key` and `?userId=` resolve user_id cleanly on standalone `/api/me/profile`
    let (st, sess_put) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("x-session-key", "sess-sync-buyer-95118")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "profile": prof_updated }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sess_put["ok"], json!(true));

    let (st, sess_get) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me/profile?userId=sess-sync-buyer-95118")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sess_get["found"], json!(true));
    assert_eq!(sess_get["profile"]["minBaths"], json!(3.0));

    // Feedback validation: invalid action -> 400
    let (st, body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/match/feedback")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("content-type", "application/json")
                    .header("x-user-id", "anon_user-95118")
                    .body(Body::from(
                        json!({
                            "listingId": "synth-ca-95118-arterial-sfr",
                            "action": "favorite_invalid"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_FEEDBACK_ACTION"));

    // Feedback validation: poisoned demographic snapshot -> 400
    let (st, body) = read_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/match/feedback")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("content-type", "application/json")
                    .header("x-user-id", "anon_user-95118")
                    .body(Body::from(
                        json!({
                            "listingId": "synth-ca-95118-arterial-sfr",
                            "action": "save",
                            "dimensionSnapshot": { "schools": 90, "racePct": 70 }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], json!("INVALID_DIMENSION_SNAPSHOT"));

    // Persist 2 saved/toured synthetic homes (strong schools/hazards) + 2 dismissed/hidden synthetic homes (weak schools/hazards, cheap)
    let feedback_events = [
        (
            "synth-ca-95118-arterial-sfr",
            "save",
            json!({ "schools": 94.0, "hazards": 88.0, "cost": 56.0 }),
            "Strong Union ESD schools",
        ),
        (
            "synth-ca-95118-culdesac-sfr",
            "tour",
            json!([
                { "dimension": "schools", "utility": 90.0, "estimated": false },
                { "dimension": "hazards", "utility": 86.0, "estimated": false },
                { "dimension": "cost", "utility": 60.0, "estimated": false }
            ]),
            "Booked weekend tour",
        ),
        (
            "synth-ca-95008-compact-sfr",
            "dismiss",
            json!({ "schools": 44.0, "hazards": 46.0, "cost": 92.0 }),
            "Weak school feeder",
        ),
        (
            "synth-ca-94303-lower-sfr",
            "hide",
            json!({ "schools": 46.0, "hazards": 48.0, "cost": 88.0 }),
            "High flood hazard",
        ),
    ];

    let mut last_fb_body = Value::Null;
    for (lid, act, snap, reason) in feedback_events {
        let (st, resp_j) = read_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/match/feedback")
                        .header("host", "localhost")
                        .header("origin", "http://localhost")
                        .header("content-type", "application/json")
                        .header("x-user-id", "anon_user-95118")
                        .body(Body::from(
                            json!({
                                "listingId": lid,
                                "action": act,
                                "dimensionSnapshot": snap,
                                "reason": reason,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "feedback {lid}: {resp_j}");
        assert_eq!(resp_j["ok"], json!(true));
        last_fb_body = resp_j;
    }

    assert_eq!(last_fb_body["feedbackCount"], json!(4));
    let suggestions = last_fb_body["weightSuggestions"].as_array().unwrap();
    assert!(
        !suggestions.is_empty(),
        "Expected weight suggestions from contrast feedback"
    );

    let schools_sug = suggestions
        .iter()
        .find(|s| s["dimension"] == json!("schools"))
        .expect("schools suggestion");
    assert_eq!(schools_sug["direction"], json!("increase"));
    assert!(
        schools_sug["suggestedWeight"].as_f64().unwrap()
            > schools_sug["currentWeight"].as_f64().unwrap()
    );

    let cost_sug = suggestions
        .iter()
        .find(|s| s["dimension"] == json!("cost"))
        .expect("cost suggestion");
    assert_eq!(cost_sug["direction"], json!("decrease"));
    assert!(
        cost_sug["suggestedWeight"].as_f64().unwrap() < cost_sug["currentWeight"].as_f64().unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_3_match_ranked_alerts_scoring_sorting_gates_and_rfc5322_mime_email() {
    // Zero `MockSmtpServer`: configure `SMTP_FROM` without `SMTP_HOST` so `evaluate_alerts_for_listings`
    // runs full profile scoring, deal-breaker gating, and DB notification recording (`queued_no_smtp`),
    // and verify the exact RFC 5322 / RFC 8058 MIME wire message directly via `lettre::Message::formatted()`.
    let env = EnvConfig::from_map(HashMap::from([(
        "SMTP_FROM".to_string(),
        "Open Estate Alerts <alerts@open-estate.local>".to_string(),
    )]));
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    let http = HttpClient::offline_loopback_only(None);

    let mut profile = PRESETS[1].to_profile(); // Education-first
    profile.budget.max_price = Some(2_100_000.0);
    profile.min_beds = Some(4.0);
    profile.min_baths = Some(2.0);
    profile.min_sqft = Some(1700.0);
    profile.deal_breakers.push(Screen::SfhaFlood);
    profile.weights.insert(Dimension::Schools, 40.0);
    profile.weights.insert(Dimension::Cost, 25.0);
    profile.weights.insert(Dimension::Homefit, 15.0);

    let sub_res = create_subscription_with_profile(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "ranked-alerts@open-estate.local".to_string(),
            name: "Alex".to_string(),
            zips: vec![
                "95118".to_string(),
                "60201".to_string(),
                "94303".to_string(),
                "94025".to_string(),
            ],
            requirements: "4 bed home under $2.1M with strong public schools outside flood zone"
                .to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "instant".to_string(),
            send_welcome_email: false,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        Some(&profile),
    )
    .await
    .unwrap();
    assert_eq!(sub_res["ok"], json!(true));
    assert_eq!(sub_res["subscription"]["hasProfile"], json!(true));

    // Ingest 4 synthetic homes in mixed order:
    // 1. `synth-ca-95118-mid-sfr` (`103 Synthholdout03 Way, San Jose, CA 95118`, $2,049,888, 4bd/2ba, 1711 sqft, 1974) -> Good match
    // 2. `synth-ca-94303-lower-sfr` (`111 Synthholdout11 St, East Palo Alto, CA 94303`, $1,048,800, FEMA Zone AE) -> Excluded by `SfhaFlood`
    // 3. `synth-ca-94025-upper-sfr` (`109 Synthholdout09 Way, Menlo Park, CA 94025`, $3,800,000, 3bd/2ba) -> Excluded (81% over budget)
    // 4. `synth-il-60201-newbuild-sfr` (`119 Synthholdout19 Ave, Evanston, IL 60201`, $1,515,000, 4bd/2.5ba, 2070 sqft, 2018) -> Strong top match
    let batch = upsert_listings_batch(
        &db,
        None,
        &env,
        &[
            ListingInput {
                id: Some("synth-ca-95118-mid-sfr".to_string()),
                street: Some("103 Synthholdout03 Way".to_string()),
                address: "103 Synthholdout03 Way, San Jose, CA 95118".to_string(),
                city: Some("San Jose".to_string()),
                state: Some("CA".to_string()),
                zip: "95118".to_string(),
                status: Some("for_sale".to_string()),
                price: Some(2_049_888),
                list_price: Some(2_049_888),
                beds: Some(4.0),
                baths: Some(2.0),
                sqft: Some(1711),
                year_built: Some(1974),
                flood_zone: Some("X".to_string()),
                school_score: Some(7.6),
                property_type: Some("Single-Family Detached".to_string()),
                ..Default::default()
            },
            ListingInput {
                id: Some("synth-ca-94303-lower-sfr".to_string()),
                street: Some("111 Synthholdout11 St".to_string()),
                address: "111 Synthholdout11 St, East Palo Alto, CA 94303".to_string(),
                city: Some("East Palo Alto".to_string()),
                state: Some("CA".to_string()),
                zip: "94303".to_string(),
                status: Some("for_sale".to_string()),
                price: Some(1_048_800),
                list_price: Some(1_048_800),
                beds: Some(4.0),
                baths: Some(2.0),
                sqft: Some(1800),
                year_built: Some(1951),
                flood_zone: Some("AE".to_string()),
                school_score: Some(9.6),
                property_type: Some("Single-Family Detached".to_string()),
                ..Default::default()
            },
            ListingInput {
                id: Some("synth-ca-94025-upper-sfr".to_string()),
                street: Some("109 Synthholdout09 Way".to_string()),
                address: "109 Synthholdout09 Way, Menlo Park, CA 94025".to_string(),
                city: Some("Menlo Park".to_string()),
                state: Some("CA".to_string()),
                zip: "94025".to_string(),
                status: Some("for_sale".to_string()),
                price: Some(3_800_000),
                list_price: Some(3_800_000),
                beds: Some(3.0),
                baths: Some(2.0),
                sqft: Some(1500),
                year_built: Some(1950),
                flood_zone: Some("X".to_string()),
                school_score: Some(4.5),
                property_type: Some("Single-Family Detached".to_string()),
                ..Default::default()
            },
            ListingInput {
                id: Some("synth-il-60201-newbuild-sfr".to_string()),
                street: Some("119 Synthholdout19 Ave".to_string()),
                address: "119 Synthholdout19 Ave, Evanston, IL 60201".to_string(),
                city: Some("Evanston".to_string()),
                state: Some("IL".to_string()),
                zip: "60201".to_string(),
                status: Some("for_sale".to_string()),
                price: Some(1_515_000),
                list_price: Some(1_515_000),
                beds: Some(4.0),
                baths: Some(2.5),
                sqft: Some(2070),
                year_built: Some(2018),
                flood_zone: Some("X".to_string()),
                school_score: Some(9.5),
                property_type: Some("Single-Family Detached".to_string()),
                ..Default::default()
            },
        ],
        true,
    )
    .await
    .unwrap();

    let eval_res = evaluate_alerts_for_listings(
        &db,
        &http,
        &env,
        &batch.changed_listings,
        "https://openestate.org",
    )
    .await
    .unwrap();

    assert_eq!(eval_res["emailsSent"], json!(0));
    assert_eq!(eval_res["notificationsTriggered"], json!(2));
    let detail = &eval_res["details"][0];
    assert_eq!(detail["usedProfileScoring"], json!(true));
    assert_eq!(detail["deliveryStatus"], json!("queued_no_smtp"));
    assert_eq!(
        detail["matchedListingIds"],
        json!(["synth-il-60201-newbuild-sfr", "synth-ca-95118-mid-sfr"]),
        "Alert items must be sorted highest Match Score first and exclude flood/over-budget homes"
    );
    let scores = detail["matchedScores"].as_array().unwrap();
    assert!(scores[0].as_i64().unwrap() > scores[1].as_i64().unwrap());

    // Single-home follow-up alert event (`synth-ca-95118-culdesac-sfr`: 104 Synthholdout04 Ct, San Jose, CA 95118)
    let single_batch = upsert_listings_batch(
        &db,
        None,
        &env,
        &[ListingInput {
            id: Some("synth-ca-95118-culdesac-sfr".to_string()),
            street: Some("104 Synthholdout04 Ct".to_string()),
            address: "104 Synthholdout04 Ct, San Jose, CA 95118".to_string(),
            city: Some("San Jose".to_string()),
            state: Some("CA".to_string()),
            zip: "95118".to_string(),
            status: Some("for_sale".to_string()),
            price: Some(2_015_000),
            list_price: Some(2_015_000),
            beds: Some(4.0),
            baths: Some(3.0),
            sqft: Some(1766),
            year_built: Some(1972),
            flood_zone: Some("X".to_string()),
            school_score: Some(9.2),
            property_type: Some("Single-Family Detached".to_string()),
            ..Default::default()
        }],
        true,
    )
    .await
    .unwrap();
    let single_eval = evaluate_alerts_for_listings(
        &db,
        &http,
        &env,
        &single_batch.changed_listings,
        "https://openestate.org",
    )
    .await
    .unwrap();
    assert_eq!(single_eval["notificationsTriggered"], json!(1));
    assert_eq!(
        single_eval["details"][0]["matchedListingIds"],
        json!(["synth-ca-95118-culdesac-sfr"])
    );

    // Direct `build_match_alert_email` + `lettre::Message::formatted()` RFC 5322 / RFC 8058 inspection
    let payload = build_match_alert_email(
        Some("Alex"),
        "4 bed home under $2.1M with strong public schools",
        &["95118".to_string(), "60201".to_string()],
        &[
            AlertMatchedItem {
                event_type: "new_listing".to_string(),
                old_price: None,
                event_key: "k-mid".to_string(),
                listing: json!({
                    "id": "synth-ca-95118-mid-sfr",
                    "address": "103 Synthholdout03 Way, San Jose, CA 95118",
                    "price": 2_049_888
                }),
                match_score: scores[1].as_i64().unwrap(),
                match_tier: "Good match".to_string(),
                why_it_matches: "Within $2.1M budget · 4 beds · Zone X".to_string(),
                tradeoffs: None,
            },
            AlertMatchedItem {
                event_type: "new_listing".to_string(),
                old_price: None,
                event_key: "k-newbuild".to_string(),
                listing: json!({
                    "id": "synth-il-60201-newbuild-sfr",
                    "address": "119 Synthholdout19 Ave, Evanston, IL 60201",
                    "price": 1_515_000
                }),
                match_score: scores[0].as_i64().unwrap(),
                match_tier: "Strong match".to_string(),
                why_it_matches:
                    "Strong public schools · 2018 construction · $585k below max budget".to_string(),
                tradeoffs: None,
            },
        ],
        "https://openestate.org",
        "tok-95118-unsubscribe",
    );
    let text_high = payload
        .text
        .find("119 Synthholdout19 Ave, Evanston, IL 60201")
        .unwrap();
    let text_low = payload
        .text
        .find("103 Synthholdout03 Way, San Jose, CA 95118")
        .unwrap();
    assert!(
        text_high < text_low,
        "Highest Match Score must appear first in plain-text body"
    );
    let html_high = payload
        .html
        .find("119 Synthholdout19 Ave, Evanston, IL 60201")
        .unwrap();
    let html_low = payload
        .html
        .find("103 Synthholdout03 Way, San Jose, CA 95118")
        .unwrap();
    assert!(
        html_high < html_low,
        "Highest Match Score must appear first in HTML body"
    );
    assert!(payload.html.contains("View Home"));
    assert!(payload.html.contains("Manage alerts"));
    assert!(!payload.html.contains("Why it matches"));
    assert!(!payload.html.contains("Trade-off"));
    assert!(!payload.html.contains("AI parsed filter"));
    assert!(!payload.html.contains("% Match"));

    let mail_req = EmailRequest {
        to: vec!["ranked-alerts@open-estate.local".to_string()],
        subject: payload.subject.clone(),
        text: Some(payload.text.clone()),
        html: Some(payload.html.clone()),
        reply_to: None,
        unsubscribe_url: Some(payload.unsubscribe_url.clone()),
    };
    let (msg, rcpts, ehlo_domain) =
        build_lettre_message(&env, &mail_req).expect("build RFC 5322 lettre::Message");
    assert_eq!(rcpts, vec!["ranked-alerts@open-estate.local".to_string()]);
    assert_eq!(ehlo_domain, "open-estate.local");
    let rfc5322 = String::from_utf8(msg.formatted()).expect("UTF-8 RFC 5322 message");
    let unfolded_rfc5322 = rfc5322.replace("\r\n ", " ");
    assert!(
        unfolded_rfc5322.contains("From:")
            && unfolded_rfc5322.contains("Open Estate Alerts")
            && unfolded_rfc5322.contains("<alerts@open-estate.local>"),
        "unexpected From header in RFC 5322:\n{unfolded_rfc5322}"
    );
    assert!(unfolded_rfc5322.contains("To: ranked-alerts@open-estate.local"));
    assert!(unfolded_rfc5322.contains(
        "List-Unsubscribe: <https://openestate.org/api/subscriptions/unsubscribe?token=tok-95118-unsubscribe>"
    ));
    assert!(unfolded_rfc5322.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"));
    assert!(rfc5322.contains("multipart/alternative"));
    assert!(rfc5322.contains("119 Synthholdout19 Ave"));
    assert!(rfc5322.contains("103 Synthholdout03 Way"));
    assert!(!rfc5322.contains("111 Synthholdout11 St"));
    assert!(!rfc5322.contains("109 Synthholdout09 Way"));
}

#[test]
fn test_4_fair_housing_audit_all_presets_languages_and_demographic_poison() {
    let rep_a = fixture_report(common::SYNTH_SECONDARY_SLUG);
    let rep_b = fixture_report(common::SYNTH_PRIMARY_SLUG);

    let hf_a = HomeFactors::from_report(&rep_a);
    let hf_b = HomeFactors::from_report(&rep_b);
    let mut commute = CommuteMatrix::new();
    commute.set(&hf_a.id, 0, 13.6);
    commute.set(&hf_b.id, 0, 18.9);

    let report_audit = fair_housing_audit_reports(&[rep_a.clone(), rep_b.clone()], &commute);
    assert!(
        report_audit.passed,
        "report audit failed: {:?}",
        report_audit.failures
    );
    assert_eq!(report_audit.presets_tested, 10);
    assert_eq!(report_audit.languages_tested, vec!["en", "zh-CN", "es"]);
    assert!(report_audit.language_invariant);
    assert!(report_audit.demographic_poison_invariant);
    assert!(report_audit.max_score_delta <= 1e-9);
    for field in [
        "racePct",
        "ancestry",
        "religion",
        "familialStatus",
        "protectedTrait",
    ] {
        assert!(report_audit
            .poisoned_fields_checked
            .contains(&field.to_string()));
    }

    let factors_audit = fair_housing_audit(&[hf_a, hf_b], &commute);
    assert!(
        factors_audit.passed,
        "factors audit failed: {:?}",
        factors_audit.failures
    );
    assert_eq!(factors_audit.presets_tested, 10);
    assert!(factors_audit.language_invariant);
    assert!(factors_audit.demographic_poison_invariant);
    assert!(factors_audit.max_score_delta <= 1e-9);

    // Direct HomeFactors::from_report demographic-poison invariance check.
    let poisoned_a = poison_report_with_demographics(&rep_a, 0);
    let poisoned_b = poison_report_with_demographics(&rep_b, 1);
    assert_eq!(
        HomeFactors::from_report(&rep_a),
        HomeFactors::from_report(&poisoned_a)
    );
    assert_eq!(
        HomeFactors::from_report(&rep_b),
        HomeFactors::from_report(&poisoned_b)
    );

    // Verify parse_dimension_snapshot & suggest_weight_adjustments reject demographic poison.
    assert!(parse_dimension_snapshot(&json!({ "schools": 90.0, "racePct": 50.0 })).is_err());
    assert!(parse_dimension_snapshot(&json!({ "familialStatus": 80.0 })).is_err());

    let base_profile = Profile {
        weights: BTreeMap::from([(Dimension::Schools, 15.0), (Dimension::Cost, 25.0)]),
        ..Default::default()
    };
    let suggestions = suggest_weight_adjustments(
        &base_profile,
        &[
            FeedbackSample {
                listing_id: "synth-ca-95118-arterial-sfr".to_string(),
                action: FeedbackAction::Save,
                dimension_utilities: BTreeMap::from([
                    (Dimension::Schools, 95.0),
                    (Dimension::Cost, 60.0),
                ]),
                estimated_dimensions: Vec::new(),
                reason: None,
            },
            FeedbackSample {
                listing_id: "synth-ca-95008-compact-sfr".to_string(),
                action: FeedbackAction::Dismiss,
                dimension_utilities: BTreeMap::from([
                    (Dimension::Schools, 40.0),
                    (Dimension::Cost, 90.0),
                ]),
                estimated_dimensions: Vec::new(),
                reason: None,
            },
        ],
    );
    assert!(suggestions
        .iter()
        .any(|s| s.dimension == Dimension::Schools && s.direction == "increase"));
    assert!(suggestions
        .iter()
        .any(|s| s.dimension == Dimension::Cost && s.direction == "decrease"));
}

#[tokio::test]
async fn test_5_sync_skips_synthetic_benchmark_comps_forever() {
    assert!(is_synthetic_comp_entry(&json!({ "isSynthetic": true })));
    assert!(is_synthetic_comp_entry(
        &json!({ "source": "Parcel + ZHVI benchmark" })
    ));
    assert!(is_synthetic_comp_entry(
        &json!({ "dealType": "Neighbor parcel + ZIP benchmark" })
    ));
    assert!(!is_synthetic_comp_entry(&json!({
        "address": "301 Synthcomp01 Way, San Jose, CA 95118",
        "price": 2_049_888,
        "dealType": "Recorded sale",
        "source": "Santa Clara County Recorder",
        "isSynthetic": false
    })));

    // Ground on the synthetic primary report + 1 verified recorded comp (`301 Synthcomp01 Way`)
    // alongside legacy benchmark/proxy comp shapes that `extract_listings_from_report` MUST
    // permanently reject.
    let mut report = fixture_report(common::SYNTH_PRIMARY_SLUG);
    if let Some(facts_obj) = report.get_mut("facts").and_then(Value::as_object_mut) {
        facts_obj.insert("nearbyListings".to_string(), json!([]));
        facts_obj.insert(
            "comps".to_string(),
            json!([
                {
                    "address": "301 Synthcomp01 Way, San Jose, CA 95118",
                    "soldDate": "2025-02-14",
                    "price": 2_049_888,
                    "beds": 4,
                    "baths": 2,
                    "sqft": 1711,
                    "dealType": "Recorded sale",
                    "source": "Santa Clara County Recorder",
                    "isSynthetic": false
                },
                {
                    "address": "302 Synthproxy02 Ct, San Jose, CA 95118",
                    "soldDate": "2025-01-10",
                    "price": 1_820_000,
                    "beds": 4,
                    "baths": 2,
                    "sqft": 1800,
                    "isSynthetic": true
                },
                {
                    "address": "303 Synthproxy03 Dr, San Jose, CA 95118",
                    "soldDate": "2025-01-05",
                    "price": 1_840_000,
                    "beds": 4,
                    "baths": 2,
                    "sqft": 1820,
                    "source": "Parcel + ZHVI benchmark"
                },
                {
                    "address": "304 Synthproxy04 Ave, Campbell, CA 95008",
                    "soldDate": "2025-01-01",
                    "price": 1_860_000,
                    "beds": 4,
                    "baths": 2,
                    "sqft": 1840,
                    "dealType": "Neighbor parcel + ZIP benchmark"
                }
            ]),
        );
    }
    if let Some(val_data) = report
        .pointer_mut("/modules/valuation/data")
        .and_then(Value::as_object_mut)
    {
        val_data.insert("activeNearby".to_string(), json!({ "forSale": [] }));
        val_data.insert(
            "comps".to_string(),
            json!([
                {
                    "address": "305 Synthproxy05 Ave, Campbell, CA 95008",
                    "soldDate": "2024-12-20",
                    "price": 1_750_000,
                    "dealType": "Prop 13 roll proxy (unverified sale)",
                    "source": "Assessor (Prop 13 transfer roll proxy)"
                }
            ]),
        );
    }

    let extracted = extract_listings_from_report(&report);
    let addrs: Vec<&str> = extracted.iter().map(|l| l.address.as_str()).collect();
    assert_eq!(
        extracted.len(),
        2,
        "Expected subject (100 Synthparcelalpha Ln) + 1 verified recorded comp (301 Synthcomp01 Way), got: {addrs:?}"
    );
    assert!(addrs.iter().any(|a| a.contains("100 Synthparcelalpha Ln")));
    assert!(addrs.iter().any(|a| a.contains("301 Synthcomp01 Way")));
    for rejected in [
        "302 Synthproxy02 Ct",
        "303 Synthproxy03 Dr",
        "304 Synthproxy04 Ave",
        "305 Synthproxy05 Ave",
    ] {
        assert!(
            !addrs.iter().any(|a| a.contains(rejected)),
            "Flagged/legacy synthetic comp `{rejected}` leaked into extracted listings!"
        );
    }

    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    let env = EnvConfig::default();
    let upserted = ingest_report_into_db(&db, None, &env, &report, false)
        .await
        .unwrap();
    assert_eq!(upserted.upserted, 2);
    assert_eq!(db.count_listings_total().await.unwrap(), 2);

    let persisted = query_listings(&db, Some("95118"), None, 20).await.unwrap();
    let persisted_addrs: Vec<&str> = persisted["listings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["address"].as_str())
        .collect();
    assert_eq!(persisted_addrs.len(), 2);
    assert!(persisted_addrs
        .iter()
        .any(|a| a.contains("100 Synthparcelalpha Ln")));
    assert!(persisted_addrs
        .iter()
        .any(|a| a.contains("301 Synthcomp01 Way")));
}
