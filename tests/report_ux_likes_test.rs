// [feat/report-ux-likes] End-to-end integration tests on procedural synthetic property fixtures:
// 1. Report refresh limit (24h / 86400s per-home cooldown -> 429 DAILY_REFRESH_LIMIT)
// 2. Internal tech/model details scrubbed from rendered HTML (/r/:slug and /) while preserving report.json
// 3. Cross-platform likes persistence (SQLite), portal signal extraction (no fake counts), & Home Hotness
// 4. Platform-signals security, CSRF/Origin checks, off-portal phishing URL sanitization, & CRON_SECRET auth
// 5. Manual-refresh-only guarantee (`light_scan_listings` reuses cached `listing_factors` with zero network
//    requests, and `seed_reports_from_dir` never overwrites a refreshed report in SQLite)
// 6. Wave 2 Verdict-First Hero, 5 Question Cards, Zone D vs Zone X flags, & Chinese-only `#fengshui`
//    (unconditional HTML checks + real Headless Chromium Playwright verification when resolvable)

mod common;

use open_estate::db::{migrate_database, DbStore, REPORT_REFRESH_COOLDOWN_SECS};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::modules::engagement::{build_engagement, extract_portal_engagement_from_value};
use open_estate::scoring::factors::{HomeFactors, FACTORS_VERSION};
use open_estate::search::light_scan::{
    build_listing_factors_row, light_scan_listings, LightScanOptions,
};
use open_estate::search::parse_listing_csv;
use open_estate::search::sync::{
    ingest_report_into_db, seed_reports_from_dir, upsert_listings_batch, ListingInput,
};
use open_estate::shared::{slugify, EnvConfig, HttpClient, ModuleOutput};
use open_estate::web::orchestrator::{AwaitReportOptions, ReportJobManager};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use open_estate::web::summary::summarize_report;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

const PRIMARY_SLUG: &str = common::SYNTH_PRIMARY_SLUG;
const PRIMARY_ADDRESS: &str = common::SYNTH_PRIMARY_ADDRESS;
const SECONDARY_SLUG: &str = common::SYNTH_SECONDARY_SLUG;
const SECONDARY_ADDRESS: &str = common::SYNTH_SECONDARY_ADDRESS;

fn synth_report(slug: &str) -> Value {
    common::synth_report_for_slug(slug)
}

