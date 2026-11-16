// End-to-End Regression Suite (`tests/e2e_browser_test.rs` + `e2e/playwright_e2e.mjs`):
// 1. Unit/integration verification of `check_complete_us_street_address`, `validate_street_address_input`,
//    `validate_address_input`, and geocoder hardening across valid & incomplete US address patterns.
// 2. Full real-browser Headless Chromium Playwright E2E run against an in-process Axum Open Estate server
//    seeded with synthetic 19-section property reports, native deterministic i18n translations,
//    grounded Q&A, heuristic/live home matching, and `/api/suggest`.

mod common;

use axum::http::StatusCode;
use open_estate::db::DbStore;
use open_estate::guardrails::{
    check_complete_us_street_address, validate_address_input, validate_street_address_input,
    HttpDefenseShield,
};
use open_estate::search::ingest_report_into_db;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

#[test]
fn test_1_complete_us_street_address_guardrails_unit() {
    let valid_addresses = [
        common::SYNTH_PRIMARY_ADDRESS,
        common::SYNTH_SECONDARY_ADDRESS,
        "1503 Synthcasa Ln, San Jose, CA 95118",
        "20-22 Syntharch St #501, Philadelphia, PA 19106-2118",
        "35 Synthbay St #5, Cambridge, MA 02139",
        "123 1/2 Synthharbor St, Boston, MA 02116",
        "W156N8480 Synthgrid Rd, Menomonee Falls, WI 53051",
        "20500 Synthblvd Blvd, Cupertino, CA 95014",
        "2221 Synthlakeshore Dr, Evanston, Illinois",
        "1600 Synthcapitol Ave NW, Washington, DC 20500, USA",
        "500 Synthisland Blvd, Honolulu, HI",
        "100 Calle Synthborinquen, San Juan, PR 00901",
        "450 Synthmarket St, 94108",
    ];
    for addr in valid_addresses {
        assert!(
            check_complete_us_street_address(addr).is_ok(),
            "expected valid complete US street address `{addr}` to pass"
        );
        assert!(
            validate_street_address_input(addr).is_ok(),
            "expected validate_street_address_input(`{addr}`) to pass"
        );
        assert!(
            validate_address_input(addr).is_ok(),
            "expected validate_address_input(`{addr}`) to pass"
        );
    }

    // Explicit negative validation inputs (incomplete / non-street / PO Box) that MUST return HTTP 400
    let incomplete_or_invalid = [
        "",
        "   ",
        "San Jose",
        "San Jose, CA",
        "95118",
        "95121, San Jose, California",
        "San Jose, CA 95121",
        "300 Synthparcel",
        "100 Synthparcelalpha Ln",
        "123 Synthalpha St",
        "123 Synthalpha Ct", // street suffix "Ct" must NOT pass as Connecticut without comma+city
        "456 Synthbeta Wy",  // street suffix "Wy" must NOT pass as Wyoming without comma+city
        "789 Synthgamma La", // street suffix "La" must NOT pass as Louisiana without comma+city
        "20500 Synthblvd Blvd", // 5-digit house number must NOT pass as 5-digit ZIP
        "Synthparcelalpha Ln, San Jose, CA 95118", // missing leading street number
        "Synthalpha St, Austin, TX",
        "PO Box 123, San Jose, CA 95118",
        "P.O. Box 4500, Austin, TX 78765",
        "Post Office Box 99, Miami, FL 33101",
    ];
    for bad in incomplete_or_invalid {
        assert!(
            check_complete_us_street_address(bad).is_err()
                || validate_street_address_input(bad).is_err(),
            "expected incomplete/invalid address `{bad}` to be rejected"
        );
        let err = validate_street_address_input(bad).expect_err(&format!(
            "validate_street_address_input must reject `{bad}`"
        ));
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "INVALID_ADDRESS");

        let err2 = validate_address_input(bad)
            .expect_err(&format!("validate_address_input must reject `{bad}`"));
        assert_eq!(err2.status, 400);
        assert_eq!(err2.code, "INVALID_ADDRESS");
    }
}

