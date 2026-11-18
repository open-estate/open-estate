//! Comprehensive Security & Auth Hardening Regression Suite:
//! - Unified internal API-key / signed `oe_session` guard (`OPEN_ESTATE_REQUIRE_INTERNAL_AUTH`)
//! - Spoofable `CF-Connecting-IP` / `CF-IPCountry` / `X-Forwarded-For` defaults (`false`)
//! - Minimal non-leaking `/api/health`
//! - Fail-closed `401` on `/api/sync`, `/api/listings/ingest`, `/api/property/:slug/platform-signals`,
//!   `GET /api/listings/shop?syncIfEmpty=true`, and `/api/scan/*`
//! - `/ops` & `/api/ops/telemetry` enforcement for `ADMIN_REQUIRE_ADMIN_HOST` + `ADMIN_PROXY_AUTH_ONLY`
//! - Strict `verify_same_origin` & `verify_admin_csrf` rejecting missing `Origin`/`Referer`
//! - Subscription email enumeration block & safe GET unsubscribe confirmation page

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use open_estate::{
    db::DbStore,
    guardrails::{extract_trusted_client_ip, verify_same_origin, HttpDefenseShield, ShieldLimits},
    shared::{EnvConfig, HttpClient},
    web::{
        admin_console::{admin_host_redirect_layer, admin_routes},
        render::render_report_html,
        server::{build_router, AppState},
    },
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

struct SecTestServer {
    base_url: String,
    state: AppState,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SecTestServer {
    async fn start(extra_env: Vec<(&str, &str)>) -> Self {
        let uid = Uuid::new_v4().simple().to_string();
        let temp_dir = std::env::temp_dir().join(format!("oe_sec_test_{uid}"));
        let reports_dir = temp_dir.join("reports");
        let http_cache_dir = temp_dir.join("http_cache");
        let db_path = temp_dir.join("sec.sqlite");
        std::fs::create_dir_all(&reports_dir).unwrap();
        std::fs::create_dir_all(&http_cache_dir).unwrap();

        let mut pairs = HashMap::from([
            ("OFFLINE_BLOCK_EXTERNAL".to_string(), "true".to_string()),
            ("SQLITE_PATH".to_string(), db_path.to_string_lossy().into_owned()),
            ("REPORTS_DIR".to_string(), reports_dir.to_string_lossy().into_owned()),
            ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
        ]);
        for (k, v) in extra_env {
            pairs.insert(k.to_string(), v.to_string());
        }
        let env = EnvConfig::from_map(pairs);
        let http = HttpClient::from_env(&env, Some(http_cache_dir));
        let db = DbStore::connect(&env).await.unwrap();
        db.ensure_schema().await.unwrap();
        let shield = HttpDefenseShield::new(&env);
        let state = AppState::with_components(env, http, db, shield, reports_dir, None);

        let router = build_router(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
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
            base_url: format!("http://{addr}"),
            state,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for SecTestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn seed_report_json(slug: &str, address: &str) -> Value {
    json!({
        "meta": {
            "version": "1.0.0-rust",
            "slug": slug,
            "displayAddress": address,
            "aiPending": false
        },
        "geo": {
            "matched": true,
            "displayAddress": address,
            "zip": "95118",
            "lat": 37.2528,
            "lon": -121.8950
        },
        "facts": {
            "beds": 4,
            "baths": 2.0,
            "sqft": 1850
        },
        "modules": {},
        "summary": {
            "scores": { "overall": 82 },
            "verdict": "Synthetic test report"
        }
    })
}

fn extract_oe_cookie(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for val in headers.get_all(reqwest::header::SET_COOKIE) {
        if let Ok(s) = val.to_str() {
            if let Some(pair) = s.split(';').next().map(str::trim) {
                if pair.starts_with("oe_session=") {
                    return Some(pair.to_string());
                }
            }
        }
    }
    None
}

fn extract_meta_csrf_token(html: &str) -> Option<String> {
    let needle = r#"meta name="oe-csrf-token" content=""#;
    let start = html.find(needle)? + needle.len();
    let end = start + html[start..].find('"')?;
    Some(html[start..end].to_string())
}

#[tokio::test]
async fn test_internal_auth_blocks_unauthenticated_api_allows_browser_session_and_api_keys() {
    let api_key = "oe-test-internal-secret-2026";
    let srv = SecTestServer::start(vec![
        ("OPEN_ESTATE_REQUIRE_INTERNAL_AUTH", "true"),
        ("OPEN_ESTATE_INTERNAL_API_KEY", api_key),
        ("OPEN_ESTATE_SESSION_SECRET", "session-hmac-test-secret-01"),
    ])
    .await;

    let slug = "420-sec-test-st-san-jose-ca-95118";
    let rep = seed_report_json(slug, "420 Sec Test St, San Jose, CA 95118");
    let html = render_report_html(&rep);
    srv.state
        .db
        .put_cached_report(slug, &rep, Some(&html))
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // 1. Unauthenticated /api/* & /r/:slug/report.json MUST fail closed with 401
    for (method, path, body) in [
        (reqwest::Method::GET, "/api/suggest?q=95118", None),
        (reqwest::Method::GET, "/api/area/forsale?zip=95118", None),
        (
            reqwest::Method::GET,
            &format!("/api/report?slug={slug}"),
            None,
        ),
        (
            reqwest::Method::POST,
            "/api/report/enqueue",
            Some(json!({ "address": "123 Synthsec St, San Jose, CA 95118" })),
        ),
        (
            reqwest::Method::POST,
            "/api/scan/batch",
            Some(json!({ "addresses": ["123 Synthsec St, San Jose, CA 95118"] })),
        ),
        (
            reqwest::Method::GET,
            &format!("/r/{slug}/report.json"),
            None,
        ),
        (reqwest::Method::GET, "/api/geo/ip", None),
        (reqwest::Method::GET, "/api/listings/shop", None),
    ] {
        let mut req = client.request(method.clone(), format!("{}{path}", srv.base_url));
        if let Some(b) = body {
            req = req.header("Origin", &srv.base_url).json(&b);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expected 401 on unauthenticated {method} {path}"
        );
    }

    // 2. Unauthenticated GET /report?address=<uncached> triggers ZERO background build
    let uncached_api_report = client
        .get(format!(
            "{}/report?address=777+Uncached+Security+Way%2C+San+Jose%2C+CA+95118",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(uncached_api_report.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(srv.state.job_mgr.jobs_by_slug.len(), 0);

    let uncached_html_report = client
        .get(format!(
            "{}/report?address=777+Uncached+Security+Way%2C+San+Jose%2C+CA+95118",
            srv.base_url
        ))
        .header("Accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(uncached_html_report.status(), StatusCode::OK);
    assert_eq!(
        srv.state.job_mgr.jobs_by_slug.len(),
        0,
        "HTML shell on GET /report must not build uncached reports server-side without credentials"
    );

    // 3. Exempt probes, minimal /api/health, and i18n remain reachable without credentials
    assert_eq!(
        client
            .get(format!("{}/healthz", srv.base_url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("{}/readyz", srv.base_url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("{}/api/i18n/en-US", srv.base_url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let health: Value = client
        .get(format!("{}/api/health", srv.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], json!(true));
    assert_eq!(health["status"], json!("ok"));
    assert!(health["ai"]["configured"].is_boolean());
    for sensitive in [
        "egress",
        "database",
        "guardrails",
        "scanner",
        "connectors",
        "syncConnectors",
        "smtp",
    ] {
        assert!(
            health.get(sensitive).is_none(),
            "/api/health must not expose `{sensitive}`: {health}"
        );
    }

    // 4. Browser flow: GET / issues signed HttpOnly oe_session cookie + <meta name="oe-csrf-token">
    let home = client.get(format!("{}/", srv.base_url)).send().await.unwrap();
    assert_eq!(home.status(), StatusCode::OK);
    let set_cookie_raw = home
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("oe_session="))
        .expect("expected oe_session Set-Cookie")
        .to_string();
    assert!(set_cookie_raw.contains("HttpOnly"));
    assert!(set_cookie_raw.contains("SameSite=Lax"));
    let oe_cookie = extract_oe_cookie(home.headers()).unwrap();
    let home_html = home.text().await.unwrap();
    let csrf_tok = extract_meta_csrf_token(&home_html).expect("expected oe-csrf-token meta tag");

    // Cookie WITHOUT X-OpenEstate-Token is rejected on mutating & compute endpoints
    let missing_tok = client
        .get(format!("{}/api/report?slug={slug}", srv.base_url))
        .header("Cookie", &oe_cookie)
        .header("Origin", &srv.base_url)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_tok.status(), StatusCode::UNAUTHORIZED);

    // Cookie + X-OpenEstate-Token + Origin unlocks browser API endpoints
    let ok_suggest = client
        .get(format!("{}/api/suggest?q=95118", srv.base_url))
        .header("Cookie", &oe_cookie)
        .header("X-OpenEstate-Token", &csrf_tok)
        .header("Origin", &srv.base_url)
        .send()
        .await
        .unwrap();
    assert_eq!(ok_suggest.status(), StatusCode::OK);

    let ok_report = client
        .get(format!("{}/api/report?slug={slug}", srv.base_url))
        .header("Cookie", &oe_cookie)
        .header("X-OpenEstate-Token", &csrf_tok)
        .header("Origin", &srv.base_url)
        .send()
        .await
        .unwrap();
    assert_eq!(ok_report.status(), StatusCode::OK);

    // Spoofed X-User-Id on /api/me/profile is ignored in favor of oe_session `sid`
    let valid_profile = serde_json::to_value(open_estate::scoring::profile::Profile::default()).unwrap();
    let put_prof = client
        .put(format!("{}/api/me/profile", srv.base_url))
        .header("Cookie", &oe_cookie)
        .header("X-OpenEstate-Token", &csrf_tok)
        .header("X-User-Id", "victim-account-999")
        .header("Origin", &srv.base_url)
        .json(&valid_profile)
        .send()
        .await
        .unwrap();
    assert_eq!(put_prof.status(), StatusCode::OK);
    let put_json: Value = put_prof.json().await.unwrap();
    let resolved_uid = put_json["userId"].as_str().unwrap();
    assert!(
        resolved_uid.starts_with("oe_"),
        "expected oe_session-bound userId, got {resolved_uid}"
    );
    assert_ne!(resolved_uid, "victim-account-999");

    // 5. OPEN_ESTATE_INTERNAL_API_KEY unlocks via header or Bearer
    let via_hdr = client
        .get(format!("{}/api/suggest?q=95118", srv.base_url))
        .header("X-OpenEstate-Internal-Key", api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(via_hdr.status(), StatusCode::OK);

    let via_bearer = client
        .get(format!("{}/r/{slug}/report.json", srv.base_url))
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(via_bearer.status(), StatusCode::OK);

    // 6. Spoofed CF-IPCountry ignored by default (trust_proxy=false, trust_cf_headers=false)
    let spoof_geo: Value = client
        .get(format!("{}/api/geo/ip", srv.base_url))
        .header("X-OpenEstate-Internal-Key", api_key)
        .header("cf-ipcountry", "US")
        .header("cf-postal-code", "94043")
        .header("cf-ipcity", "Mountain View")
        .header("cf-region-code", "CA")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(
        spoof_geo.get("source").and_then(|v| v.as_str()),
        Some("edge_cloudflare"),
        "spoofed CF headers must be ignored by default: {spoof_geo}"
    );
}

#[tokio::test]
async fn test_fail_closed_operator_batch_ops_csrf_and_subscription_safeguards() {
    // ShieldLimits default must have trust_proxy = false and trust_cf_headers = false
    let defaults = ShieldLimits::default();
    assert!(!defaults.trust_proxy);
    assert!(!defaults.trust_cf_headers);
    let mut cf_hdrs = axum::http::HeaderMap::new();
    cf_hdrs.insert("cf-connecting-ip", "203.0.113.99".parse().unwrap());
    assert_eq!(
        extract_trusted_client_ip(&cf_hdrs, Some("10.0.0.5".parse().unwrap()), true),
        "10.0.0.5"
    );

    // Empty Origin + empty Referer must be rejected unless Sec-Fetch-Site == same-origin
    let env_default = EnvConfig::default();
    let mut bare_post_hdrs = axum::http::HeaderMap::new();
    bare_post_hdrs.insert("content-type", "application/json".parse().unwrap());
    assert_eq!(
        verify_same_origin(&bare_post_hdrs, "example.com", &env_default)
            .unwrap_err()
            .status,
        403
    );
    bare_post_hdrs.insert("sec-fetch-site", "same-origin".parse().unwrap());
    assert!(verify_same_origin(&bare_post_hdrs, "example.com", &env_default).is_ok());

    // Fail closed when CRON_SECRET and OPEN_ESTATE_BATCH_TOKEN are UNSET even with OPEN_ESTATE_BATCH_REQUIRE_TOKEN=true
    let srv = SecTestServer::start(vec![("OPEN_ESTATE_BATCH_REQUIRE_TOKEN", "true")]).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    for (method, path, body) in [
        (reqwest::Method::POST, "/api/sync", Some(json!({ "zip": "95118" }))),
        (
            reqwest::Method::POST,
            "/api/property/100-main-st-95118/platform-signals",
            Some(json!({ "zillow": { "saves": 1 } })),
        ),
        (
            reqwest::Method::GET,
            "/api/listings/shop?zip=95118&syncIfEmpty=true",
            None,
        ),
    ] {
        let mut req = client
            .request(method.clone(), format!("{}{path}", srv.base_url))
            .header("Origin", &srv.base_url);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expected fail-closed 401 on {method} {path}"
        );
    }

    for (method, path) in [
        (reqwest::Method::POST, "/api/listings/ingest"),
        (reqwest::Method::POST, "/api/scan/batch"),
        (reqwest::Method::GET, "/api/scan/batch/nonexistent"),
        (reqwest::Method::POST, "/api/scan/batch/nonexistent/cancel"),
        (reqwest::Method::GET, "/api/scan/metrics"),
    ] {
        let resp = client
            .request(method.clone(), format!("{}{path}", srv.base_url))
            .header("Origin", &srv.base_url)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "expected 404 on removed commercial/batch route {method} {path}"
        );
    }

    // Create a subscription -> verify email enumeration blocked & GET unsubscribe does NOT mutate active=0
    let sub_create_resp = client
        .post(format!("{}/api/subscriptions", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "email": "alert-sec-test@example.com",
            "zips": ["95118"],
            "requirements": "4+ beds under $2M in 95118"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(sub_create_resp.status(), StatusCode::CREATED);
    let sub_create: Value = sub_create_resp.json().await.unwrap();
    let unsub_tok = sub_create["subscription"]["unsubscribeToken"]
        .as_str()
        .unwrap()
        .to_string();

    let enum_resp = client
        .get(format!(
            "{}/api/subscriptions?email=alert-sec-test@example.com",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(enum_resp.status(), StatusCode::UNAUTHORIZED);

    let unsub_get = client
        .get(format!(
            "{}/api/subscriptions/unsubscribe?token={unsub_tok}",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unsub_get.status(), StatusCode::OK);
    let unsub_html = unsub_get.text().await.unwrap();
    assert!(unsub_html.contains("method=\"post\""));

    // Verify subscription is STILL active=true after GET confirmation page!
    let check_still_active: Value = client
        .get(format!(
            "{}/api/subscriptions?token={unsub_tok}",
            srv.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(check_still_active["subscriptions"][0]["active"], json!(true));

    // POST unsubscribe actually deactivates it
    let unsub_post = client
        .post(format!(
            "{}/api/subscriptions/unsubscribe?token={unsub_tok}",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unsub_post.status(), StatusCode::OK);
    let check_unsubbed: Value = client
        .get(format!(
            "{}/api/subscriptions?token={unsub_tok}",
            srv.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(check_unsubbed["subscriptions"][0]["active"], json!(false));

    // Verify /ops + /api/ops/telemetry obey ADMIN_REQUIRE_ADMIN_HOST and ADMIN_PROXY_AUTH_ONLY
    let uid = Uuid::new_v4().simple().to_string();
    let tmp_dir = std::env::temp_dir().join(format!("oe_ops_sec_{uid}"));
    let rep_dir = tmp_dir.join("reports");
    std::fs::create_dir_all(&rep_dir).unwrap();
    let ops_env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_PATH".to_string(),
            tmp_dir.join("ops.sqlite").to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            rep_dir.to_string_lossy().into_owned(),
        ),
        ("ADMIN_HOST".to_string(), "admin.openestate.local".to_string()),
        ("ADMIN_REQUIRE_ADMIN_HOST".to_string(), "true".to_string()),
        ("ADMIN_PROXY_AUTH_ENABLED".to_string(), "true".to_string()),
        ("ADMIN_PROXY_AUTH_ONLY".to_string(), "true".to_string()),
        ("ADMIN_PROXY_AUTH_SECRET".to_string(), "proxy-sec-987".to_string()),
        (
            "OPEN_ESTATE_OPS_TOKEN".to_string(),
            "ops-tok-should-not-bypass".to_string(),
        ),
    ]));
    let ops_db = DbStore::connect(&ops_env).await.unwrap();
    ops_db.ensure_schema().await.unwrap();
    let ops_state = AppState::with_components(
        ops_env,
        HttpClient::from_env(&EnvConfig::default(), None),
        ops_db,
        HttpDefenseShield::default(),
        rep_dir,
        None,
    );
    let ops_app = axum::Router::new()
        .route(
            "/ops",
            axum::routing::get(open_estate::web::ops_dashboard::handle_ops_dashboard_page),
        )
        .route(
            "/api/ops/telemetry",
            axum::routing::get(open_estate::web::ops_dashboard::handle_ops_telemetry_json),
        )
        .merge(admin_routes())
        .layer(admin_host_redirect_layer(ops_state.clone()))
        .with_state(ops_state);

    // Public host /ops -> 302 to admin host; /api/ops/telemetry -> 404
    let pub_ops = ops_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ops")
                .header(header::HOST, "public.openestate.local")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pub_ops.status(), StatusCode::FOUND);

    let pub_ops_api = ops_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ops/telemetry")
                .header(header::HOST, "public.openestate.local")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pub_ops_api.status(), StatusCode::NOT_FOUND);

    // Admin host with OPEN_ESTATE_OPS_TOKEN alone rejected under ADMIN_PROXY_AUTH_ONLY=true
    let tok_ops_api = ops_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ops/telemetry")
                .header(header::HOST, "admin.openestate.local")
                .header(header::AUTHORIZATION, "Bearer ops-tok-should-not-bypass")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tok_ops_api.status(), StatusCode::UNAUTHORIZED);

    // Admin host with missing Origin on mutating /api/admin/modules/.../toggle -> 403 CSRF
    let missing_origin_admin = ops_app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/modules/natural_hazards/toggle")
                .header(header::HOST, "admin.openestate.local")
                .header("x-auth-request-email", "admin@openestate.local")
                .header("x-proxy-auth-secret", "proxy-sec-987")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"enabled":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_origin_admin.status(), StatusCode::FORBIDDEN);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[derive(Debug, Default)]
struct TestExtHooks;

impl open_estate::web::hooks::ServerExtensionHooks for TestExtHooks {
    fn extra_routes(&self) -> (axum::Router<AppState>, axum::Router<AppState>) {
        use axum::routing::{get, post};
        let small = axum::Router::new()
            .route(
                "/api/commercial/config",
                get(|| async { (StatusCode::OK, r#"{"ok":true}"#) }),
            )
            .route(
                "/api/auth/otp/request",
                post(|| async { (StatusCode::OK, r#"{"ok":true}"#) }),
            )
            .route(
                "/api/auth/verify",
                get(|| async { (StatusCode::OK, r#"{"ok":true}"#) }),
            );
        (small, axum::Router::new())
    }

    fn authorize_report<'a>(
        &'a self,
        state: &'a AppState,
        slug: &'a str,
        rec_opt: Option<open_estate::db::CachedReportRecord>,
        headers: &'a axum::http::HeaderMap,
        _raw_query: &'a str,
        html_404: bool,
    ) -> open_estate::web::hooks::BoxFuture<
        'a,
        std::result::Result<open_estate::web::hooks::AuthorizedReport, axum::response::Response>,
    > {
        Box::pin(async move {
            let allow = headers
                .get("x-test-allow")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| s == "1");
            if !allow && matches!(slug, "private-agent-slug" | "999-synthext-ln-san-jose-ca-95118")
            {
                return Err(open_estate::web::http_util::json_response(
                    StatusCode::FORBIDDEN,
                    json!({ "ok": false, "code": "REPORT_ACCESS_DENIED" }),
                ));
            }
            open_estate::web::hooks::default_authorize_report(&state.env, rec_opt, html_404)
        })
    }
}

#[tokio::test]
async fn test_verifier_audit_regressions_admin_csrf_auth_exemptions_and_acl() {
    use open_estate::web::ops_dashboard::verify_ops_admin_access;
    use std::sync::Arc;

    // A) FAIL #1 + ops_dashboard proxy-only parity:
    let uid = Uuid::new_v4().simple().to_string();
    let tmp_dir = std::env::temp_dir().join(format!("oe_verifier_reg_{uid}"));
    let rep_dir = tmp_dir.join("reports");
    let cache_dir = tmp_dir.join("http_cache");
    std::fs::create_dir_all(&rep_dir).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();

    let proxy_only_env = EnvConfig::from_map(HashMap::from([
        ("SQLITE_PATH".to_string(), tmp_dir.join("admin.sqlite").to_string_lossy().into_owned()),
        ("REPORTS_DIR".to_string(), rep_dir.to_string_lossy().into_owned()),
        ("OFFLINE_BLOCK_EXTERNAL".to_string(), "true".to_string()),
        ("ADMIN_PROXY_AUTH_ONLY".to_string(), "true".to_string()),
        ("OPEN_ESTATE_OPS_TOKEN".to_string(), "ops-secret-999".to_string()),
    ]));
    let proxy_db = DbStore::connect(&proxy_only_env).await.unwrap();
    proxy_db.ensure_schema().await.unwrap();
    let proxy_state = AppState::with_components(
        proxy_only_env.clone(),
        HttpClient::from_env(&proxy_only_env, Some(cache_dir.clone())),
        proxy_db,
        HttpDefenseShield::default(),
        rep_dir.clone(),
        None,
    );
    let mut tok_headers = axum::http::HeaderMap::new();
    tok_headers.insert(header::AUTHORIZATION, "Bearer ops-secret-999".parse().unwrap());
    let ops_only_res = verify_ops_admin_access(
        &proxy_state,
        &tok_headers,
        &axum::http::Extensions::new(),
    );
    assert!(
        ops_only_res.is_err(),
        "ADMIN_PROXY_AUTH_ONLY=true alone (without ADMIN_PROXY_AUTH_ENABLED) must block ops token"
    );

    let csrf_env = EnvConfig::from_map(HashMap::from([
        ("SQLITE_PATH".to_string(), tmp_dir.join("csrf.sqlite").to_string_lossy().into_owned()),
        ("REPORTS_DIR".to_string(), rep_dir.to_string_lossy().into_owned()),
        ("OFFLINE_BLOCK_EXTERNAL".to_string(), "true".to_string()),
        ("OPEN_ESTATE_OPS_TOKEN".to_string(), "ops-csrf-tok".to_string()),
    ]));
    let csrf_db = DbStore::connect(&csrf_env).await.unwrap();
    csrf_db.ensure_schema().await.unwrap();
    let csrf_state = AppState::with_components(
        csrf_env.clone(),
        HttpClient::from_env(&csrf_env, Some(cache_dir.clone())),
        csrf_db,
        HttpDefenseShield::default(),
        rep_dir.clone(),
        None,
    );
    let csrf_app = build_router(csrf_state);

    // Cookie-authenticated admin/ops mutation WITHOUT Origin/Sec-Fetch-Site MUST fail 403 CSRF
    for endpoint in [
        "/api/admin/extensions/census_geocoder/toggle",
        "/api/admin/subscriptions",
    ] {
        let missing_origin = csrf_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(endpoint)
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::COOKIE, "open_estate_ops_token=ops-csrf-tok")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            missing_origin.status(),
            StatusCode::FORBIDDEN,
            "{endpoint} must reject cookie-authenticated POST lacking Origin/Sec-Fetch-Site"
        );
    }

    // B) FAIL #2, #3A, #3B, no-304 on internal auth, and oe_session priority over open_estate_vid:
    let ext_env = EnvConfig::from_map(HashMap::from([
        ("OFFLINE_BLOCK_EXTERNAL".to_string(), "true".to_string()),
        ("SQLITE_PATH".to_string(), tmp_dir.join("ext.sqlite").to_string_lossy().into_owned()),
        ("REPORTS_DIR".to_string(), rep_dir.to_string_lossy().into_owned()),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
        ("OPEN_ESTATE_REQUIRE_INTERNAL_AUTH".to_string(), "true".to_string()),
        ("OPEN_ESTATE_SESSION_SECRET".to_string(), "sec-reg-hmac-key-2026".to_string()),
    ]));
    let ext_db = DbStore::connect(&ext_env).await.unwrap();
    ext_db.ensure_schema().await.unwrap();
    let priv_agent_slug = "private-agent-slug";
    let priv_ext_addr = "999 SynthExt Ln, San Jose, CA 95118";
    let priv_ext_slug = "999-synthext-ln-san-jose-ca-95118";
    let pub_slug = "111-synthetag-ln-san-jose-ca-95118";
    for (s, a) in [
        (priv_agent_slug, "888 SynthAgent Ln, San Jose, CA 95118"),
        (priv_ext_slug, priv_ext_addr),
        (pub_slug, "111 SynthEtag Ln, San Jose, CA 95118"),
    ] {
        let rep = seed_report_json(s, a);
        let html = render_report_html(&rep);
        ext_db.put_cached_report(s, &rep, Some(&html)).await.unwrap();
    }
    ext_db
        .record_user_search(
            "vid:forged_vid_123",
            priv_agent_slug,
            None,
            Some("888 SynthAgent Ln, San Jose, CA 95118"),
        )
        .await
        .unwrap();
    let ext_state = AppState::with_components(
        ext_env.clone(),
        HttpClient::from_env(&ext_env, Some(cache_dir)),
        ext_db,
        HttpDefenseShield::default(),
        rep_dir,
        None,
    )
    .with_extension_hooks(Arc::new(TestExtHooks));
    let ext_app = build_router(ext_state.clone());

    // FAIL #2: /api/commercial/config and /api/auth/otp/request MUST require internal auth;
    // GET /api/auth/verify remains exempt for email links.
    let unauth_cfg = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/commercial/config")
                .header(header::HOST, "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_cfg.status(), StatusCode::UNAUTHORIZED);

    let unauth_otp = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/otp/request")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::ORIGIN, "http://127.0.0.1:8080")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"email":"buyer@example.com"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_otp.status(), StatusCode::UNAUTHORIZED);

    let exempt_verify = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/verify?token=abc")
                .header(header::HOST, "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exempt_verify.status(), StatusCode::OK);

    // Mint oe_session + CSRF token via GET /r/{pub_slug}/ even with If-None-Match (must NOT 304!)
    let report_resp = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/r/{pub_slug}/"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::IF_NONE_MATCH, "*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(report_resp.status(), StatusCode::OK);
    assert!(
        report_resp.headers().get(header::ETAG).is_none(),
        "GET /r/:slug must omit ETag when OPEN_ESTATE_REQUIRE_INTERNAL_AUTH=true"
    );
    let oe_cookie = report_resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|s| s.split(';').next().map(str::trim).filter(|p| p.starts_with("oe_session=")))
        .expect("oe_session cookie expected")
        .to_string();
    let html_bytes = axum::body::to_bytes(report_resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let report_html = String::from_utf8_lossy(&html_bytes);
    let csrf_tok = extract_meta_csrf_token(&report_html).expect("oe-csrf-token expected");

    // Browser session WITH oe_session + X-OpenEstate-Token + Origin unlocks /api/commercial/config & /api/auth/otp/request
    let auth_otp = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/otp/request")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::ORIGIN, "http://127.0.0.1:8080")
                .header(header::COOKIE, &oe_cookie)
                .header("X-OpenEstate-Token", &csrf_tok)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"email":"buyer@example.com"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(auth_otp.status(), StatusCode::OK);

    // FAIL #3A: GET /api/report?slug=private MUST enforce authorize_report
    let denied_recs = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/report?slug={priv_agent_slug}"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::ORIGIN, "http://127.0.0.1:8080")
                .header(header::COOKIE, &oe_cookie)
                .header("X-OpenEstate-Token", &csrf_tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied_recs.status(), StatusCode::FORBIDDEN);

    let allowed_recs = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/report?slug={priv_agent_slug}"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::ORIGIN, "http://127.0.0.1:8080")
                .header(header::COOKIE, &oe_cookie)
                .header("X-OpenEstate-Token", &csrf_tok)
                .header("x-test-allow", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed_recs.status(), StatusCode::OK);

    // FAIL #3B: execute_extension_summary must enforce authorize_report even with refresh=true
    let denied_ext = ext_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/extension/summary?address={}&refresh=true",
                    urlencoding::encode(priv_ext_addr)
                ))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::ORIGIN, "http://127.0.0.1:8080")
                .header(header::COOKIE, &oe_cookie)
                .header("X-OpenEstate-Token", &csrf_tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied_ext.status(), StatusCode::FORBIDDEN);

    // GET /api/deal-room must prefer oe_session over forged open_estate_vid when internal_auth=true
    let deal_room_resp = ext_app
        .oneshot(
            Request::builder()
                .uri("/api/deal-room")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::ORIGIN, "http://127.0.0.1:8080")
                .header(header::COOKIE, format!("{oe_cookie}; open_estate_vid=forged_vid_123"))
                .header("X-OpenEstate-Token", &csrf_tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deal_room_resp.status(), StatusCode::OK);
    let dr_bytes = axum::body::to_bytes(deal_room_resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let dr_json: Value = serde_json::from_slice(&dr_bytes).unwrap();
    assert_eq!(
        dr_json["availableCount"].as_u64(),
        Some(0),
        "forged open_estate_vid must NOT override oe_session when internal auth is required: {dr_json}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
