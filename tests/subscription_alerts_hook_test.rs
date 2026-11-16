//! End-to-end integration test for the `LiveZipListingProvider` subscription & alert
//! pipeline using SQLite WAL, a local TCP SMTP server (`lettre`), and synthetic property
//! records derived from `tests/common/mod.rs`.

#![recursion_limit = "512"]

mod common;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use open_estate::db::DbStore;
use open_estate::search::alerts::{
    build_match_alert_email_with_brand, create_and_send_manage_link,
    create_subscription_with_profile_and_provider, delete_subscription_authorized,
    evaluate_alerts_for_listings, flush_due_subscription_digests, list_subscriptions,
    list_subscriptions_by_manage_token, normalize_frequency,
    preview_subscription_matches_authorized, run_background_sync_cycle_with_provider,
    unsubscribe_by_token, update_subscription_authorized, AlertMatchedItem,
    CreateSubscriptionRequest, UpdateSubscriptionPatch,
};
use open_estate::search::matcher::build_candidate_report_highlights;
use open_estate::search::sync::{
    sync_zip_listings_with_provider, ListingInput, LiveZipListingProvider,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Debug, Clone)]
struct CapturedEmail {
    mail_from: String,
    rcpt_to: Vec<String>,
    raw_mime: String,
}

async fn spawn_local_smtp_server() -> (u16, Arc<Mutex<Vec<CapturedEmail>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local TCP SMTP listener");
    let port = listener.local_addr().unwrap().port();
    let captured: Arc<Mutex<Vec<CapturedEmail>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let cap = Arc::clone(&captured_srv);
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let _ = write_half
                    .write_all(b"220 127.0.0.1 ESMTP local-test-smtp\r\n")
                    .await;

                let mut mail_from = String::new();
                let mut rcpt_to: Vec<String> = Vec::new();
                let mut line = String::new();

                loop {
                    line.clear();
                    let Ok(n) = reader.read_line(&mut line).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    let upper = trimmed.to_ascii_uppercase();

                    if upper.starts_with("EHLO ") || upper == "EHLO" {
                        let _ = write_half
                            .write_all(b"250-127.0.0.1\r\n250-8BITMIME\r\n250 SMTPUTF8\r\n")
                            .await;
                    } else if upper.starts_with("HELO ") || upper == "HELO" {
                        let _ = write_half.write_all(b"250 127.0.0.1\r\n").await;
                    } else if upper.starts_with("MAIL FROM:") {
                        mail_from = trimmed["MAIL FROM:".len()..].trim().to_string();
                        rcpt_to.clear();
                        let _ = write_half.write_all(b"250 Ok\r\n").await;
                    } else if upper.starts_with("RCPT TO:") {
                        rcpt_to.push(trimmed["RCPT TO:".len()..].trim().to_string());
                        let _ = write_half.write_all(b"250 Ok\r\n").await;
                    } else if upper == "DATA" {
                        let _ = write_half
                            .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                            .await;
                        let mut data_buf = String::new();
                        loop {
                            let mut data_line = String::new();
                            let Ok(dn) = reader.read_line(&mut data_line).await else {
                                break;
                            };
                            if dn == 0 {
                                break;
                            }
                            let d_trim = data_line.trim_end_matches(['\r', '\n']);
                            if d_trim == "." {
                                break;
                            }
                            if let Some(unescaped) = data_line.strip_prefix("..") {
                                data_buf.push_str(unescaped);
                            } else {
                                data_buf.push_str(&data_line);
                            }
                        }
                        cap.lock().await.push(CapturedEmail {
                            mail_from: mail_from.clone(),
                            rcpt_to: rcpt_to.clone(),
                            raw_mime: data_buf,
                        });
                        let _ = write_half.write_all(b"250 Ok: queued\r\n").await;
                    } else if upper == "RSET" || upper == "NOOP" {
                        let _ = write_half.write_all(b"250 Ok\r\n").await;
                    } else if upper == "QUIT" {
                        let _ = write_half.write_all(b"221 Bye\r\n").await;
                        break;
                    } else {
                        let _ = write_half.write_all(b"250 Ok\r\n").await;
                    }
                }
            });
        }
    });

    (port, captured)
}

fn temp_sqlite_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("openestate_{tag}_{}.db", Uuid::new_v4()))
}

fn cleanup_sqlite_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", path.display())));
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", path.display())));
}

