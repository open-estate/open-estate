//! Integration tests for `/admin` generic reverse-proxy authentication
//! (`ADMIN_PROXY_AUTH_*` + `ADMIN_LOGOUT_URL`), `ADMIN_HOST` routing/isolation,
//! GCP-style Light/Dark UI + Sign out flow, User Management CRUD + inspection + CSV export,
//! and Subscription Management + frequency + criteria retranslation + delivery logs + CSV export.

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    routing::get,
    Router,
};
use open_estate::db::SqlParam;
use open_estate::search::CURRENT_FILTER_SCHEMA_VERSION;
use open_estate::shared::{now_iso, EnvConfig};
use open_estate::web::admin_console::{admin_host_redirect_layer, admin_routes};
use open_estate::web::server::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use tower::ServiceExt;
use uuid::Uuid;

async fn setup_admin_test_app(
    mut extra_env: HashMap<String, String>,
) -> (std::path::PathBuf, AppState, Router) {
    let dir = std::env::temp_dir().join(format!("oe-admin-auth-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.sqlite");
    extra_env.insert("SQLITE_DB_PATH".into(), db_path.to_string_lossy().into_owned());
    extra_env.insert("SQLITE_PATH".into(), db_path.to_string_lossy().into_owned());
    let env = EnvConfig::from_map(extra_env);
    let state = AppState::from_env(env).await.expect("AppState::from_env");
    state.db.ensure_schema().await.expect("ensure_schema");

    let app = Router::new()
        .route("/", get(|| async { (StatusCode::OK, "public-home") }))
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .merge(admin_routes())
        .with_state(state.clone())
        .layer(admin_host_redirect_layer(state.clone()));

    (dir, state, app)
}

async fn send_json_req(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body_opt: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let body = match body_opt {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 2 * 1024 * 1024).await.unwrap();
    let json_val = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    (status, resp_headers, json_val)
}

async fn send_raw_req(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 2 * 1024 * 1024).await.unwrap();
    (status, resp_headers, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn reverse_proxy_auth_secret_headers_allowlist_only_mode_and_logout() {
    let proxy_secret = "proxy-shared-secret-2026";
    let ops_token = "fallback-ops-secret-123";

    let mut env = HashMap::new();
    env.insert("ADMIN_PROXY_AUTH_ENABLED".into(), "true".into());
    env.insert("ADMIN_PROXY_AUTH_SECRET".into(), proxy_secret.into());
    env.insert(
        "ADMIN_ALLOWED_EMAILS".into(),
        "op@example.org,@trusted-ops.example".into(),
    );
    env.insert("ADMIN_LOGOUT_URL".into(), "/sso/logout".into());
    env.insert("OPEN_ESTATE_OPS_TOKEN".into(), ops_token.into());

    let (dir, state, app) = setup_admin_test_app(env).await;

    // 1. Missing proxy headers & Bearer token rejects with 401 PROXY_AUTH_REQUIRED (no loopback bypass)
    let (st_unauth, _, body_unauth) =
        send_json_req(&app, "GET", "/api/admin/overview", &[], None).await;
    assert_eq!(st_unauth, StatusCode::UNAUTHORIZED);
    assert_eq!(body_unauth["code"], "PROXY_AUTH_REQUIRED");
    assert_eq!(body_unauth["tokenConfigured"], true);

    // 2. Forged X-Auth-Request-Email header WITHOUT X-Proxy-Auth-Secret MUST be rejected!
    let (st_forged_no_sec, _, body_forged_no_sec) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[("X-Auth-Request-Email", "op@example.org")],
        None,
    )
    .await;
    assert_eq!(st_forged_no_sec, StatusCode::UNAUTHORIZED);
    assert_eq!(body_forged_no_sec["code"], "PROXY_AUTH_INVALID_SECRET");

    // 3. Forged X-Auth-Request-Email with WRONG X-Proxy-Auth-Secret MUST be rejected!
    let (st_forged_bad_sec, _, body_forged_bad_sec) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Auth-Request-Email", "op@example.org"),
            ("X-Proxy-Auth-Secret", "wrong-secret"),
        ],
        None,
    )
    .await;
    assert_eq!(st_forged_bad_sec, StatusCode::UNAUTHORIZED);
    assert_eq!(body_forged_bad_sec["code"], "PROXY_AUTH_INVALID_SECRET");

    // 4. Forged X-Auth-Request-Email even alongside Bearer token MUST NOT silently pass with forged email
    let bearer_hdr = format!("Bearer {ops_token}");
    let (st_forged_with_bearer, _, body_forged_with_bearer) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[
            ("Authorization", &bearer_hdr),
            ("X-Auth-Request-Email", "op@example.org"),
        ],
        None,
    )
    .await;
    assert_eq!(st_forged_with_bearer, StatusCode::UNAUTHORIZED);
    assert_eq!(body_forged_with_bearer["code"], "PROXY_AUTH_INVALID_SECRET");

    // 5. Valid secret WITHOUT user header MUST be rejected
    let (st_no_user, _, body_no_user) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[("X-Proxy-Auth-Secret", proxy_secret)],
        None,
    )
    .await;
    assert_eq!(st_no_user, StatusCode::UNAUTHORIZED);
    assert_eq!(body_no_user["code"], "PROXY_AUTH_MISSING_USER");

    // 6. Duplicate or comma-smuggled user header MUST be rejected
    let (st_dup, _, body_dup) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Auth-Request-Email", "op@example.org"),
            ("X-Auth-Request-Email", "attacker@evil.org"),
        ],
        None,
    )
    .await;
    assert_eq!(st_dup, StatusCode::UNAUTHORIZED);
    assert_eq!(body_dup["code"], "PROXY_AUTH_MISSING_USER");

    let (st_comma, _, body_comma) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Auth-Request-Email", "op@example.org,attacker@evil.org"),
        ],
        None,
    )
    .await;
    assert_eq!(st_comma, StatusCode::FORBIDDEN);
    assert_eq!(body_comma["code"], "PROXY_AUTH_MISSING_USER");

    // 7. Valid X-Proxy-Auth-Secret + exact allowlisted email (case-insensitive) succeeds
    let (st_ok, _, body_ok) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Auth-Request-Email", "Op@Example.ORG"),
        ],
        None,
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "expected valid proxy auth to pass: {body_ok}");
    assert_eq!(body_ok["operator"]["actor"], "proxy:op@example.org");
    assert_eq!(body_ok["operator"]["viaProxyAuth"], true);
    assert_eq!(body_ok["operator"]["viaToken"], false);
    assert_eq!(body_ok["operator"]["proxyAuthEnabled"], true);
    assert_eq!(body_ok["operator"]["logoutUrl"], "/sso/logout");

    // 8. Domain suffix allowlist match succeeds
    let (st_dom, _, body_dom) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Auth-Request-Email", "Alice@Trusted-Ops.EXAMPLE"),
        ],
        None,
    )
    .await;
    assert_eq!(st_dom, StatusCode::OK);
    assert_eq!(
        body_dom["operator"]["actor"],
        "proxy:alice@trusted-ops.example"
    );

    // 9. Verified proxy auth entries are recorded in admin_audit_log
    let audits = state.db.list_admin_audit_logs(25).await.unwrap();
    assert!(audits
        .iter()
        .any(|a| a.action == "admin.proxy.auth" && a.actor == "proxy:op@example.org"));
    assert!(audits
        .iter()
        .any(|a| a.action == "admin.proxy.auth" && a.actor == "proxy:alice@trusted-ops.example"));

    // 10. Valid secret but email outside ADMIN_ALLOWED_EMAILS rejected with 403
    let (st_denied, _, body_denied) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Auth-Request-Email", "stranger@other.org"),
        ],
        None,
    )
    .await;
    assert_eq!(st_denied, StatusCode::FORBIDDEN);
    assert_eq!(body_denied["code"], "PROXY_AUTH_EMAIL_DENIED");

    // 11. OPEN_ESTATE_OPS_TOKEN coexists cleanly when ADMIN_PROXY_AUTH_ONLY != true
    let (st_bearer, _, body_bearer) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[("Authorization", &bearer_hdr)],
        None,
    )
    .await;
    assert_eq!(st_bearer, StatusCode::OK);
    assert_eq!(body_bearer["operator"]["viaToken"], true);
    assert_eq!(body_bearer["operator"]["viaProxyAuth"], false);

    let (st_login, login_hdrs, body_login) = send_json_req(
        &app,
        "POST",
        "/api/admin/session",
        &[("Host", "localhost"), ("Origin", "http://localhost")],
        Some(json!({ "token": ops_token })),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK);
    assert_eq!(body_login["ok"], true);
    let session_cookie = login_hdrs
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .expect("session cookie")
        .to_string();
    let (st_cookie_ok, _, body_cookie_ok) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[("Cookie", &session_cookie)],
        None,
    )
    .await;
    assert_eq!(st_cookie_ok, StatusCode::OK);
    assert_eq!(body_cookie_ok["operator"]["viaToken"], true);

    // Gate HTML when token login coexists shows token form
    let (st_gate_tok, _, gate_tok_html) = send_raw_req(&app, "GET", "/admin", &[]).await;
    assert_eq!(st_gate_tok, StatusCode::UNAUTHORIZED);
    assert!(gate_tok_html.contains("style=\"display:block\""));

    // 12. Custom ADMIN_PROXY_USER_HEADER + ADMIN_PROXY_AUTH_ONLY=true (auto-enabled via secret)
    let mut env_only = HashMap::new();
    // Omit ADMIN_PROXY_AUTH_ENABLED to verify auto-enable when ADMIN_PROXY_AUTH_SECRET is configured
    env_only.insert("ADMIN_PROXY_AUTH_ONLY".into(), "true".into());
    env_only.insert("ADMIN_PROXY_USER_HEADER".into(), "X-Forwarded-User".into());
    env_only.insert("ADMIN_PROXY_AUTH_SECRET".into(), proxy_secret.into());
    env_only.insert("ADMIN_LOGOUT_URL".into(), "/sso/logout".into());
    env_only.insert("OPEN_ESTATE_OPS_TOKEN".into(), ops_token.into());
    let (dir_only, _, app_only) = setup_admin_test_app(env_only).await;

    let (st_login_only, _, body_login_only) = send_json_req(
        &app_only,
        "POST",
        "/api/admin/session",
        &[("Host", "localhost"), ("Origin", "http://localhost")],
        Some(json!({ "token": ops_token })),
    )
    .await;
    assert_eq!(st_login_only, StatusCode::FORBIDDEN);
    assert_eq!(body_login_only["code"], "PROXY_AUTH_ONLY");

    let (st_bearer_denied, _, body_bearer_denied) = send_json_req(
        &app_only,
        "GET",
        "/api/admin/overview",
        &[("Authorization", &bearer_hdr)],
        None,
    )
    .await;
    assert_eq!(st_bearer_denied, StatusCode::UNAUTHORIZED);
    assert_eq!(body_bearer_denied["code"], "PROXY_AUTH_REQUIRED");
    assert_eq!(body_bearer_denied["tokenConfigured"], false);

    // Default header x-auth-request-email must NOT satisfy custom X-Forwarded-User config
    let (st_wrong_hdr, _, body_wrong_hdr) = send_json_req(
        &app_only,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Auth-Request-Email", "op@example.org"),
        ],
        None,
    )
    .await;
    assert_eq!(st_wrong_hdr, StatusCode::UNAUTHORIZED);
    assert_eq!(body_wrong_hdr["code"], "PROXY_AUTH_MISSING_USER");

    // Custom header + secret passes
    let (st_custom_ok, _, body_custom_ok) = send_json_req(
        &app_only,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Forwarded-User", "ops-lead"),
        ],
        None,
    )
    .await;
    assert_eq!(st_custom_ok, StatusCode::OK);
    assert_eq!(body_custom_ok["operator"]["actor"], "proxy:ops-lead");
    assert_eq!(body_custom_ok["operator"]["proxyAuthOnly"], true);

    // 13. /admin HTML renders GCP-style rail, Light & Dark themes, generic badges, #signOutBtn & ADMIN_LOGOUT_URL
    let (st_gate_only, _, gate_only_html) = send_raw_req(&app_only, "GET", "/admin", &[]).await;
    assert_eq!(st_gate_only, StatusCode::UNAUTHORIZED);
    assert!(gate_only_html.contains("gate-card"));
    assert!(gate_only_html.contains("#1a73e8"));
    assert!(gate_only_html.contains("prefers-color-scheme: dark"));
    assert!(gate_only_html.contains("<form id=\"loginForm\" style=\"display:none\">"));

    let (st_html, _, admin_html) = send_raw_req(
        &app_only,
        "GET",
        "/admin",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Forwarded-User", "ops-lead"),
        ],
    )
    .await;
    assert_eq!(st_html, StatusCode::OK);
    assert!(admin_html.contains("id=\"signOutBtn\""), "Sign out button missing");
    assert!(admin_html.contains("Sign out"));
    assert!(admin_html.contains("console-shell") && admin_html.contains("nav-rail"));
    assert!(admin_html.contains("id=\"navRailToggleBtn\""));
    assert!(admin_html.contains("prefers-color-scheme: dark"));
    assert!(admin_html.contains("data-theme=\"light\"") && admin_html.contains("data-theme=\"dark\""));
    assert!(admin_html.contains("id=\"themeSelect\""));
    assert!(admin_html.contains("Proxy Auth:"));
    assert!(admin_html.contains("SSO Proxy") && admin_html.contains("Operator Token"));
    assert!(admin_html.contains("/sso/logout"));
    let html_lc = admin_html.to_ascii_lowercase();
    assert!(!html_lc.contains(concat!("cloud", "flare access")));
    assert!(!html_lc.contains(concat!("cf-", "access")));
    assert!(!html_lc.contains(concat!("cf_", "access")));
    assert!(!html_lc.contains(concat!("cdn-", "cgi")));

    // 14. DELETE /api/admin/session clears ops cookie, obeys CSRF, and returns ADMIN_LOGOUT_URL
    let (st_csrf, _, body_csrf) = send_json_req(
        &app_only,
        "DELETE",
        "/api/admin/session",
        &[
            ("Host", "admin.example.com"),
            ("Origin", "https://evil.example"),
        ],
        None,
    )
    .await;
    assert_eq!(st_csrf, StatusCode::FORBIDDEN);
    assert_eq!(body_csrf["code"], "CSRF_ORIGIN_MISMATCH");

    let (st_logout, logout_hdrs, body_logout) = send_json_req(
        &app_only,
        "DELETE",
        "/api/admin/session",
        &[
            ("Host", "localhost"),
            ("Origin", "http://localhost"),
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Forwarded-User", "ops-lead"),
            ("X-Forwarded-Proto", "https"),
        ],
        None,
    )
    .await;
    assert_eq!(st_logout, StatusCode::OK);
    assert_eq!(body_logout["ok"], true);
    assert_eq!(body_logout["proxyAuthEnabled"], true);
    assert_eq!(body_logout["logoutUrl"], "/sso/logout");
    let set_cookies: Vec<&str> = logout_hdrs
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert!(set_cookies
        .iter()
        .any(|c| c.contains("open_estate_ops_token=;") && c.contains("Secure")));

    // 15. Misconfigured ADMIN_PROXY_AUTH_ENABLED=true without ADMIN_PROXY_AUTH_SECRET fails closed
    let mut env_misconf = HashMap::new();
    env_misconf.insert("ADMIN_PROXY_AUTH_ENABLED".into(), "true".into());
    let (dir_misconf, _, app_misconf) = setup_admin_test_app(env_misconf).await;

    let (st_misconf_forged, _, body_misconf_forged) = send_json_req(
        &app_misconf,
        "GET",
        "/api/admin/overview",
        &[("X-Auth-Request-Email", "op@example.org")],
        None,
    )
    .await;
    assert_eq!(st_misconf_forged, StatusCode::UNAUTHORIZED);
    assert_eq!(body_misconf_forged["code"], "PROXY_AUTH_MISCONFIGURED");

    let (st_misconf_empty, _, body_misconf_empty) =
        send_json_req(&app_misconf, "GET", "/api/admin/overview", &[], None).await;
    assert_eq!(st_misconf_empty, StatusCode::UNAUTHORIZED);
    assert_eq!(body_misconf_empty["code"], "PROXY_AUTH_MISCONFIGURED");

    // 16. Explicit ADMIN_PROXY_AUTH_ENABLED=false disables proxy headers even if secret is set,
    //     and unset ADMIN_LOGOUT_URL returns empty logoutUrl on DELETE /api/admin/session
    let mut env_disabled = HashMap::new();
    env_disabled.insert("ADMIN_PROXY_AUTH_ENABLED".into(), "false".into());
    env_disabled.insert("ADMIN_PROXY_AUTH_SECRET".into(), proxy_secret.into());
    env_disabled.insert("ADMIN_TOKEN".into(), ops_token.into());
    let (dir_dis, _, app_dis) = setup_admin_test_app(env_disabled).await;

    let (st_dis_proxy, _, body_dis_proxy) = send_json_req(
        &app_dis,
        "GET",
        "/api/admin/overview",
        &[
            ("X-Proxy-Auth-Secret", proxy_secret),
            ("X-Auth-Request-Email", "op@example.org"),
        ],
        None,
    )
    .await;
    assert_eq!(st_dis_proxy, StatusCode::UNAUTHORIZED);
    assert_eq!(body_dis_proxy["code"], "ADMIN_AUTH_REQUIRED");

    let (st_dis_logout, _, body_dis_logout) = send_json_req(
        &app_dis,
        "DELETE",
        "/api/admin/session",
        &[("Host", "localhost"), ("Origin", "http://localhost")],
        None,
    )
    .await;
    assert_eq!(st_dis_logout, StatusCode::OK);
    assert_eq!(body_dis_logout["proxyAuthEnabled"], false);
    assert_eq!(body_dis_logout["logoutUrl"], "");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir_only);
    let _ = std::fs::remove_dir_all(&dir_misconf);
    let _ = std::fs::remove_dir_all(&dir_dis);
}