struct TestServerHandle {
    base_url: String,
    state: AppState,
    db_path: PathBuf,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServerHandle {
    async fn start() -> Self {
        Self::start_with_opts(Vec::new(), true).await
    }

    async fn start_with_env(extra: Vec<(&str, &str)>) -> Self {
        Self::start_with_opts(extra, true).await
    }

    async fn start_with_opts(extra: Vec<(&str, &str)>, offline_http: bool) -> Self {
        let temp_dir = std::env::temp_dir().join(format!("openestate-ux-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");

        let db_path = temp_dir.join("test.sqlite");
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
            map.insert(k.to_string(), v.to_string());
        }
        let env = EnvConfig::from_map(map);

        let http = if offline_http {
            HttpClient::offline_loopback_only(Some(temp_dir.join("http_cache")))
        } else {
            HttpClient::from_env(&env, Some(temp_dir.join("http_cache")))
        };
        let db = DbStore::connect(&env).await.expect("connect test sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let state = AppState::with_components(
            env,
            http,
            db,
            HttpDefenseShield::default(),
            reports_dir,
            None,
        );

        let router = build_router(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

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
            state,
            db_path,
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

#[tokio::test]
async fn test_1_daily_refresh_limit_and_24h_cooldown() {
    let srv = TestServerHandle::start().await;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();

    let slug = PRIMARY_SLUG;
    let address = PRIMARY_ADDRESS;
    let rep = synth_report(slug);

    // Seed completed report & record initial generation timestamp
    srv.state
        .db
        .put_cached_report(slug, &rep, Some(&render_report_html(&rep)))
        .await
        .unwrap();
    srv.state.db.record_report_refresh(slug).await.unwrap();

    // 1. Refresh status endpoint reports cooldown active (`canRefresh: false`)
    let st_resp = client
        .get(format!(
            "{}/api/property/{slug}/refresh-status",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(st_resp.status(), reqwest::StatusCode::OK);
    let st_json: Value = st_resp.json().await.unwrap();
    assert_eq!(st_json["ok"], json!(true));
    assert_eq!(st_json["canRefresh"], json!(false));
    assert!(st_json["nextRefreshAt"].as_str().is_some());
    let cd = st_json["refreshCooldownSeconds"].as_i64().unwrap();
    assert!(
        cd > 86_300 && cd <= REPORT_REFRESH_COOLDOWN_SECS,
        "unexpected cooldown: {cd}"
    );

    // 2. Report metadata includes `nextRefreshAt` and `refreshCooldownSeconds`
    let api_hit: Value = client
        .get(format!(
            "{}/api/report?ai=0&address={}",
            srv.base_url,
            urlencoding::encode(address)
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(api_hit
        .pointer("/report/meta/nextRefreshAt")
        .and_then(Value::as_str)
        .is_some());
    assert!(
        api_hit
            .pointer("/report/meta/refreshCooldownSeconds")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            > 86_300
    );

    // 3. Duplicate refresh via POST /api/report/enqueue within 24h -> 429 DAILY_REFRESH_LIMIT
    let enq_429 = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "address": address, "refresh": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(enq_429.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let ra_hdr: i64 = enq_429
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .expect("429 must include Retry-After header");
    assert!(ra_hdr > 86_300);
    let body_429: Value = enq_429.json().await.unwrap();
    assert_eq!(body_429["ok"], json!(false));
    assert_eq!(body_429["code"], json!("DAILY_REFRESH_LIMIT"));
    assert!(body_429["nextRefreshAt"].as_str().is_some());
    assert!(body_429["retryAfterSeconds"].as_i64().unwrap_or(0) > 86_300);

    // 4. Duplicate refresh via GET /api/report?refresh=1 -> 429 DAILY_REFRESH_LIMIT
    let get_429 = client
        .get(format!(
            "{}/api/report?refresh=1&address={}",
            srv.base_url,
            urlencoding::encode(address)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(get_429.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let get_429_json: Value = get_429.json().await.unwrap();
    assert_eq!(get_429_json["code"], json!("DAILY_REFRESH_LIMIT"));

    // 5. Duplicate refresh via POST /api/report -> 429 DAILY_REFRESH_LIMIT
    let post_429 = client
        .post(format!("{}/api/report", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "address": address, "refresh": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(post_429.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let post_429_json: Value = post_429.json().await.unwrap();
    assert_eq!(post_429_json["code"], json!("DAILY_REFRESH_LIMIT"));

    // 6. Duplicate refresh via GET /report?refresh=1 -> 429 DAILY_REFRESH_LIMIT
    let page_429 = client
        .get(format!(
            "{}/report?refresh=1&address={}",
            srv.base_url,
            urlencoding::encode(address)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(page_429.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let page_429_json: Value = page_429.json().await.unwrap();
    assert_eq!(page_429_json["code"], json!("DAILY_REFRESH_LIMIT"));

    // 7. Verify `ReportJobManager::await_or_build_report` initial build on a synthetic holdout parcel
    //    records `report_refresh_log`.
    let holdout = common::synth_holdout_by_id("synth-ca-95118-mid-sfr");
    let build_addr = holdout["address"].as_str().expect("holdout address");
    let build_slug = slugify(build_addr);
    let pre_geo = ModuleOutput {
        data: holdout["raw"]["geo"].clone(),
        sources: vec![],
        warnings: vec![],
    };
    let built_rep = ReportJobManager::default()
        .await_or_build_report(
            &srv.state.http,
            &srv.state.env,
            &srv.state.db,
            AwaitReportOptions {
                address: build_addr.to_string(),
                slug: Some(build_slug.clone()),
                raw_slug: Some(build_slug.clone()),
                pre_geocoded: Some(pre_geo),
                given_facts: Some(holdout["raw"]["facts"].clone()),
                run_ai: false,
                wait_for_ai: false,
                refresh: false,
                data_dir: None,
            },
        )
        .await
        .expect("offline deterministic report build on synthetic holdout parcel succeeds");
    assert_eq!(built_rep.pointer("/meta/slug"), Some(&json!(build_slug)));

    // Immediately requesting refresh=true on that newly built report must return 429 DAILY_REFRESH_LIMIT
    let built_429 = client
        .post(format!("{}/api/report/enqueue", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "address": build_addr, "refresh": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(built_429.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let built_429_json: Value = built_429.json().await.unwrap();
    assert_eq!(built_429_json["code"], json!("DAILY_REFRESH_LIMIT"));

    // 8. Simulate >24h elapsed (`last_refreshed_unix = now - 90_000`) -> refresh becomes allowed!
    let old_unix = chrono::Utc::now().timestamp() - 90_000;
    srv.state
        .db
        .execute_params(
            "UPDATE report_refresh_log SET last_refreshed_unix = ? WHERE slug = ?",
            &[
                open_estate::db::SqlParam::from(old_unix),
                open_estate::db::SqlParam::from(slug),
            ],
        )
        .await
        .unwrap();
    let st_after = client
        .get(format!(
            "{}/api/property/{slug}/refresh-status",
            srv.base_url
        ))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(st_after["canRefresh"], json!(true));
    assert_eq!(st_after["refreshCooldownSeconds"], json!(0));
}

#[tokio::test]
async fn test_2_tech_details_scrub_from_rendered_html_and_home() {
    let srv = TestServerHandle::start().await;
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();

    let slug = PRIMARY_SLUG;
    let mut rep = synth_report(slug);
    rep["ai"]["model"] = json!("gpt-4.1");
    rep["ai"]["fallbackUsed"] = json!(true);
    rep["ai"]["effort"] = json!("max");
    rep["ai"]["provider"] = json!("openai");
    assert_eq!(
        rep.pointer("/ai/model"),
        Some(&json!("gpt-4.1")),
        "synthetic report carries internal AI model metadata before HTML sanitization"
    );

    srv.state
        .db
        .put_cached_report(slug, &rep, Some(&render_report_html(&rep)))
        .await
        .unwrap();

    // 1. Inspect rendered report page HTML (`/r/:slug/`)
    let r_html = client
        .get(format!("{}/r/{slug}/", srv.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(r_html.status(), reqwest::StatusCode::OK);
    let html_txt = r_html.text().await.unwrap();

    for forbidden in [
        "gpt-4o-mini",
        "gpt-4.1",
        "fallbackUsed",
        "deterministic-fallback",
        "llm:gpt-4o-mini",
        "max reasoning",
        "aiModel",
        "OPENAI_API_KEY",
        "\"effort\"",
        "\"provider\"",
    ] {
        assert!(
            !html_txt.contains(forbidden),
            "Report HTML leaked internal tech string `{forbidden}`"
        );
    }

    // Verify clean product-facing labels & UX controls exist in HTML
    for expected in [
        "✦ AI Insights",
        "AI Inspection Review",
        "AI Title Review",
        "✦ AI Analyst",
        "↻ Refresh Report",
        "Open Estate Likes & Buyer Interest",
    ] {
        assert!(
            html_txt.contains(expected),
            "Report HTML missing expected UI label `{expected}`"
        );
    }

    // 2. Verify direct `render_report_html` strips inspection & titleReview internal keys
    let mut rep_with_docs = rep.clone();
    rep_with_docs["inspection"] = json!({
        "model": "gpt-4.1",
        "effort": "max",
        "fallbackUsed": true,
        "docSummary": { "headline": "Roof wear noted" }
    });
    rep_with_docs["titleReview"] = json!({
        "model": "gpt-4o-mini",
        "effort": "medium",
        "fallbackUsed": false,
        "aiHeadline": "Clear Schedule A"
    });
    let doc_html = render_report_html(&rep_with_docs);
    for forbidden in [
        "gpt-4o-mini",
        "gpt-4.1",
        "fallbackUsed",
        "\"effort\"",
        "\"provider\"",
    ] {
        assert!(
            !doc_html.contains(forbidden),
            "render_report_html with inspection/titleReview leaked `{forbidden}`"
        );
    }

    // 3. Verify Home page (`/`) contains zero tech details
    let home_html = client
        .get(format!("{}/", srv.base_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for forbidden in [
        "gpt-4o-mini",
        "gpt-4.1",
        "fallbackUsed",
        "aiModel",
        "\"effort\"",
        "\"provider\"",
    ] {
        assert!(
            !home_html.contains(forbidden),
            "Home HTML leaked `{forbidden}`"
        );
    }

    // 4. Verify `/r/:slug/report.json` still preserves raw report JSON keys for API compatibility
    let rj: Value = client
        .get(format!("{}/r/{slug}/report.json", srv.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rj.pointer("/ai/model"), Some(&json!("gpt-4.1")));
    assert_eq!(rj.pointer("/ai/fallbackUsed"), Some(&json!(true)));
}

#[tokio::test]
async fn test_3_cross_platform_likes_portal_extraction_hotness_and_migration() {
    let srv = TestServerHandle::start_with_env(vec![("CRON_SECRET", "test-cron-ux-999")]).await;
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();

    let slug = PRIMARY_SLUG;
    let address = PRIMARY_ADDRESS;
    let rep = synth_report(slug);

    srv.state
        .db
        .put_cached_report(slug, &rep, Some(&render_report_html(&rep)))
        .await
        .unwrap();

    // 1. Initial GET likes: 0 in-app likes, external portals report null (no fake counts!)
    let init_likes: Value = client
        .get(format!("{}/api/property/{slug}/likes", srv.base_url))
        .header("X-OpenEstate-Client-Id", "viewer-alpha-1111")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(init_likes["ok"], json!(true));
    assert_eq!(init_likes["likesCount"], json!(0));
    assert_eq!(init_likes["likedByViewer"], json!(false));
    let platforms = init_likes["engagement"]["platforms"].as_array().unwrap();
    assert_eq!(platforms.len(), 5);
    for p in platforms {
        if p["id"] != "in_app" {
            assert_eq!(
                p["likesCount"],
                Value::Null,
                "portal {} must report null when no feed data exists",
                p["id"]
            );
            assert!(
                p["url"].as_str().unwrap_or("").starts_with("https://"),
                "portal {} must provide lookup URL",
                p["id"]
            );
        }
    }

    // 2. Toggle Like for viewer A -> count = 1, likedByViewer = true
    let post_a1: Value = client
        .post(format!("{}/api/property/{slug}/likes", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "viewer-alpha-1111")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(post_a1["likesCount"], json!(1));
    assert_eq!(post_a1["likedByViewer"], json!(true));

    // Idempotent explicit set `liked: true` for viewer A -> stays 1
    let post_a_idem: Value = client
        .post(format!("{}/api/property/{slug}/likes", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "viewer-alpha-1111")
        .json(&json!({ "liked": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(post_a_idem["likesCount"], json!(1));
    assert_eq!(post_a_idem["likedByViewer"], json!(true));

    // Viewer B toggles Like -> count = 2
    let post_b: Value = client
        .post(format!("{}/api/property/{slug}/likes", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "viewer-beta-2222")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(post_b["likesCount"], json!(2));
    assert_eq!(post_b["likedByViewer"], json!(true));

    // Verify SQLite persistence directly by opening a fresh DbStore handle on the same SQLite file
    let fresh_db = DbStore::connect_url(&srv.db_path.to_string_lossy(), Some("sqlite"))
        .await
        .unwrap();
    assert_eq!(
        fresh_db
            .get_property_likes_state(slug, Some("viewer-alpha-1111"))
            .await
            .unwrap(),
        (2, true)
    );

    // Viewer A toggles again -> count drops to 1, likedByViewer = false
    let post_a_unlike: Value = client
        .post(format!("{}/api/property/{slug}/likes", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "viewer-alpha-1111")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(post_a_unlike["likesCount"], json!(1));
    assert_eq!(post_a_unlike["likedByViewer"], json!(false));

    // 3. Ingest portal engagement via `/api/listings/ingest` fails closed without Bearer, succeeds with Bearer
    let ingest_unauth = client
        .post(format!("{}/api/listings/ingest", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "zip": "95118", "listings": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(ingest_unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let ingest_resp = client
        .post(format!("{}/api/listings/ingest", srv.base_url))
        .header("Authorization", "Bearer test-cron-ux-999")
        .json(&json!({
            "zip": "95118",
            "triggerAlerts": false,
            "listings": [{
                "address": address,
                "city": "San Jose",
                "state": "CA",
                "zip": "95118",
                "status": "for_sale",
                "price": 2_150_000,
                "beds": 4,
                "baths": 2,
                "sqft": 2040,
                "daysOnMarket": 4,
                "source": "Redfin",
                "reportSlug": slug,
                "favoriteCount": 84,
                "pageViewCount": 1450,
                "hotHome": true
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ingest_resp.status(), reqwest::StatusCode::OK);

    // Fetch live report.json and assert ingested portal engagement + unfabricated null for other portals
    let rj: Value = client
        .get(format!("{}/r/{slug}/report.json", srv.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let eng = &rj["engagement"];
    assert_eq!(eng["inAppLikes"], json!(1));
    assert_eq!(eng["totalKnownLikes"], json!(85));
    assert_eq!(eng["hotnessLabel"], json!("🔥 Hot Home"));
    assert!(eng["hotnessScore"].as_i64().unwrap() >= 75);
    let reason = eng["hotnessReason"].as_str().unwrap();
    assert!(reason.contains("85 tracked save/like(s)"));
    assert!(reason.contains("Hot Home"));

    let plats = eng["platforms"].as_array().unwrap();
    let redfin = plats.iter().find(|p| p["id"] == "redfin").unwrap();
    assert_eq!(redfin["likesCount"], json!(84));
    assert_eq!(redfin["viewsCount"], json!(1450));
    assert_eq!(redfin["hotTag"], json!("Hot Home"));
    assert!(redfin["url"].as_str().unwrap_or("").starts_with("https://"));
    let zillow = plats.iter().find(|p| p["id"] == "zillow").unwrap();
    assert_eq!(zillow["likesCount"], Value::Null);
    assert_eq!(zillow["viewsCount"], Value::Null);

    // 4. Optional POST /api/property/:slug/platform-signals adds Zillow stats cleanly
    let sig_resp: Value = client
        .post(format!(
            "{}/api/property/{slug}/platform-signals",
            srv.base_url
        ))
        .header("Authorization", "Bearer test-cron-ux-999")
        .json(&json!({
            "zillow": { "saves": 32, "views": 620 }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sig_resp["ok"], json!(true));
    assert_eq!(sig_resp["engagement"]["totalKnownLikes"], json!(117));
    let sig_plats = sig_resp["engagement"]["platforms"].as_array().unwrap();
    assert_eq!(
        sig_plats.iter().find(|p| p["id"] == "zillow").unwrap()["likesCount"],
        json!(32)
    );
    assert_eq!(
        sig_plats.iter().find(|p| p["id"] == "realtor").unwrap()["likesCount"],
        Value::Null
    );

    // 5. CSV parser preserves optional Favorites / Views / Hot Home columns on synthetic listing row
    let csv_text = "SALE TYPE,PROPERTY TYPE,ADDRESS,CITY,STATE OR PROVINCE,ZIP OR POSTAL CODE,PRICE,BEDS,BATHS,SQUARE FEET,FAVORITES,VIEWS,HOT HOME,URL\n\
                    For Sale,Single Family Residential,301 Synthlisting01 Dr,San Jose,CA,95118,2049888,4,2.0,1711,55,980,true,/report?address=301+Synthlisting01+Dr\n";
    let parsed_csv = parse_listing_csv(csv_text, "95118");
    assert_eq!(parsed_csv.len(), 1);
    let csv_raw: Value = serde_json::from_str(parsed_csv[0].raw_json.as_deref().unwrap()).unwrap();
    assert_eq!(csv_raw["favoriteCount"], json!(55));
    assert_eq!(csv_raw["pageViewCount"], json!(980));
    assert_eq!(csv_raw["hotHome"], json!(true));

    // 6. Hotness label progression unit checks using synthetic Campbell & San Jose parcels
    let quiet = build_engagement(
        SECONDARY_SLUG,
        SECONDARY_ADDRESS,
        &json!({}),
        None,
        &[],
        None,
        0,
    );
    assert_eq!(quiet["hotnessLabel"], json!("Quiet"));
    assert_eq!(quiet["totalKnownLikes"], json!(0));

    let steady = build_engagement(
        SECONDARY_SLUG,
        SECONDARY_ADDRESS,
        &json!({ "daysOnMarket": 20 }),
        None,
        &[],
        None,
        5,
    );
    assert_eq!(steady["hotnessLabel"], json!("Steady"));

    let warm = build_engagement(
        PRIMARY_SLUG,
        PRIMARY_ADDRESS,
        &json!({ "daysOnMarket": 10 }),
        Some(&rep["modules"]["market"]["data"]),
        &[],
        None,
        20,
    );
    assert_eq!(warm["hotnessLabel"], json!("Warm Interest"));

    let mut extracted = HashMap::new();
    extract_portal_engagement_from_value(
        &json!({ "realtor": { "favorites": 14, "views": 310 } }),
        None,
        &mut extracted,
    );
    assert_eq!(extracted.get("realtor").unwrap().likes_count, Some(14));

    // 7. Verify `migrate_database` streams engagement & refresh tables idempotently
    let dst_path = srv.temp_dir.join("mig-dst.sqlite");
    let dst_db = DbStore::connect_url(&dst_path.to_string_lossy(), Some("sqlite"))
        .await
        .unwrap();
    let mig_1 = migrate_database(&srv.state.db, &dst_db).await.unwrap();
    assert!(mig_1.tables.contains_key("report_refresh_log"));
    assert_eq!(mig_1.tables.get("property_likes").copied(), Some(1));
    assert_eq!(
        mig_1.tables.get("property_platform_signals").copied(),
        Some(1)
    );
    let mig_2 = migrate_database(&srv.state.db, &dst_db).await.unwrap();
    assert_eq!(mig_2.tables, mig_1.tables);
    assert_eq!(
        dst_db
            .get_property_likes_state(slug, Some("viewer-beta-2222"))
            .await
            .unwrap(),
        (1, true)
    );
}

#[tokio::test]
async fn test_4_platform_signals_auth_off_portal_url_and_likes_security() {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();

    // Part A: platform-signals fails closed 401 when CRON_SECRET unset or unauthenticated; authenticated call drops off-portal
    // phishing URLs, and likes enforce strict Referer + no body.clientId vote inflation.
    let srv_open = TestServerHandle::start_with_env(vec![("CRON_SECRET", "open-cron-test-888")]).await;
    let slug = PRIMARY_SLUG;
    let mut rep = synth_report(slug);
    rep["facts"]["mlsListing"] = json!({
        "provider": "Zillow Feed",
        "url": "https://portal.attacker.example/steal",
        "favoriteCount": 19
    });
    srv_open
        .state
        .db
        .put_cached_report(slug, &rep, None)
        .await
        .unwrap();

    // Missing auth on platform-signals -> 401 even with Origin
    let bare_sig = client
        .post(format!(
            "{}/api/property/{slug}/platform-signals",
            srv_open.base_url
        ))
        .header("Origin", &srv_open.base_url)
        .json(&json!({ "zillow": { "saves": 55 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(bare_sig.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Authenticated platform-signals with phishing/http/credentialed URLs -> off-portal URLs dropped
    let phish_sig = client
        .post(format!(
            "{}/api/property/{slug}/platform-signals",
            srv_open.base_url
        ))
        .header("Authorization", "Bearer open-cron-test-888")
        .json(&json!({
            "zillow": { "saves": 40, "url": "https://phishing.evil.example/portal-login" },
            "redfin": { "saves": 12, "url": "http://insecure.portal.example/home/1" },
            "realtor": { "saves": 8, "url": "https://user:pass@auth.portal.example/home/2" },
            "homes": { "saves": 5 }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(phish_sig.status(), reqwest::StatusCode::OK);
    let sig_json: Value = phish_sig.json().await.unwrap();
    let plats = sig_json["engagement"]["platforms"].as_array().unwrap();
    for pid in ["zillow", "redfin", "realtor", "homes"] {
        let p_url = plats.iter().find(|p| p["id"] == pid).unwrap()["url"]
            .as_str()
            .unwrap();
        assert!(
            p_url.starts_with("https://"),
            "expected canonical https fallback URL for {pid}, got {p_url}"
        );
        assert!(!p_url.contains("evil.example"));
        assert!(!p_url.contains("attacker.example"));
        assert!(!p_url.contains("portal.example"));
        assert!(!p_url.contains("user:pass"));
    }

    // Likes security: missing Content-Type -> 415 Unsupported Media Type; cross-origin Referer -> 403
    let missing_ct = client
        .post(format!("{}/api/property/{slug}/likes", srv_open.base_url))
        .header("Referer", "https://attacker.evil.example/csrf")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        missing_ct.status(),
        reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );

    let bad_ref = client
        .post(format!("{}/api/property/{slug}/likes", srv_open.base_url))
        .header("Referer", "https://attacker.evil.example/csrf")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_ref.status(), reqwest::StatusCode::FORBIDDEN);

    // Likes security: body.clientId vote-inflation attempt rejected (400 unexpected key)
    let bot_body = client
        .post(format!("{}/api/property/{slug}/likes", srv_open.base_url))
        .header("Origin", &srv_open.base_url)
        .json(&json!({ "clientId": "bot-0001" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bot_body.status(), reqwest::StatusCode::BAD_REQUEST);

    // Without X-OpenEstate-Client-Id, calls from the same IP map to the same visitor:{ip} entry
    let ip_like_1: Value = client
        .post(format!("{}/api/property/{slug}/likes", srv_open.base_url))
        .header("Origin", &srv_open.base_url)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ip_like_1["likesCount"], json!(1));
    assert_eq!(ip_like_1["likedByViewer"], json!(true));
    let ip_like_2: Value = client
        .post(format!("{}/api/property/{slug}/likes", srv_open.base_url))
        .header("Origin", &srv_open.base_url)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ip_like_2["likesCount"], json!(0));
    assert_eq!(ip_like_2["likedByViewer"], json!(false));

    // Part B: CRON_SECRET configured -> same-origin without Bearer fails 401; valid Bearer succeeds
    let srv_cron = TestServerHandle::start_with_env(vec![("CRON_SECRET", "cron-secret-999")]).await;
    let unauth_same_origin = client
        .post(format!(
            "{}/api/property/{slug}/platform-signals",
            srv_cron.base_url
        ))
        .header("Origin", &srv_cron.base_url)
        .json(&json!({ "zillow": { "saves": 99 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauth_same_origin.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let err_json: Value = unauth_same_origin.json().await.unwrap();
    assert_eq!(err_json["code"], json!("UNAUTHORIZED"));

    let bad_bearer = client
        .post(format!(
            "{}/api/property/{slug}/platform-signals",
            srv_cron.base_url
        ))
        .header("Authorization", "Bearer wrong-token")
        .json(&json!({ "zillow": { "saves": 99 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_bearer.status(), reqwest::StatusCode::UNAUTHORIZED);

    let ok_bearer = client
        .post(format!(
            "{}/api/property/{slug}/platform-signals",
            srv_cron.base_url
        ))
        .header("Authorization", "Bearer cron-secret-999")
        .json(&json!({ "zillow": { "saves": 99 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_bearer.status(), reqwest::StatusCode::OK);
    let ok_json: Value = ok_bearer.json().await.unwrap();
    assert_eq!(ok_json["engagement"]["totalKnownLikes"], json!(99));
}

#[tokio::test]
async fn test_5_manual_refresh_only_no_auto_refresh_of_existing_houses() {
    let srv = TestServerHandle::start().await;
    let secondary_rep = synth_report(SECONDARY_SLUG);

    // 1. Seed synthetic secondary parcel in `listings` + `listing_factors` with
    //    `scanned_at` years in the past ("2020-01-01T00:00:00Z").
    let secondary_listing = ListingInput {
        id: Some(SECONDARY_SLUG.to_string()),
        address: SECONDARY_ADDRESS.to_string(),
        street: Some("200 Synthparcelbeta Ave".to_string()),
        city: Some("Campbell".to_string()),
        state: Some("CA".to_string()),
        zip: "95008".to_string(),
        lat: Some(37.2872),
        lon: Some(-121.9500),
        status: Some("sold".to_string()),
        price: Some(2_350_000),
        beds: Some(4.0),
        baths: Some(3.0),
        sqft: Some(2_180),
        lot_sqft: Some(7_400),
        year_built: Some(1962),
        property_type: Some("Single Family".to_string()),
        ..Default::default()
    };
    upsert_listings_batch(
        &srv.state.db,
        None,
        &srv.state.env,
        &[secondary_listing],
        false,
    )
    .await
    .unwrap();
    let rows = srv
        .state
        .db
        .query_listings_in_areas(&["95008".to_string()], &[], None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let mut hf = HomeFactors::from_report(&secondary_rep);
    hf.id = rows[0].id.clone();
    hf.version = FACTORS_VERSION;
    let mut lf_row = build_listing_factors_row(
        &rows[0],
        &hf,
        "light_scan",
        rows[0].lat,
        rows[0].lon,
        Some("Low"),
        Some(71.0),
        Some(8.7),
    );
    lf_row.scanned_at = "2020-01-01T00:00:00Z".to_string();
    lf_row.updated_at = "2020-01-01T00:00:00Z".to_string();
    srv.state
        .db
        .upsert_listing_factors_batch(&[lf_row])
        .await
        .unwrap();

    // `srv.state.http` is `HttpClient::offline_loopback_only`, which fails closed on any external
    // request and tracks slot request counters in `egress_status_json()`.
    let ls_no_auto = light_scan_listings(
        &srv.state.db,
        &srv.state.http,
        &srv.state.env,
        &rows,
        &LightScanOptions {
            network_enabled: true,
            force_refresh: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ls_no_auto.reused_cached, 1);
    assert_eq!(ls_no_auto.scanned_light, 0);
    let slot_requests: u64 = srv.state.http.egress_status_json()["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["requests"].as_u64())
        .sum();
    assert_eq!(
        slot_requests, 0,
        "existing house factors must never auto-refresh without manual refresh"
    );

    // 2. Verify `seed_reports_from_dir` never overwrites an already-refreshed report in `cached_reports`.
    let mut refreshed_rep = synth_report(PRIMARY_SLUG);
    refreshed_rep["meta"]["generatedAt"] = json!("2026-11-14T02:00:00Z");
    refreshed_rep["modules"]["valuation"]["data"]["estimate"]["value"] = json!(2_450_000);
    srv.state
        .db
        .put_cached_report(PRIMARY_SLUG, &refreshed_rep, Some("<html>Refreshed</html>"))
        .await
        .unwrap();
    ingest_report_into_db(&srv.state.db, None, &srv.state.env, &refreshed_rep, false)
        .await
        .unwrap();

    let tmp_seed_dir =
        std::env::temp_dir().join(format!("openestate-no-overwrite-{}", Uuid::new_v4()));
    let slug_dir = tmp_seed_dir.join(PRIMARY_SLUG);
    std::fs::create_dir_all(&slug_dir).unwrap();
    let older_disk_rep = synth_report(PRIMARY_SLUG);
    std::fs::write(
        slug_dir.join("report.json"),
        serde_json::to_string(&older_disk_rep).unwrap(),
    )
    .unwrap();
    let seeded_cnt = seed_reports_from_dir(&srv.state.db, &tmp_seed_dir)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp_seed_dir);
    assert_eq!(
        seeded_cnt, 0,
        "existing refreshed DB report must not be overwritten from disk"
    );

    let stored = srv
        .state
        .db
        .get_cached_report(PRIMARY_SLUG)
        .await
        .unwrap()
        .expect("cached report exists");
    assert_eq!(
        stored
            .report
            .pointer("/modules/valuation/data/estimate/value"),
        Some(&json!(2_450_000))
    );
}

const WAVE2_PLAYWRIGHT_CHECK_MJS: &str = r#"
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
let playwrightPkg;
try {
  playwrightPkg = require('playwright');
} catch {
  if (process.env.PLAYWRIGHT_MODULE_PATH) {
    playwrightPkg = require(process.env.PLAYWRIGHT_MODULE_PATH);
  } else {
    console.log('SKIP playwright_not_resolvable');
    process.exit(0);
  }
}
const { chromium } = playwrightPkg;

function findChromiumExecutable() {
  if (
    process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH &&
    fs.existsSync(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH)
  ) {
    return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;
  }
  const cacheRoot = path.join(process.env.HOME || '/root', '.cache', 'ms-playwright');
  if (fs.existsSync(cacheRoot)) {
    const dirs = fs
      .readdirSync(cacheRoot)
      .filter((d) => d.startsWith('chromium-'))
      .sort()
      .reverse();
    for (const d of dirs) {
      for (const sub of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
        const candidate = path.join(cacheRoot, d, sub);
        if (fs.existsSync(candidate)) return candidate;
      }
    }
  }
  return undefined;
}

const [, , htmlPath] = process.argv;
const html = fs.readFileSync(htmlPath, 'utf8');

let browser;
try {
  browser = await chromium.launch({
    headless: true,
    executablePath: findChromiumExecutable(),
    args: ['--no-sandbox', '--disable-dev-shm-usage'],
  });
} catch (err) {
  console.log('SKIP chromium_launch_unavailable: ' + String(err?.message || err));
  process.exit(0);
}

try {
  const context = await browser.newContext({
    colorScheme: 'light',
    viewport: { width: 1440, height: 900 },
  });
  const page = await context.newPage();
  await page.route('https://**', (r) => r.abort());
  await page.setContent(html, { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(100);

  const out = await page.evaluate(async () => {
    const heroRect = document.getElementById('verdictHeroGrid')?.getBoundingClientRect();
    const cardsRect = document.getElementById('questionCardsStack')?.getBoundingClientRect();
    const cardIds = ['qCardCost', 'qCardRisk', 'qCardPrice', 'qCardLiving', 'qCardPotential'];
    const cards = cardIds.map((id) => {
      const root = document.getElementById(id);
      return {
        id,
        answer: (root?.querySelector('.q-card-answer')?.textContent || '').trim(),
        verdict: (root?.querySelector('.q-card-verdict')?.textContent || '').trim(),
        text: (root?.textContent || '').trim(),
      };
    });

    const toggle = document.querySelector('#qCardCost .q-card-toggle');
    if (toggle) toggle.click();
    const costExpanded = toggle?.getAttribute('aria-expanded') === 'true';

    const fsByLang = {};
    for (const lang of ['en', 'es', 'zh']) {
      if (window.OpenEstateCore && typeof window.OpenEstateCore.setLang === 'function') {
        window.OpenEstateCore.setLang(lang);
      }
      fsByLang[lang] = (document.getElementById('fengshui')?.textContent || '').trim();
    }

    return {
      verdictFirst: !!(heroRect && cardsRect && heroRect.top < cardsRect.top),
      cards,
      costExpanded,
      fsByLang,
    };
  });

  console.log('WAVE2 ' + JSON.stringify(out));
  await context.close();
} finally {
  await browser.close();
}
"#;

#[test]
fn test_6_wave2_verdict_first_hero_question_cards_zone_d_and_no_ops_link() {
    let secondary = synth_report(SECONDARY_SLUG);
    let mut primary = synth_report(PRIMARY_SLUG);
    primary["modules"]["hazards"]["data"]["flood"] = json!({
        "zone": "D",
        "sfha": false,
        "zoneSubtype": "AREA WITH UNDETERMINED FLOOD HAZARD",
        "nearestSfhaDistM": 620.0,
        "insuranceRequired": false
    });
    primary["summary"] = summarize_report(&primary);
    normalize_stored_report(&mut primary);

    for (name, rep) in [("secondary", &secondary), ("primary", &primary)] {
        let html = render_report_html(rep);
        assert!(
            !html.contains("href=\"/ops\"") && !html.contains("Platform Ops"),
            "[{name}] Report HTML must not contain public /ops link"
        );
        assert!(
            html.contains("id=\"verdictHeroGrid\"")
                && html.contains("id=\"verdictMediaCol\"")
                && html.contains("id=\"questionCardsStack\"")
                && html.contains("id=\"qCardCost\"")
                && html.contains("id=\"qCardRisk\"")
                && html.contains("id=\"qCardPrice\"")
                && html.contains("id=\"qCardLiving\"")
                && html.contains("id=\"qCardPotential\""),
            "[{name}] Report HTML missing Wave 2 verdict hero or 5 question card containers"
        );
        assert!(
            !open_estate::shared::has_retired_brand(&html),
            "[{name}] Report HTML leaked retired brand"
        );
        assert!(
            !include_str!("../assets/template.html").contains("location.search")
                && !include_str!("../assets/report_workspace.js").contains("location.search"),
            "[{name}] template.html and report_workspace.js must not use location.search"
        );
    }

    // Real Headless Chromium Playwright check when `playwright` is resolvable.
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return;
    }

    for (label, rep, expect_zone_d) in
        [("secondary", &secondary, false), ("primary", &primary, true)]
    {
        let dir = std::env::temp_dir().join(format!("openestate-wave2-pw-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let runner_path = dir.join("wave2.mjs");
        let html_path = dir.join("report.html");
        std::fs::write(&runner_path, WAVE2_PLAYWRIGHT_CHECK_MJS).unwrap();
        std::fs::write(&html_path, render_report_html(rep)).unwrap();

        let out = std::process::Command::new("node")
            .arg(&runner_path)
            .arg(&html_path)
            .output()
            .expect("run wave2 playwright check");
        let _ = std::fs::remove_dir_all(&dir);

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "[{label}] Wave 2 Chromium check failed: {stdout}\n{stderr}"
        );
        if stdout.lines().any(|l| l.starts_with("SKIP ")) {
            eprintln!("[{label}] skipping Chromium Wave 2 DOM check (playwright not resolvable)");
            return;
        }
        let line = stdout
            .lines()
            .find(|l| l.starts_with("WAVE2 "))
            .unwrap_or_else(|| panic!("[{label}] missing WAVE2 output: {stdout}"));
        let res: Value = serde_json::from_str(&line["WAVE2 ".len()..]).unwrap();

        assert_eq!(
            res["verdictFirst"],
            json!(true),
            "[{label}] verdict hero precedes question cards"
        );
        assert_eq!(
            res["costExpanded"],
            json!(true),
            "[{label}] qCardCost drawer expands on click"
        );
        let cards = res["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 5, "[{label}] all 5 question cards rendered");
        for c in cards {
            assert!(
                !c["answer"].as_str().unwrap_or("").is_empty(),
                "[{label}] empty answer on {}",
                c["id"]
            );
        }
        let risk_text = cards[1]["text"].as_str().unwrap_or("");
        if expect_zone_d {
            assert!(
                risk_text.contains("Zone D") && risk_text.contains('⚠'),
                "[{label}] Zone D card must show undetermined warning flag: {risk_text}"
            );
        } else {
            assert!(
                !risk_text.contains("Zone D"),
                "[{label}] Zone X card must not claim Zone D: {risk_text}"
            );
        }
        for lang in ["en", "es", "zh"] {
            let fs_txt = res["fsByLang"][lang].as_str().unwrap_or("");
            assert!(
                fs_txt.contains("堪舆") || fs_txt.contains("坐") || fs_txt.contains("飞星"),
                "[{label}] #fengshui must stay Simplified Chinese in {lang}: {fs_txt}"
            );
        }
    }
}