fn load_synth_95118_report() -> Value {
    let mut rep = common::synth_report_primary();
    if let Some(comps) = rep["facts"]["comps"].as_array_mut() {
        for c in comps {
            c["source"] = json!("County Recorder");
        }
    }
    rep["facts"]["nearbyListings"] = json!([
        {
            "address": "301 Synthlisting01 Dr",
            "price": 1_699_000,
            "beds": 4.0,
            "baths": 2.5,
            "sqft": 1920,
            "yearBuilt": 1976
        },
        {
            "address": "302 Synthlisting02 Dr",
            "price": 2_488_888,
            "beds": 3.0,
            "baths": 2.0,
            "sqft": 1650,
            "yearBuilt": 1978
        }
    ]);
    rep["modules"]["hazards"]["data"]["wildfire"]["california"] = json!({
        "fhsz": "Not in a Fire Hazard Severity Zone (LRA NonWildland)",
        "fhszClass": 0
    });
    rep
}

fn synth_95118_listings_from_report(rep: &Value) -> Vec<ListingInput> {
    let city = rep
        .pointer("/geo/city")
        .and_then(Value::as_str)
        .map(String::from);
    let state = rep
        .pointer("/geo/state")
        .and_then(Value::as_str)
        .map(String::from);
    let zip = rep
        .pointer("/geo/zip")
        .and_then(Value::as_str)
        .unwrap_or("95118")
        .to_string();
    let lat = rep.pointer("/geo/lat").and_then(Value::as_f64);
    let lon = rep.pointer("/geo/lon").and_then(Value::as_f64);

    let nearby = rep
        .pointer("/facts/nearbyListings")
        .and_then(Value::as_array)
        .expect("facts.nearbyListings in synthetic report");
    assert!(!nearby.is_empty(), "nearbyListings must not be empty");

    nearby
        .iter()
        .map(|nl| {
            let street = nl
                .get("address")
                .and_then(Value::as_str)
                .expect("nearbyListing address")
                .to_string();
            let full_addr = format!(
                "{street}, {}, {} {zip}",
                city.as_deref().unwrap_or("San Jose"),
                state.as_deref().unwrap_or("CA")
            );
            ListingInput {
                street: Some(street),
                address: full_addr,
                city: city.clone(),
                state: state.clone(),
                zip: zip.clone(),
                lat,
                lon,
                status: None,
                price: nl.get("price").and_then(Value::as_i64),
                list_price: None,
                beds: nl.get("beds").and_then(Value::as_f64),
                baths: nl.get("baths").and_then(Value::as_f64),
                sqft: nl.get("sqft").and_then(Value::as_i64),
                year_built: nl.get("yearBuilt").and_then(Value::as_i64),
                property_type: Some("Single-Family Detached".to_string()),
                source: None,
                ..Default::default()
            }
        })
        .collect()
}

struct TestLiveZipProvider {
    by_zip: Arc<RwLock<HashMap<String, Vec<ListingInput>>>>,
}

impl LiveZipListingProvider for TestLiveZipProvider {
    fn fetch_zip_forsale<'a>(
        &'a self,
        zip: &'a str,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Option<Vec<ListingInput>>> + Send + 'a>> {
        Box::pin(async move {
            let map = self.by_zip.read().await;
            map.get(zip)
                .map(|v| v.iter().take(limit).cloned().collect())
        })
    }
}

fn unfold_mime_headers(raw_mime: &str) -> String {
    raw_mime.replace("\r\n ", " ").replace("\r\n\t", " ")
}

fn decode_qp_mime(raw_mime: &str) -> String {
    unfold_mime_headers(raw_mime)
        .replace("=\r\n", "")
        .replace("=\n", "")
        .replace("=3D", "=")
}

#[test]
fn test_hook_backed_subscription_alerts_smtp_dedup_price_drop_and_unsub() {
    std::thread::Builder::new()
        .name("hook-alerts-16m".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(hook_backed_subscription_alerts_inner());
        })
        .expect("spawn 16MB test thread")
        .join()
        .expect("join 16MB test thread");
}