#[tokio::test]
async fn admin_host_routing_and_strict_isolation() {
    let token = "host-test-admin-secret";
    let mut env = HashMap::new();
    env.insert("OPEN_ESTATE_OPS_TOKEN".into(), token.into());
    env.insert("ADMIN_HOST".into(), "admin.example.com".into());
    env.insert("ADMIN_REQUIRE_ADMIN_HOST".into(), "false".into());

    let (dir, _, app) = setup_admin_test_app(env).await;
    let auth_hdr = format!("Bearer {token}");

    // On ADMIN_HOST, `/` redirects to `/admin` (preserving query string)
    let (st_root, hdrs_root, _) = send_raw_req(
        &app,
        "GET",
        "/?tab=users",
        &[("Host", "admin.example.com")],
    )
    .await;
    assert_eq!(st_root, StatusCode::FOUND);
    assert_eq!(
        hdrs_root.get(header::LOCATION).and_then(|v| v.to_str().ok()),
        Some("/admin?tab=users")
    );

    // On public host when ADMIN_REQUIRE_ADMIN_HOST=false, `/` and `/admin` serve normally
    let (st_pub_home, _, pub_body) =
        send_raw_req(&app, "GET", "/", &[("Host", "app.example.com")]).await;
    assert_eq!(st_pub_home, StatusCode::OK);
    assert_eq!(pub_body, "public-home");

    let (st_pub_adm, _, _) = send_json_req(
        &app,
        "GET",
        "/api/admin/overview",
        &[("Host", "app.example.com"), ("Authorization", &auth_hdr)],
        None,
    )
    .await;
    assert_eq!(st_pub_adm, StatusCode::OK);

    // With ADMIN_REQUIRE_ADMIN_HOST=true + TRUST_PROXY_HEADERS=true:
    // public `/admin` -> 302 to admin host, `/api/admin/*` -> 404, while X-Forwarded-Host: admin.example.com succeeds
    let mut env_strict = HashMap::new();
    env_strict.insert("OPEN_ESTATE_OPS_TOKEN".into(), token.into());
    env_strict.insert("ADMIN_HOST".into(), "admin.example.com".into());
    env_strict.insert("ADMIN_REQUIRE_ADMIN_HOST".into(), "true".into());
    env_strict.insert("TRUST_PROXY_HEADERS".into(), "true".into());
    let (dir_strict, _, app_strict) = setup_admin_test_app(env_strict).await;

    let (st_redir, hdrs_redir, _) = send_raw_req(
        &app_strict,
        "GET",
        "/admin?view=subs",
        &[("Host", "app.example.com")],
    )
    .await;
    assert_eq!(st_redir, StatusCode::FOUND);
    assert_eq!(
        hdrs_redir.get(header::LOCATION).and_then(|v| v.to_str().ok()),
        Some("https://admin.example.com/admin?view=subs")
    );

    let (st_api_404, _, body_404) = send_json_req(
        &app_strict,
        "GET",
        "/api/admin/overview",
        &[("Host", "app.example.com"), ("Authorization", &auth_hdr)],
        None,
    )
    .await;
    assert_eq!(st_api_404, StatusCode::NOT_FOUND);
    assert_eq!(body_404["code"], "ADMIN_HOST_REQUIRED");

    let (st_health, _, health_body) =
        send_raw_req(&app_strict, "GET", "/healthz", &[("Host", "app.example.com")]).await;
    assert_eq!(st_health, StatusCode::OK);
    assert_eq!(health_body, "ok");

    // Admin host works cleanly (both Host and TRUST_PROXY_HEADERS X-Forwarded-Host)
    let (st_admin_ok, _, _) = send_json_req(
        &app_strict,
        "GET",
        "/api/admin/overview",
        &[("Host", "admin.example.com"), ("Authorization", &auth_hdr)],
        None,
    )
    .await;
    assert_eq!(st_admin_ok, StatusCode::OK);

    let (st_xfh_root, hdrs_xfh, _) = send_raw_req(
        &app_strict,
        "GET",
        "/",
        &[("Host", "internal.svc.local:8080"), ("X-Forwarded-Host", "admin.example.com")],
    )
    .await;
    assert_eq!(st_xfh_root, StatusCode::FOUND);
    assert_eq!(
        hdrs_xfh.get(header::LOCATION).and_then(|v| v.to_str().ok()),
        Some("/admin")
    );

    // OPEN_ESTATE_TRUST_PROXY=false overrides TRUST_PROXY_HEADERS=true so spoofed X-Forwarded-Host is ignored
    let mut env_override = HashMap::new();
    env_override.insert("OPEN_ESTATE_OPS_TOKEN".into(), token.into());
    env_override.insert("ADMIN_HOST".into(), "admin.example.com".into());
    env_override.insert("ADMIN_REQUIRE_ADMIN_HOST".into(), "true".into());
    env_override.insert("TRUST_PROXY_HEADERS".into(), "true".into());
    env_override.insert("OPEN_ESTATE_TRUST_PROXY".into(), "false".into());
    let (dir_override, _, app_override) = setup_admin_test_app(env_override).await;

    let (st_spoof_xfh, _, body_spoof_xfh) = send_json_req(
        &app_override,
        "GET",
        "/api/admin/overview",
        &[
            ("Host", "app.example.com"),
            ("X-Forwarded-Host", "admin.example.com"),
            ("Authorization", &auth_hdr),
        ],
        None,
    )
    .await;
    assert_eq!(st_spoof_xfh, StatusCode::NOT_FOUND);
    assert_eq!(body_spoof_xfh["code"], "ADMIN_HOST_REQUIRED");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir_strict);
    let _ = std::fs::remove_dir_all(&dir_override);
}