fn playwright_chromium_available() -> bool {
    Command::new("node")
        .arg("-e")
        .arg("import('playwright').then(async p => { const b = await p.chromium.launch({headless:true}); await b.close(); process.exit(0); }).catch(() => process.exit(1));")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_2_full_playwright_chromium_e2e_suite() {
    let temp_dir = std::env::temp_dir().join(format!("openestate-e2e-browser-{}", Uuid::new_v4()));
    let reports_dir = temp_dir.join("reports");
    let http_cache_dir = temp_dir.join("http_cache");
    std::fs::create_dir_all(&reports_dir).unwrap();
    std::fs::create_dir_all(&http_cache_dir).unwrap();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut rep_primary: Value = common::synth_report_primary();
    let mut rep_secondary: Value = common::synth_report_secondary();

    // Populate all deterministic derived modules & native translations (`native: true`) via production normalizer
    normalize_stored_report(&mut rep_primary);
    normalize_stored_report(&mut rep_secondary);

    let db_path = temp_dir.join("e2e.sqlite");
    let env = EnvConfig::from_process_and_dotenv(Some(Path::new("."))).with_overrides([
        ("SQLITE_DB_PATH", db_path.to_string_lossy().into_owned()),
        ("SQLITE_PATH", db_path.to_string_lossy().into_owned()),
        ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
        ("SYNC_INTERVAL_MIN", "0".to_string()),
    ]);

    let http = HttpClient::from_env(&env, Some(http_cache_dir));
    let db = DbStore::connect(&env).await.expect("connect e2e sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let primary_slug = common::SYNTH_PRIMARY_SLUG;
    let secondary_slug = common::SYNTH_SECONDARY_SLUG;

    db.put_cached_report(
        primary_slug,
        &rep_primary,
        Some(&render_report_html(&rep_primary)),
    )
    .await
    .unwrap();
    db.record_report_refresh(primary_slug).await.unwrap();

    db.put_cached_report(
        secondary_slug,
        &rep_secondary,
        Some(&render_report_html(&rep_secondary)),
    )
    .await
    .unwrap();

    // Verify startup incomplete-report purge removes pre-guardrail ZIP/city-only slugs while preserving valid street reports
    db.put_cached_report(
        "95121-san-jose-california",
        &json!({ "meta": { "slug": "95121-san-jose-california", "displayAddress": "95121, San Jose, California" } }),
        Some("<html>incomplete</html>"),
    )
    .await
    .unwrap();
    db.put_cached_report(
        "synthsector-bayamon",
        &json!({ "meta": { "slug": "synthsector-bayamon", "displayAddress": "Synthsector, Bayamón" } }),
        Some("<html>incomplete</html>"),
    )
    .await
    .unwrap();
    let purged = db.purge_incomplete_cached_reports().await.unwrap();
    assert_eq!(purged, 2, "Expected 2 incomplete cached reports purged");
    assert!(db
        .get_cached_report("95121-san-jose-california")
        .await
        .unwrap()
        .is_none());
    assert!(db
        .get_cached_report("synthsector-bayamon")
        .await
        .unwrap()
        .is_none());
    assert!(db.get_cached_report(primary_slug).await.unwrap().is_some());
    assert!(db
        .get_cached_report(secondary_slug)
        .await
        .unwrap()
        .is_some());

    // Ingest synthetic reports & their comparable sales into SQLite for `/api/match`
    ingest_report_into_db(&db, None, &env, &rep_primary, false)
        .await
        .unwrap();
    ingest_report_into_db(&db, None, &env, &rep_secondary, false)
        .await
        .unwrap();

    let state = AppState::with_components(
        env,
        http,
        db,
        HttpDefenseShield::default(),
        reports_dir,
        Some(temp_dir.join("datasets")),
    );

    let router = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind e2e server");
    let srv_addr = listener.local_addr().unwrap();
    let base_url = format!("http://{srv_addr}");
    let host_hdr = srv_addr.to_string();
    let (srv_stop_tx, srv_stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = srv_stop_rx.await;
        })
        .await;
    });

    // Direct Rust HTTP assertions against the live Axum server
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .build()
        .expect("build http client");

    let home_res = client
        .get(format!("{base_url}/"))
        .send()
        .await
        .expect("GET /");
    assert_eq!(home_res.status(), StatusCode::OK);

    let rep_res = client
        .get(format!("{base_url}/r/{primary_slug}/"))
        .send()
        .await
        .expect("GET primary report HTML");
    assert_eq!(rep_res.status(), StatusCode::OK);
    let rep_html = rep_res.text().await.unwrap();
    assert!(rep_html.contains("100 Synthparcelalpha Ln"));

    let tr_res = client
        .post(format!("{base_url}/api/translate"))
        .header("Host", &host_hdr)
        .header("Origin", &base_url)
        .json(&json!({ "slug": primary_slug, "lang": "zh" }))
        .send()
        .await
        .expect("POST /api/translate");
    assert_eq!(tr_res.status(), StatusCode::OK);
    let tr_json: Value = tr_res.json().await.unwrap();
    assert_eq!(tr_json["ok"], json!(true));
    assert_eq!(tr_json["lang"], json!("zh"));

    let ask_res = client
        .post(format!("{base_url}/api/ask"))
        .header("Host", &host_hdr)
        .header("Origin", &base_url)
        .json(&json!({
            "slug": primary_slug,
            "question": "What is the estimated monthly ownership cost and HOA fee?"
        }))
        .send()
        .await
        .expect("POST /api/ask");
    assert_eq!(ask_res.status(), StatusCode::OK);
    let ask_json: Value = ask_res.json().await.unwrap();
    assert_eq!(ask_json["ok"], json!(true));
    assert!(
        ask_json["answer"]
            .as_str()
            .map(|s| s.len() >= 15)
            .unwrap_or(false),
        "expected grounded answer from /api/ask, got {ask_json:?}"
    );

    let match_res = client
        .post(format!("{base_url}/api/match"))
        .header("Host", &host_hdr)
        .header("Origin", &base_url)
        .json(&json!({
            "query": "3+ bed single family home in 95118 under $2.6M",
            "autoSync": false
        }))
        .send()
        .await
        .expect("POST /api/match");
    assert_eq!(match_res.status(), StatusCode::OK);
    let match_json: Value = match_res.json().await.unwrap();
    assert_eq!(match_json["ok"], json!(true));

    let suggest_res = client
        .get(format!("{base_url}/api/suggest?q=30"))
        .send()
        .await
        .expect("GET /api/suggest");
    assert_eq!(suggest_res.status(), StatusCode::OK);

    let chromium_ok = tokio::task::spawn_blocking(playwright_chromium_available)
        .await
        .unwrap_or(false);
    if !chromium_ok {
        let _ = srv_stop_tx.send(());
        let _ = std::fs::remove_dir_all(&temp_dir);
        eprintln!(
            "skipping Headless Chromium Playwright run in test_2_full_playwright_chromium_e2e_suite: Playwright Chromium binary unavailable"
        );
        return;
    }

    let script_path = manifest_dir.join("e2e/playwright_e2e.mjs");
    let base_url_clone = base_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("node")
            .arg(&script_path)
            .env("BASE_URL", &base_url_clone)
            .env("PRIMARY_SLUG", primary_slug)
            .env("SECONDARY_SLUG", secondary_slug)
            .output()
            .expect("spawn `node e2e/playwright_e2e.mjs`")
    })
    .await
    .expect("join playwright task");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }

    let _ = srv_stop_tx.send(());
    let _ = std::fs::remove_dir_all(&temp_dir);

    assert!(
        output.status.success(),
        "Playwright E2E browser suite failed with exit status {:?}\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("ALL OPEN_ESTATE PLAYWRIGHT E2E CHECKS PASSED"),
        "Expected E2E completion banner in stdout, got:\n{stdout}"
    );
}