async fn hook_backed_subscription_alerts_inner() {
    let db_path = temp_sqlite_path("hook_alerts_e2e");
    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .expect("open sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let (smtp_port, captured_emails) = spawn_local_smtp_server().await;

    let mut env_map = HashMap::new();
    env_map.insert("SMTP_HOST".to_string(), "127.0.0.1".to_string());
    env_map.insert("SMTP_PORT".to_string(), smtp_port.to_string());
    env_map.insert("SMTP_SECURE".to_string(), "none".to_string());
    env_map.insert("SMTP_FROM".to_string(), "alerts@open-estate.local".to_string());
    env_map.insert("BRAND_NAME".to_string(), "OpenEstateTest".to_string());
    let env = EnvConfig::from_map(env_map);
    let http = HttpClient::offline_loopback_only(None);

    let fixture_report = load_synth_95118_report();
    let initial_listings = synth_95118_listings_from_report(&fixture_report);

    let by_zip = Arc::new(RwLock::new(HashMap::new()));
    by_zip
        .write()
        .await
        .insert("95118".to_string(), initial_listings.clone());
    let provider = TestLiveZipProvider {
        by_zip: Arc::clone(&by_zip),
    };

    // 1. Subscribe to 95118 (4bd under $1.85M matches 301 Synthlisting01 Dr at $1,699,000, not 302 at $1,888,888)
    let sub_out = create_subscription_with_profile_and_provider(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "buyer@example.com".to_string(),
            name: "Taylor".to_string(),
            zips: vec!["95118".to_string()],
            requirements: "Single family 4+ beds under $1.85M in 95118".to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "instant".to_string(),
            send_welcome_email: true,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        None,
        Some(&provider),
    )
    .await
    .expect("create_subscription_with_profile_and_provider");

    assert_eq!(sub_out["ok"], true);
    assert_eq!(sub_out["welcomeDelivery"]["sent"], true);
    let unsub_token = sub_out["subscription"]["unsubscribeToken"]
        .as_str()
        .expect("unsubscribeToken")
        .to_string();

    {
        let emails = captured_emails.lock().await;
        assert_eq!(emails.len(), 1, "expected 1 welcome email");
        assert_eq!(emails[0].mail_from, "<alerts@open-estate.local>");
        assert_eq!(emails[0].rcpt_to, vec!["<buyer@example.com>"]);
        let unfolded = unfold_mime_headers(&emails[0].raw_mime);
        assert!(
            unfolded.contains(&format!(
                "List-Unsubscribe: <https://openestate.org/api/subscriptions/unsubscribe?token={unsub_token}>"
            )),
            "welcome email missing RFC 8058 List-Unsubscribe header:\n{unfolded}"
        );
        assert!(
            unfolded.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"),
            "welcome email missing RFC 8058 List-Unsubscribe-Post header"
        );
    }

    // 2. First background sync cycle via LiveZipListingProvider -> ingests 95118 & sends new_listing alert
    let cycle1 = run_background_sync_cycle_with_provider(
        &db,
        &http,
        &env,
        "https://openestate.org",
        None,
        5,
        Some(&provider),
    )
    .await
    .expect("cycle 1");

    assert_eq!(cycle1["ok"], true);
    assert_eq!(cycle1["zipsSynced"], 1);
    assert_eq!(
        cycle1["zipReports"][0]["sourcesUsed"],
        serde_json::json!(["Live Area Feed"])
    );
    assert_eq!(cycle1["zipReports"][0]["activeCount"], 2);
    assert_eq!(cycle1["alerts"]["emailsSent"], 1);
    assert_eq!(cycle1["alerts"]["notificationsTriggered"], 1);

    {
        let emails = captured_emails.lock().await;
        assert_eq!(emails.len(), 2, "expected welcome + 1 new-listing alert email");
        let unfolded = unfold_mime_headers(&emails[1].raw_mime);
        assert!(
            unfolded.contains("301 Synthlisting01 Dr"),
            "alert email should contain matching synthetic listing 301 Synthlisting01 Dr"
        );
        assert!(
            unfolded.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"),
            "alert email missing RFC 8058 List-Unsubscribe-Post header"
        );
        assert!(
            unfolded.contains(&format!(
                "List-Unsubscribe: <https://openestate.org/api/subscriptions/unsubscribe?token={unsub_token}>"
            )),
            "alert email missing RFC 8058 List-Unsubscribe header"
        );
    }

    // 3. Second sync cycle (forced) with identical listings -> deduplication prevents duplicate email
    let cycle2 = run_background_sync_cycle_with_provider(
        &db,
        &http,
        &env,
        "https://openestate.org",
        Some("95118"),
        5,
        Some(&provider),
    )
    .await
    .expect("cycle 2");
    assert_eq!(cycle2["changesDetected"], 0);
    assert_eq!(cycle2["alerts"]["emailsSent"], 0);
    assert_eq!(captured_emails.lock().await.len(), 2);

    // 4. Price drop on 301 Synthlisting01 Dr ($1,699,000 -> $1,649,000) -> triggers price_change alert
    {
        let mut guard = by_zip.write().await;
        let list = guard.get_mut("95118").unwrap();
        for item in list.iter_mut() {
            if item.street.as_deref() == Some("301 Synthlisting01 Dr") {
                item.price = Some(1_649_000);
            }
        }
    }

    let cycle3 = run_background_sync_cycle_with_provider(
        &db,
        &http,
        &env,
        "https://openestate.org",
        Some("95118"),
        5,
        Some(&provider),
    )
    .await
    .expect("cycle 3 price drop");

    assert_eq!(cycle3["zipReports"][0]["priceChangeCount"], 1);
    assert_eq!(cycle3["alerts"]["emailsSent"], 1);
    assert_eq!(cycle3["alerts"]["notificationsTriggered"], 1);

    {
        let emails = captured_emails.lock().await;
        assert_eq!(emails.len(), 3, "expected price-drop alert email");
        let unfolded = unfold_mime_headers(&emails[2].raw_mime);
        assert!(unfolded.contains("1,649,000"));
        assert!(unfolded.contains("1,699,000"));
        assert!(unfolded.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"));
    }

    // 5. Verify BRAND_NAME is used for cached report source label ("OpenEstateTest Reports")
    db.put_cached_report(common::SYNTH_PRIMARY_SLUG, &fixture_report, None)
        .await
        .expect("put cached synthetic report");
    let brand_sync =
        sync_zip_listings_with_provider(&db, &http, &env, "95118", false, Some(&provider))
            .await
            .expect("sync with cached report + provider");
    assert!(
        brand_sync
            .sources_used
            .iter()
            .any(|s| s == "OpenEstateTest Reports"),
        "expected dynamic BRAND_NAME Reports label, got {:?}",
        brand_sync.sources_used
    );
    assert!(
        brand_sync.sources_used.iter().any(|s| s == "Live Area Feed"),
        "expected Live Area Feed source label, got {:?}",
        brand_sync.sources_used
    );

    // 6. One-click unsubscribe deactivates subscription and blocks subsequent price drops
    let unsub_res = unsubscribe_by_token(&db, &unsub_token)
        .await
        .expect("unsubscribe_by_token");
    assert_eq!(unsub_res["ok"], true);
    assert_eq!(unsub_res["found"], true);

    {
        let mut guard = by_zip.write().await;
        let list = guard.get_mut("95118").unwrap();
        for item in list.iter_mut() {
            if item.street.as_deref() == Some("301 Synthlisting01 Dr") {
                item.price = Some(1_599_000);
            }
        }
    }

    let cycle4 = run_background_sync_cycle_with_provider(
        &db,
        &http,
        &env,
        "https://openestate.org",
        Some("95118"),
        5,
        Some(&provider),
    )
    .await
    .expect("cycle 4 after unsubscribe");
    assert_eq!(cycle4["alerts"]["emailsSent"], 0);
    assert_eq!(captured_emails.lock().await.len(), 3);

    // Verify HTTP routes: POST /api/subscriptions, POST /api/sync, and RFC 8058 POST /api/subscriptions/unsubscribe
    let mut http_env_map = HashMap::new();
    let http_db_path = temp_sqlite_path("hook_alerts_http");
    http_env_map.insert(
        "SQLITE_PATH".to_string(),
        http_db_path.to_string_lossy().to_string(),
    );
    http_env_map.insert("SMTP_HOST".to_string(), "127.0.0.1".to_string());
    http_env_map.insert("SMTP_PORT".to_string(), smtp_port.to_string());
    http_env_map.insert("SMTP_SECURE".to_string(), "none".to_string());
    http_env_map.insert("SMTP_FROM".to_string(), "alerts@open-estate.local".to_string());
    http_env_map.insert("CRON_SECRET".to_string(), "test-cron-sync-secret".to_string());
    let http_state = AppState::from_env(EnvConfig::from_map(http_env_map))
        .await
        .expect("http AppState");
    let app = build_router(http_state);

    let sub_req = Request::builder()
        .method("POST")
        .uri("/api/subscriptions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "localhost:3000")
        .header(header::ORIGIN, "http://localhost:3000")
        .body(Body::from(
            serde_json::json!({
                "email": "http-sub@example.com",
                "zips": ["95118"],
                "requirements": "3+ beds under $2M in 95118"
            })
            .to_string(),
        ))
        .unwrap();
    let sub_resp = app.clone().oneshot(sub_req).await.unwrap();
    assert_eq!(sub_resp.status(), StatusCode::CREATED);
    let sub_bytes = axum::body::to_bytes(sub_resp.into_body(), 1024 * 64)
        .await
        .unwrap();
    let sub_json: Value = serde_json::from_slice(&sub_bytes).unwrap();
    let http_token = sub_json["subscription"]["unsubscribeToken"]
        .as_str()
        .unwrap()
        .to_string();

    let unauth_sync_req = Request::builder()
        .method("POST")
        .uri("/api/sync")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "localhost:3000")
        .header(header::ORIGIN, "http://localhost:3000")
        .body(Body::from(serde_json::json!({ "zip": "95118" }).to_string()))
        .unwrap();
    let unauth_sync_resp = app.clone().oneshot(unauth_sync_req).await.unwrap();
    assert_eq!(unauth_sync_resp.status(), StatusCode::UNAUTHORIZED);

    let sync_req = Request::builder()
        .method("POST")
        .uri("/api/sync")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer test-cron-sync-secret")
        .body(Body::from(serde_json::json!({ "zip": "95118" }).to_string()))
        .unwrap();
    let sync_resp = app.clone().oneshot(sync_req).await.unwrap();
    assert_eq!(sync_resp.status(), StatusCode::OK);

    let unsub_req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/subscriptions/unsubscribe?token={http_token}"
        ))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("List-Unsubscribe=One-Click"))
        .unwrap();
    let unsub_resp = app.clone().oneshot(unsub_req).await.unwrap();
    assert_eq!(unsub_resp.status(), StatusCode::OK);

    let check_req = Request::builder()
        .method("GET")
        .uri(format!("/api/subscriptions?token={http_token}"))
        .body(Body::empty())
        .unwrap();
    let check_resp = app.oneshot(check_req).await.unwrap();
    assert_eq!(check_resp.status(), StatusCode::OK);
    let check_bytes = axum::body::to_bytes(check_resp.into_body(), 1024 * 64)
        .await
        .unwrap();
    let check_json: Value = serde_json::from_slice(&check_bytes).unwrap();
    assert_eq!(check_json["subscriptions"][0]["active"], false);

    cleanup_sqlite_files(&db_path);
    cleanup_sqlite_files(&http_db_path);
}