#[tokio::test]
async fn admin_users_crud_filtering_inspection_deactivate_and_csv() {
    let token = "users-admin-secret";
    let mut env = HashMap::new();
    env.insert("OPEN_ESTATE_OPS_TOKEN".into(), token.into());
    let (dir, state, app) = setup_admin_test_app(env).await;
    let auth_hdr = format!("Bearer {token}");
    let hdrs = [("Authorization", auth_hdr.as_str())];

    // 1. Create two users (one with custom maxActiveNlAlerts override, one blocked)
    let (st1, _, u1_res) = send_json_req(
        &app,
        "POST",
        "/api/admin/users",
        &hdrs,
        Some(json!({
            "email": "vip-buyer@example.com",
            "displayName": "VIP Buyer",
            "role": "buyer",
            "planTier": "team_cre",
            "maxActiveNlAlerts": 99,
            "status": "active",
            "notes": "Enterprise buyer override"
        })),
    )
    .await;
    assert_eq!(st1, StatusCode::CREATED, "user1 create: {u1_res}");
    let u1_id = u1_res["user"]["userId"].as_str().unwrap().to_string();
    assert_eq!(u1_res["user"]["planTier"], "team_cre");
    assert_eq!(u1_res["user"]["maxActiveNlAlerts"], 99);
    assert_eq!(u1_res["user"]["effectiveMaxActiveNlAlerts"], 99);

    let (st2, _, u2_res) = send_json_req(
        &app,
        "POST",
        "/api/admin/users",
        &hdrs,
        Some(json!({
            "email": "blocked-spam@example.com",
            "displayName": "Abuse Account",
            "role": "member",
            "planTier": "explorer",
            "status": "blocked",
            "notes": "Policy hold"
        })),
    )
    .await;
    assert_eq!(st2, StatusCode::CREATED, "user2 create: {u2_res}");
    let u2_id = u2_res["user"]["userId"].as_str().unwrap().to_string();
    assert_eq!(u2_res["user"]["status"], "blocked");

    // 2. Seed subscription + notification + report search history + agent match request for u1
    let (_, _, sub_created) = send_json_req(
        &app,
        "POST",
        "/api/admin/subscriptions",
        &hdrs,
        Some(json!({
            "email": "vip-buyer@example.com",
            "zips": ["95014"],
            "requirements": "4+ bed under $2.8M",
            "frequency": "daily",
            "active": true
        })),
    )
    .await;
    let sub_id = sub_created["subscription"]["id"].as_str().unwrap().to_string();

    let now = now_iso();
    state
        .db
        .execute_params(
            "INSERT INTO user_search_history (actor_key, slug, raw_slug, display_address, updated_at) VALUES (?, ?, ?, ?, ?)",
            &[
                SqlParam::from(format!("user:{u1_id}").as_str()),
                SqlParam::from("100-synthadminalpha-ln-san-jose-ca-95118"),
                SqlParam::from("100 Synthadminalpha Ln, San Jose, CA 95118"),
                SqlParam::from("100 Synthadminalpha Ln, San Jose, CA 95118"),
                SqlParam::from(now.as_str()),
            ],
        )
        .await
        .unwrap();

    state
        .db
        .execute_params(
            "INSERT INTO agent_match_requests (id, slug, display_address, buyer_name, buyer_email, need_type, doc_needed, smtp_status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                SqlParam::from("amr-1"),
                SqlParam::from("100-synthadminalpha-ln-san-jose-ca-95118"),
                SqlParam::from("100 Synthadminalpha Ln, San Jose, CA 95118"),
                SqlParam::from("VIP Buyer"),
                SqlParam::from("vip-buyer@example.com"),
                SqlParam::from("tour"),
                SqlParam::from("disclosures"),
                SqlParam::from("sent"),
                SqlParam::from(now.as_str()),
            ],
        )
        .await
        .unwrap();

    state
        .db
        .execute_params(
            "INSERT INTO subscription_notifications (id, subscription_id, listing_id, event_key, email_to, subject, match_score, delivery_status, delivery_error, sent_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                SqlParam::from("notif-u1"),
                SqlParam::from(sub_id.as_str()),
                SqlParam::from("lst-100"),
                SqlParam::from("ev-notif-u1"),
                SqlParam::from("vip-buyer@example.com"),
                SqlParam::from("Daily Cupertino Match"),
                SqlParam::from(94i64),
                SqlParam::from("sent"),
                SqlParam::from(None::<&str>),
                SqlParam::from(now.as_str()),
            ],
        )
        .await
        .unwrap();

    // 3. List & filter users by plan_tier, role, status, search
    let (st_f1, _, f1) = send_json_req(
        &app,
        "GET",
        "/api/admin/users?plan_tier=team_cre&role=buyer&status=active&search=vip",
        &hdrs,
        None,
    )
    .await;
    assert_eq!(st_f1, StatusCode::OK);
    assert_eq!(f1["total"], 1);
    assert_eq!(f1["items"][0]["activeSubscriptions"], 1);
    assert_eq!(f1["items"][0]["pausedSubscriptions"], 0);

    let (_, _, f_blocked) =
        send_json_req(&app, "GET", "/api/admin/users?status=blocked", &hdrs, None).await;
    assert_eq!(f_blocked["total"], 1);
    assert_eq!(f_blocked["items"][0]["userId"], u2_id);

    // 4. Inspect user detail (subscriptions, reportActivity, notificationsRecent)
    let (st_insp, _, insp) = send_json_req(
        &app,
        "GET",
        &format!("/api/admin/users/{u1_id}"),
        &hdrs,
        None,
    )
    .await;
    assert_eq!(st_insp, StatusCode::OK, "inspect response: {insp}");
    assert_eq!(insp["subscriptionCounts"]["active"], 1);
    assert_eq!(insp["reportActivity"]["viewedReportsCount"], 1);
    assert_eq!(insp["reportActivity"]["agentMatchRequests"].as_array().unwrap().len(), 1);
    assert_eq!(insp["notificationsRecent"].as_array().unwrap().len(), 1);

    // 5. Patch user: clear maxActiveNlAlerts override -> falls back to plan tier cap (pro_monthly=25)
    let (st_patch, _, patched) = send_json_req(
        &app,
        "PATCH",
        &format!("/api/admin/users/{u1_id}"),
        &hdrs,
        Some(json!({
            "planTier": "pro_monthly",
            "maxActiveNlAlerts": null,
            "role": "agent"
        })),
    )
    .await;
    assert_eq!(st_patch, StatusCode::OK);
    assert_eq!(patched["user"]["planTier"], "pro_monthly");
    assert_eq!(patched["user"]["maxActiveNlAlerts"], Value::Null);
    assert_eq!(patched["user"]["effectiveMaxActiveNlAlerts"], 25);

    // 6. Export CSV before deactivation
    let (st_csv, csv_hdrs, csv_body) =
        send_raw_req(&app, "GET", "/api/admin/users/export.csv?plan_tier=pro_monthly", &hdrs).await;
    assert_eq!(st_csv, StatusCode::OK);
    assert!(csv_hdrs
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("text/csv"));
    assert!(csv_body.contains("user_id,email,display_name"));
    assert!(csv_body.contains("vip-buyer@example.com"));
    assert!(!csv_body.contains("blocked-spam@example.com"));

    // 7. Deactivate user & pause their subscriptions
    let (st_deact, _, deact_res) = send_json_req(
        &app,
        "POST",
        &format!("/api/admin/users/{u1_id}/deactivate"),
        &hdrs,
        Some(json!({ "status": "suspended", "pauseSubscriptions": true })),
    )
    .await;
    assert_eq!(st_deact, StatusCode::OK);
    assert_eq!(deact_res["pausedSubscriptions"], 1);

    let sub_after = state.db.fetch_subscription_by_id(&sub_id).await.unwrap().unwrap();
    assert_eq!(sub_after.active, 0, "deactivation should pause user's active alert");

    // 8. Hard delete u2 and confirm 404 on subsequent GET
    let (st_del, _, del_res) =
        send_json_req(&app, "DELETE", &format!("/api/admin/users/{u2_id}"), &hdrs, None).await;
    assert_eq!(st_del, StatusCode::OK);
    assert_eq!(del_res["mode"], "deleted");

    let (st_gone, _, _) =
        send_json_req(&app, "GET", &format!("/api/admin/users/{u2_id}"), &hdrs, None).await;
    assert_eq!(st_gone, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn admin_subscriptions_crud_frequency_retranslate_delivery_logs_and_csv() {
    let token = "subs-admin-secret";
    let mut env = HashMap::new();
    env.insert("OPEN_ESTATE_OPS_TOKEN".into(), token.into());
    let (dir, state, app) = setup_admin_test_app(env).await;
    let auth_hdr = format!("Bearer {token}");
    let hdrs = [("Authorization", auth_hdr.as_str())];

    // 1. Create two subscriptions with distinct frequencies & ZIPs
    let (st_c1, _, c1) = send_json_req(
        &app,
        "POST",
        "/api/admin/subscriptions",
        &hdrs,
        Some(json!({
            "email": "daily-hunter@example.com",
            "name": "Cupertino Digest",
            "zips": ["95014"],
            "requirements": "Single family home 4+ beds under $3M",
            "frequency": "daily",
            "notifyOnNew": true,
            "notifyOnSold": false,
            "active": true
        })),
    )
    .await;
    assert_eq!(st_c1, StatusCode::CREATED, "sub1 create: {c1}");
    let sub1_id = c1["subscription"]["id"].as_str().unwrap().to_string();
    assert_eq!(c1["subscription"]["frequency"], "daily");

    let (st_c2, _, c2) = send_json_req(
        &app,
        "POST",
        "/api/admin/subscriptions",
        &hdrs,
        Some(json!({
            "email": "weekly-watch@example.com",
            "name": "Sunnyvale Weekly",
            "zips": ["94087"],
            "requirements": "Townhouse 3+ beds with garage under $1.9M",
            "frequency": "weekly",
            "notifyOnNew": true,
            "notifyOnSold": true,
            "active": false
        })),
    )
    .await;
    assert_eq!(st_c2, StatusCode::CREATED, "sub2 create: {c2}");
    let sub2_id = c2["subscription"]["id"].as_str().unwrap().to_string();
    assert_eq!(c2["subscription"]["active"], false);
    assert_eq!(c2["subscription"]["frequency"], "weekly");

    // 2. Seed delivery notifications for sub1 (one sent, one failed)
    let now = now_iso();
    state
        .db
        .execute_params(
            "INSERT INTO subscription_notifications (id, subscription_id, listing_id, event_key, email_to, subject, match_score, delivery_status, delivery_error, sent_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                SqlParam::from("dn-1"),
                SqlParam::from(sub1_id.as_str()),
                SqlParam::from("lst-1"),
                SqlParam::from("ev-dn-1"),
                SqlParam::from("daily-hunter@example.com"),
                SqlParam::from("New Match: SynthListing-01"),
                SqlParam::from(92i64),
                SqlParam::from("sent"),
                SqlParam::from(None::<&str>),
                SqlParam::from("2026-11-15T06:00:00Z"),
            ],
        )
        .await
        .unwrap();
    state
        .db
        .execute_params(
            "INSERT INTO subscription_notifications (id, subscription_id, listing_id, event_key, email_to, subject, match_score, delivery_status, delivery_error, sent_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                SqlParam::from("dn-2"),
                SqlParam::from(sub1_id.as_str()),
                SqlParam::from("lst-2"),
                SqlParam::from("ev-dn-2"),
                SqlParam::from("daily-hunter@example.com"),
                SqlParam::from("New Match: SynthListing-02"),
                SqlParam::from(88i64),
                SqlParam::from("failed"),
                SqlParam::from("SMTP relay 421 temporary deferral"),
                SqlParam::from(now.as_str()),
            ],
        )
        .await
        .unwrap();

    // 3. Filter subscriptions by frequency, status, zip, and email
    let (st_l1, _, l1) = send_json_req(
        &app,
        "GET",
        "/api/admin/subscriptions?frequency=daily&zip=95014&status=active&email=daily-hunter@example.com",
        &hdrs,
        None,
    )
    .await;
    assert_eq!(st_l1, StatusCode::OK);
    assert_eq!(l1["total"], 1);
    assert_eq!(l1["items"][0]["id"], sub1_id);
    assert_eq!(l1["items"][0]["notificationsCount"], 2);
    assert_eq!(l1["items"][0]["lastDeliveryStatus"], "failed");

    let (_, _, l_paused) = send_json_req(
        &app,
        "GET",
        "/api/admin/subscriptions?status=paused&frequency=weekly",
        &hdrs,
        None,
    )
    .await;
    assert_eq!(l_paused["total"], 1);
    assert_eq!(l_paused["items"][0]["id"], sub2_id);

    // 4. Delivery Log endpoint (`/api/admin/subscriptions/:id/notifications`)
    let (st_logs, _, logs_res) = send_json_req(
        &app,
        "GET",
        &format!("/api/admin/subscriptions/{sub1_id}/notifications"),
        &hdrs,
        None,
    )
    .await;
    assert_eq!(st_logs, StatusCode::OK);
    assert_eq!(logs_res["total"], 2);
    let items = logs_res["items"].as_array().unwrap();
    assert!(items
        .iter()
        .any(|n| n["deliveryStatus"] == "failed" && n["deliveryError"] == "SMTP relay 421 temporary deferral"));

    // 5. Edit sub2: change ZIPs, requirements, frequency, notify flags, resume active
    let (st_ed, _, ed_res) = send_json_req(
        &app,
        "PATCH",
        &format!("/api/admin/subscriptions/{sub2_id}"),
        &hdrs,
        Some(json!({
            "zips": ["94087", "95051"],
            "requirements": "Detached 4+ bed under $2.4M with big yard",
            "frequency": "instant",
            "notifyOnNew": false,
            "notifyOnSold": true,
            "active": true
        })),
    )
    .await;
    assert_eq!(st_ed, StatusCode::OK, "sub2 patch: {ed_res}");
    assert_eq!(ed_res["subscription"]["frequency"], "instant");
    assert_eq!(ed_res["subscription"]["active"], true);
    assert_eq!(ed_res["subscription"]["notifyOnNew"], false);
    assert_eq!(ed_res["subscription"]["notifyOnSold"], true);

    // 6. Force both subscriptions to stale filter_schema_version = 0 and test single + batch retranslate
    state
        .db
        .execute_params("UPDATE subscriptions SET filter_schema_version = 0", &[])
        .await
        .unwrap();

    let (st_rt1, _, rt1_res) = send_json_req(
        &app,
        "POST",
        &format!("/api/admin/subscriptions/{sub1_id}/retranslate"),
        &hdrs,
        Some(json!({})),
    )
    .await;
    assert_eq!(st_rt1, StatusCode::OK, "single retranslate: {rt1_res}");
    assert_eq!(
        rt1_res["subscription"]["filterSchemaVersion"],
        CURRENT_FILTER_SCHEMA_VERSION
    );

    let (st_rtb, _, rtb_res) = send_json_req(
        &app,
        "POST",
        "/api/admin/subscriptions/retranslate-stale",
        &hdrs,
        Some(json!({ "maxBatch": 50 })),
    )
    .await;
    assert_eq!(st_rtb, StatusCode::OK, "batch stale retranslate: {rtb_res}");
    assert!(rtb_res["summary"]["retranslated"].as_u64().unwrap_or(0) >= 1);

    let sub2_after = state.db.fetch_subscription_by_id(&sub2_id).await.unwrap().unwrap();
    assert_eq!(sub2_after.filter_schema_version, CURRENT_FILTER_SCHEMA_VERSION as i64);

    // 7. Export subscriptions CSV
    let (st_csv, _, csv_body) = send_raw_req(
        &app,
        "GET",
        "/api/admin/subscriptions/export.csv?zip=95014",
        &hdrs,
    )
    .await;
    assert_eq!(st_csv, StatusCode::OK);
    assert!(csv_body.contains("id,email,name,zips,requirements_text"));
    assert!(csv_body.contains("daily-hunter@example.com"));
    assert!(!csv_body.contains("weekly-watch@example.com"));

    // 8. Delete sub2 and verify watched_zips count updates cleanly
    let (st_del, _, del_res) = send_json_req(
        &app,
        "DELETE",
        &format!("/api/admin/subscriptions/{sub2_id}"),
        &hdrs,
        None,
    )
    .await;
    assert_eq!(st_del, StatusCode::OK);
    assert_eq!(del_res["deleted"], true);

    let wz = state.db.list_admin_watched_zips(50).await.unwrap();
    let wz_95051 = wz.iter().find(|z| z.zip == "95051");
    if let Some(row) = wz_95051 {
        assert_eq!(row.subscriber_count, 0);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