#[tokio::test]
async fn test_candidate_report_highlights_extracts_report_data() {
    let db_path = temp_sqlite_path("report_highlights");
    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .expect("open sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let env = EnvConfig::from_map(HashMap::new());
    let rep = load_synth_95118_report();
    let slug = common::SYNTH_PRIMARY_SLUG;
    db.put_cached_report(slug, &rep, None)
        .await
        .expect("cache synthetic report");

    let mut listing = open_estate::search::sync::extract_listings_from_report(&rep)
        .into_iter()
        .next()
        .expect("subject listing");
    listing.report_slug = Some(slug.to_string());
    open_estate::search::sync::upsert_listings_batch(&db, None, &env, &[listing], false)
        .await
        .expect("upsert subject listing");

    let rows = db
        .fetch_listings_sql("SELECT * FROM listings WHERE report_slug = ?", &[slug.into()])
        .await
        .expect("fetch listing row");
    assert_eq!(rows.len(), 1);

    let expected_val = rep["modules"]["valuation"]["data"]["estimate"]["value"]
        .as_f64()
        .unwrap()
        .round() as i64;
    let expected_monthly = rep["modules"]["valuation"]["data"]["ownership"]["monthly"]["total"]
        .as_f64()
        .unwrap()
        .round() as i64;

    let hl = build_candidate_report_highlights(&rep, &rows[0])
        .expect("non-empty reportHighlights");
    assert_eq!(hl["estimateValue"], expected_val);
    assert_eq!(hl["monthlyCost"], expected_monthly);
    let expected_rent = rep["modules"]["valuation"]["data"]["ownership"]["rentEstimate"]
        .as_f64()
        .unwrap()
        .round() as i64;
    assert_eq!(hl["rentEstimate"], expected_rent);
    assert_eq!(hl["floodZone"], "X");
    assert_eq!(hl["seismicPgaG"], 0.58);
    assert_eq!(
        hl["wildfireHazard"],
        "Not in a Fire Hazard Severity Zone (LRA NonWildland)"
    );

    cleanup_sqlite_files(&db_path);
}

#[tokio::test]
async fn test_email_cards_cap_10_photos_and_initial_matches_on_subscribe() {
    let db_path = temp_sqlite_path("email_cards_welcome");
    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .expect("open sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let (smtp_port, captured_emails) = spawn_local_smtp_server().await;
    let mut env_map = HashMap::new();
    env_map.insert("SMTP_HOST".to_string(), "127.0.0.1".to_string());
    env_map.insert("SMTP_PORT".to_string(), smtp_port.to_string());
    env_map.insert("SMTP_SECURE".to_string(), "none".to_string());
    env_map.insert("SMTP_FROM".to_string(), "alerts@open-estate.local".to_string());
    env_map.insert("BRAND_NAME".to_string(), "OpenEstateTest".to_string());
    let env = EnvConfig::from_map(env_map);
    let http = HttpClient::offline_loopback_only(None);

    let mut many_items = Vec::new();
    for idx in 1..=12_i64 {
        let photo = if idx == 1 {
            Some("https://photos.example.org/home-1.jpg")
        } else if idx == 2 {
            Some("javascript:alert(1)")
        } else {
            None
        };
        many_items.push(AlertMatchedItem {
            event_type: if idx == 1 {
                "price_change".to_string()
            } else {
                "new_listing".to_string()
            },
            old_price: if idx == 1 { Some(1_699_000) } else { None },
            event_key: format!("k-{idx}"),
            listing: serde_json::json!({
                "id": format!("l-{idx}"),
                "address": format!("{} Synthlisting Dr, San Jose, CA 95118", 300 + idx),
                "street": format!("{} Synthlisting Dr", 300 + idx),
                "status": "for_sale",
                "price": 1_649_000 - idx * 1_000,
                "beds": 4.0,
                "baths": 2.5,
                "sqft": 1950,
                "photoUrl": photo,
            }),
            match_score: 95 - idx,
            match_tier: "Strong Match".to_string(),
            why_it_matches: "Should never appear in card email".to_string(),
            tradeoffs: Some("Should also never appear".to_string()),
        });
    }

    let payload = build_match_alert_email_with_brand(
        Some("Jordan"),
        "4+ beds under $1.85M in 95118",
        &["95118".to_string()],
        &many_items,
        "https://openestate.org",
        "tok-cap10-test-12345",
        "en-US",
        "OpenEstateTest",
        "open-estate.local",
    );
    assert!(payload.html.contains("https://photos.example.org/home-1.jpg"));
    assert!(!payload.html.contains("javascript:alert(1)"));
    assert!(payload.html.contains("View 2 more matching homes"));
    assert!(payload.html.contains("301 Synthlisting Dr"));
    assert!(!payload.html.contains("312 Synthlisting Dr"));
    assert!(payload.html.contains("Manage alerts"));
    assert!(payload.html.contains("Unsubscribe"));
    assert!(payload.html.contains("View Home"));
    for forbidden in [
        "AI parsed filter",
        "Why it matches",
        "Trade-off",
        "% Match",
        "Should never appear",
        "Should also never appear",
    ] {
        assert!(!payload.html.contains(forbidden), "HTML leaked `{forbidden}`");
        assert!(!payload.text.contains(forbidden), "Text leaked `{forbidden}`");
    }

    let sub0 = create_subscription_with_profile_and_provider(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "minimal-welcome@example.com".to_string(),
            name: "Sam".to_string(),
            zips: vec!["95118".to_string()],
            requirements: "4+ beds under $1.85M in 95118".to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "DAILY".to_string(),
            send_welcome_email: true,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        None,
        None,
    )
    .await
    .expect("0-match subscribe");
    assert_eq!(sub0["subscription"]["frequency"], "daily");
    assert_eq!(sub0["welcomeDelivery"]["sent"], true);
    assert_eq!(sub0["welcomeDelivery"]["initialMatchCount"], 0);

    {
        let emails = captured_emails.lock().await;
        assert_eq!(emails.len(), 1);
        let mime = decode_qp_mime(&emails[0].raw_mime);
        assert!(mime.contains("95118"));
        assert!(mime.contains("Daily digest"));
        assert!(mime.contains("Manage alerts"));
        assert!(mime.contains("Unsubscribe"));
        assert!(!mime.contains("AI parsed filter"));
    }

    let rep = load_synth_95118_report();
    let listings = synth_95118_listings_from_report(&rep);
    open_estate::search::sync::upsert_listings_batch(&db, None, &env, &listings, false)
        .await
        .expect("seed 95118 listings");

    let sub1 = create_subscription_with_profile_and_provider(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "instant-matches@example.com".to_string(),
            name: "Morgan".to_string(),
            zips: vec!["95118".to_string()],
            requirements: "4+ beds under $1.85M in 95118".to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "instant".to_string(),
            send_welcome_email: true,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        None,
        None,
    )
    .await
    .expect("1-match subscribe");
    assert_eq!(sub1["welcomeDelivery"]["sent"], true);
    assert!(sub1["welcomeDelivery"]["initialMatchCount"].as_u64().unwrap() >= 1);

    {
        let emails = captured_emails.lock().await;
        assert_eq!(emails.len(), 2);
        let mime = decode_qp_mime(&emails[1].raw_mime);
        assert!(mime.contains("301 Synthlisting01 Dr"));
        assert!(mime.contains("View Home"));
        assert!(!mime.contains("302 Synthlisting02 Dr"));
        assert!(!mime.contains("AI parsed filter"));
    }

    cleanup_sqlite_files(&db_path);
}

#[tokio::test]
async fn test_daily_weekly_digest_batching_flush_and_authorized_crud_manage_links() {
    assert_eq!(normalize_frequency("DAILY"), "daily");
    assert_eq!(normalize_frequency("Weekly"), "weekly");
    assert_eq!(normalize_frequency("unknown"), "instant");

    let db_path = temp_sqlite_path("digest_and_crud");
    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .expect("open sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let (smtp_port, captured_emails) = spawn_local_smtp_server().await;
    let mut env_map = HashMap::new();
    env_map.insert("SMTP_HOST".to_string(), "127.0.0.1".to_string());
    env_map.insert("SMTP_PORT".to_string(), smtp_port.to_string());
    env_map.insert("SMTP_SECURE".to_string(), "none".to_string());
    env_map.insert("SMTP_FROM".to_string(), "alerts@open-estate.local".to_string());
    env_map.insert("FREE_MAX_ACTIVE_NL_ALERTS".to_string(), "1".to_string());
    let env = EnvConfig::from_map(env_map);
    let http = HttpClient::offline_loopback_only(None);

    let daily_out = create_subscription_with_profile_and_provider(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "digest@example.com".to_string(),
            name: "Daily Buyer".to_string(),
            zips: vec!["95118".to_string()],
            requirements: "4+ beds under $1.85M in 95118".to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "daily".to_string(),
            send_welcome_email: false,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        None,
        None,
    )
    .await
    .unwrap();
    let daily_id = daily_out["subscription"]["id"].as_str().unwrap().to_string();
    let daily_tok = daily_out["subscription"]["unsubscribeToken"]
        .as_str()
        .unwrap()
        .to_string();

    let weekly_out = create_subscription_with_profile_and_provider(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "weekly@example.com".to_string(),
            name: "Weekly Buyer".to_string(),
            zips: vec!["95118".to_string()],
            requirements: "4+ beds under $1.85M in 95118".to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "weekly".to_string(),
            send_welcome_email: false,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        None,
        None,
    )
    .await
    .unwrap();
    let weekly_id = weekly_out["subscription"]["id"].as_str().unwrap().to_string();

    let rep = load_synth_95118_report();
    let listings = synth_95118_listings_from_report(&rep);
    let batch1 = open_estate::search::sync::upsert_listings_batch(&db, None, &env, &listings, true)
        .await
        .unwrap();
    let eval1 = evaluate_alerts_for_listings(
        &db,
        &http,
        &env,
        &batch1.changed_listings,
        "https://openestate.org",
    )
    .await
    .unwrap();
    assert_eq!(eval1["emailsSent"], 0);
    assert_eq!(eval1["notificationsTriggered"], 0);
    assert_eq!(eval1["queuedDigests"], 2);
    assert_eq!(captured_emails.lock().await.len(), 0);

    let daily_row = db.fetch_subscription_by_id(&daily_id).await.unwrap().unwrap();
    assert!(daily_row.last_notified_at.is_none());
    assert!(daily_row
        .digest_pending_json
        .as_deref()
        .unwrap_or("")
        .contains("301 Synthlisting01 Dr"));

    let past_25h = (chrono::Utc::now() - chrono::Duration::hours(25))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    db.execute_params(
        "UPDATE subscriptions SET created_at = ? WHERE id IN (?, ?)",
        &[past_25h.into(), daily_id.as_str().into(), weekly_id.as_str().into()],
    )
    .await
    .unwrap();

    let empty_provider = TestLiveZipProvider {
        by_zip: Arc::new(RwLock::new(HashMap::new())),
    };
    let sync_res = run_background_sync_cycle_with_provider(
        &db,
        &http,
        &env,
        "https://openestate.org",
        Some("95118"),
        1,
        Some(&empty_provider),
    )
    .await
    .unwrap();
    assert_eq!(sync_res["digestFlush"]["flushedSubscriptions"], 1);
    assert_eq!(sync_res["digestFlush"]["emailsSent"], 1);
    assert_eq!(captured_emails.lock().await.len(), 1);

    let daily_after = db.fetch_subscription_by_id(&daily_id).await.unwrap().unwrap();
    assert!(daily_after.last_notified_at.is_some());
    assert!(daily_after.digest_pending_json.is_none());

    let weekly_still = db.fetch_subscription_by_id(&weekly_id).await.unwrap().unwrap();
    assert!(weekly_still.last_notified_at.is_none());
    assert!(weekly_still.digest_pending_json.is_some());

    let past_8d = (chrono::Utc::now() - chrono::Duration::days(8))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    db.execute_params(
        "UPDATE subscriptions SET created_at = ? WHERE id = ?",
        &[past_8d.into(), weekly_id.as_str().into()],
    )
    .await
    .unwrap();
    let flush_wk = flush_due_subscription_digests(&db, &env, "https://openestate.org")
        .await
        .unwrap();
    assert_eq!(flush_wk["flushedSubscriptions"], 1);
    assert_eq!(flush_wk["emailsSent"], 1);
    assert_eq!(captured_emails.lock().await.len(), 2);

    let by_email = list_subscriptions(&db, Some("digest@example.com"), None)
        .await
        .unwrap();
    assert_eq!(by_email.len(), 1);
    assert_eq!(by_email[0]["frequency"], "daily");
    assert!(by_email[0].get("unsubscribeToken").is_none());

    let bad_upd = update_subscription_authorized(
        &db,
        &http,
        &env,
        &daily_id,
        Some("wrong-token-123456"),
        None,
        UpdateSubscriptionPatch {
            name: Some("Hacked".to_string()),
            ..Default::default()
        },
    )
    .await;
    assert!(bad_upd.is_err());

    let link_res = create_and_send_manage_link(
        &db,
        &env,
        "digest@example.com",
        "https://openestate.org",
        "en-US",
    )
    .await
    .unwrap();
    assert_eq!(link_res["sent"], true);

    let manage_tok = {
        let emails = captured_emails.lock().await;
        assert_eq!(emails.len(), 3);
        let mime = decode_qp_mime(&emails[2].raw_mime);
        let marker = "/alerts?manage=";
        let start = mime.find(marker).expect("manage link in email") + marker.len();
        mime[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
    };
    assert!(manage_tok.len() >= 12);

    let via_manage = list_subscriptions_by_manage_token(&db, &manage_tok)
        .await
        .unwrap()
        .expect("valid manage token");
    assert_eq!(via_manage.0, "digest@example.com");
    assert_eq!(via_manage.1.len(), 1);

    let upd_res = update_subscription_authorized(
        &db,
        &http,
        &env,
        &daily_id,
        None,
        Some(&manage_tok),
        UpdateSubscriptionPatch {
            name: Some("Updated Alert".to_string()),
            frequency: Some("weekly".to_string()),
            active: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(upd_res["ok"], true);
    assert_eq!(upd_res["subscription"]["name"], "Updated Alert");
    assert_eq!(upd_res["subscription"]["frequency"], "weekly");
    assert_eq!(upd_res["subscription"]["active"], false);

    assert!(list_subscriptions(&db, Some("digest@example.com"), None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        list_subscriptions(&db, None, Some(&daily_tok)).await.unwrap()[0]["active"],
        false
    );
    assert_eq!(
        list_subscriptions_by_manage_token(&db, &manage_tok)
            .await
            .unwrap()
            .unwrap()
            .1[0]["active"],
        false
    );

    let preview = preview_subscription_matches_authorized(
        &db,
        &http,
        &env,
        &daily_id,
        None,
        Some(&manage_tok),
        10,
    )
    .await
    .unwrap();
    assert_eq!(preview["ok"], true);
    assert!(preview["count"].as_u64().unwrap() >= 1);

    let _second = create_subscription_with_profile_and_provider(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "digest@example.com".to_string(),
            name: "Active One".to_string(),
            zips: vec!["95118".to_string()],
            requirements: "3+ beds under $2M in 95118".to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "instant".to_string(),
            send_welcome_email: false,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        None,
        None,
    )
    .await
    .unwrap();

    let resume_blocked = update_subscription_authorized(
        &db,
        &http,
        &env,
        &daily_id,
        Some(&daily_tok),
        None,
        UpdateSubscriptionPatch {
            active: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(resume_blocked["ok"], false);
    assert_eq!(resume_blocked["code"], "PAYWALL_ALERT_LIMIT_REACHED");
    assert_eq!(resume_blocked["status"], 402);

    let del_res = delete_subscription_authorized(&db, &daily_id, None, Some(&manage_tok))
        .await
        .unwrap();
    assert_eq!(del_res["ok"], true);
    assert_eq!(del_res["deleted"], true);
    assert!(db.fetch_subscription_by_id(&daily_id).await.unwrap().is_none());

    cleanup_sqlite_files(&db_path);
}
