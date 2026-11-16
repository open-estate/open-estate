//! Signed browser session cookie (`oe_session`) & unified internal API-key / session guard.
//!
//! Activated when `OPEN_ESTATE_REQUIRE_INTERNAL_AUTH=true` OR when non-empty
//! `OPEN_ESTATE_INTERNAL_API_KEY` / `OPEN_ESTATE_API_KEY` is configured.
//! Zero external dependencies beyond `sha2`, `hex`, and `uuid` already in `Cargo.toml`.

use crate::guardrails::{
    constant_time_eq, extract_browser_client_id, is_automated_scraper_ua,
    verify_same_origin_no_ct, GuardrailError,
};
use crate::shared::{now_unix_secs, EnvConfig};
use crate::web::http_util::{guardrail_error_to_response, json_response, req_host_str};
use crate::web::server::AppState;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use uuid::Uuid;

pub const OE_SESSION_COOKIE_NAME: &str = "oe_session";
pub const OE_SESSION_TTL_SECS: i64 = 86_400;

static EPHEMERAL_SESSION_SECRET: OnceLock<[u8; 32]> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct VerifiedOeSession {
    pub sid: String,
    pub iat: i64,
    pub csrf_token: String,
}

fn parse_bool_flag(env: &EnvConfig, key: &str, default: bool) -> bool {
    match env.get(key).map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => true,
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => false,
        _ => default,
    }
}

fn non_empty_env<'a>(env: &'a EnvConfig, key: &str) -> Option<&'a str> {
    env.get(key).map(str::trim).filter(|s| !s.is_empty())
}

/// Returns `true` when `OPEN_ESTATE_REQUIRE_INTERNAL_AUTH=true` OR either
/// `OPEN_ESTATE_INTERNAL_API_KEY` or `OPEN_ESTATE_API_KEY` is non-empty.
pub fn internal_auth_required(env: &EnvConfig) -> bool {
    parse_bool_flag(env, "OPEN_ESTATE_REQUIRE_INTERNAL_AUTH", false)
        || non_empty_env(env, "OPEN_ESTATE_INTERNAL_API_KEY").is_some()
        || non_empty_env(env, "OPEN_ESTATE_API_KEY").is_some()
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)?;
    if auth.len() > 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
        let tok = auth[7..].trim();
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    None
}

fn any_header_matches_secret(headers: &HeaderMap, header_names: &[&str], secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    for name in header_names {
        if let Some(cand) = headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if constant_time_eq(cand.as_bytes(), secret.as_bytes()) {
                return true;
            }
        }
    }
    if let Some(bearer) = extract_bearer_token(headers) {
        if constant_time_eq(bearer.as_bytes(), secret.as_bytes()) {
            return true;
        }
    }
    false
}

/// Validates `OPEN_ESTATE_INTERNAL_API_KEY` or `OPEN_ESTATE_API_KEY` via
/// `Authorization: Bearer`, `X-OpenEstate-Internal-Key`, or `X-OpenEstate-API-Key`.
pub fn has_valid_internal_api_key(headers: &HeaderMap, env: &EnvConfig) -> bool {
    let custom_hdrs = ["x-openestate-internal-key", "x-openestate-api-key"];
    for env_key in ["OPEN_ESTATE_INTERNAL_API_KEY", "OPEN_ESTATE_API_KEY"] {
        if let Some(secret) = non_empty_env(env, env_key) {
            if any_header_matches_secret(headers, &custom_hdrs, secret) {
                return true;
            }
        }
    }
    false
}

/// Validates `CRON_SECRET` (`Bearer`), `OPEN_ESTATE_INTERNAL_API_KEY`, or `OPEN_ESTATE_API_KEY`.
pub fn has_valid_cron_or_internal_key(headers: &HeaderMap, env: &EnvConfig) -> bool {
    if has_valid_internal_api_key(headers, env) {
        return true;
    }
    if let Some(cron_secret) = non_empty_env(env, "CRON_SECRET") {
        if let Some(bearer) = extract_bearer_token(headers) {
            if constant_time_eq(bearer.as_bytes(), cron_secret.as_bytes()) {
                return true;
            }
        }
    }
    false
}

/// Validates `OPEN_ESTATE_BATCH_TOKEN`, `ADMIN_API_TOKEN`, `OPEN_ESTATE_INTERNAL_API_KEY`, or
/// `OPEN_ESTATE_API_KEY`.
pub fn has_valid_batch_or_internal_key(headers: &HeaderMap, env: &EnvConfig) -> bool {
    if has_valid_internal_api_key(headers, env) {
        return true;
    }
    for env_key in ["OPEN_ESTATE_BATCH_TOKEN", "ADMIN_API_TOKEN"] {
        if let Some(secret) = non_empty_env(env, env_key) {
            if any_header_matches_secret(headers, &["x-openestate-batch-token"], secret) {
                return true;
            }
        }
    }
    false
}

/// Returns `true` when explicit server-configured API/ops/cron/batch/internal key auth succeeded.
pub fn has_valid_configured_key_auth(headers: &HeaderMap, env: &EnvConfig) -> bool {
    if has_valid_internal_api_key(headers, env)
        || has_valid_cron_or_internal_key(headers, env)
        || has_valid_batch_or_internal_key(headers, env)
    {
        return true;
    }
    let admin_hdrs = ["x-openestate-ops-token", "x-openestate-admin-token"];
    for env_key in [
        "OPEN_ESTATE_OPS_TOKEN",
        "OPEN_ESTATE_ADMIN_TOKEN",
        "ADMIN_TOKEN",
    ] {
        if let Some(secret) = non_empty_env(env, env_key) {
            if any_header_matches_secret(headers, &admin_hdrs, secret) {
                return true;
            }
        }
    }
    false
}

fn session_signing_secret(env: &EnvConfig) -> Vec<u8> {
    for key in [
        "OPEN_ESTATE_SESSION_SECRET",
        "OPEN_ESTATE_INTERNAL_API_KEY",
        "OPEN_ESTATE_API_KEY",
        "SESSION_SECRET",
    ] {
        if let Some(s) = non_empty_env(env, key) {
            return s.as_bytes().to_vec();
        }
    }
    let eph = EPHEMERAL_SESSION_SECRET.get_or_init(|| {
        let mut out = [0u8; 32];
        out[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        out[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        out
    });
    eph.to_vec()
}

/// Standard RFC 2104 HMAC-SHA256 using `sha2::Sha256`.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k_pad = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = Sha256::digest(key);
        k_pad[..32].copy_from_slice(&hashed);
    } else {
        k_pad[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k_pad[i];
        opad[i] ^= k_pad[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn csrf_token_for(secret: &[u8], sid: &str, iat: i64) -> String {
    hex::encode(hmac_sha256(secret, format!("csrf:v1.{sid}.{iat}").as_bytes()))
}

fn sign_oe_session_cookie(secret: &[u8], sid: &str, iat: i64) -> String {
    let payload = format!("v1.{sid}.{iat}");
    let sig = hex::encode(hmac_sha256(secret, payload.as_bytes()));
    format!("{payload}.{sig}")
}

fn extract_cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())?;
    let prefix = format!("{name}=");
    for part in raw.split(';').map(str::trim) {
        if let Some(val) = part.strip_prefix(&prefix) {
            let clean = val.trim().trim_matches('"');
            if !clean.is_empty() {
                return Some(clean.to_string());
            }
        }
    }
    None
}

/// Validates an incoming `oe_session` cookie (`v1.<sid>.<iat>.<sig>`).
pub fn resolve_verified_oe_session(
    env: &EnvConfig,
    headers: &HeaderMap,
) -> Option<VerifiedOeSession> {
    let raw = extract_cookie_value(headers, OE_SESSION_COOKIE_NAME)?;
    let mut parts = raw.split('.');
    let ver = parts.next()?;
    let sid = parts.next()?;
    let iat_str = parts.next()?;
    let sig_hex = parts.next()?;
    if parts.next().is_some() || ver != "v1" {
        return None;
    }
    if !(16..=64).contains(&sid.len())
        || !sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    if sig_hex.len() != 64 || !sig_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let iat: i64 = iat_str.parse().ok()?;
    let now = now_unix_secs();
    if iat > now + 300 || now - iat > OE_SESSION_TTL_SECS {
        return None;
    }

    let secret = session_signing_secret(env);
    let expected_sig = hex::encode(hmac_sha256(&secret, format!("v1.{sid}.{iat}").as_bytes()));
    let sig_norm = sig_hex.to_ascii_lowercase();
    if !constant_time_eq(sig_norm.as_bytes(), expected_sig.as_bytes()) {
        return None;
    }

    Some(VerifiedOeSession {
        sid: sid.to_string(),
        iat,
        csrf_token: csrf_token_for(&secret, sid, iat),
    })
}

pub fn is_https_request(env: &EnvConfig, headers: &HeaderMap) -> bool {
    if parse_bool_flag(env, "SESSION_COOKIE_SECURE", false) {
        return true;
    }
    if headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .is_some_and(|p| p.eq_ignore_ascii_case("https"))
    {
        return true;
    }
    env.get("PUBLIC_BASE_URL")
        .map(str::trim)
        .is_some_and(|u| u.to_ascii_lowercase().starts_with("https://"))
}

const OE_CSRF_FETCH_WRAPPER_JS: &str = r#"(()=>{try{var m=document.querySelector('meta[name="oe-csrf-token"]');var t=m?m.getAttribute('content'):'';if(!t)return;var needs=function(u){try{var raw=typeof u==='string'?u:(u&&typeof u==='object'&&'url'in u?u.url:'');var p=new URL(raw,location.href);return p.origin===location.origin&&(p.pathname.indexOf('/api/')===0||/^\/r\/[^\/]+\/report\.json$/i.test(p.pathname));}catch(e){return false;}};var origFetch=window.fetch;if(typeof origFetch==='function'){window.fetch=function(input,init){var url=(input&&typeof input==='object'&&'url'in input)?input.url:input;if(!needs(url))return origFetch.apply(this,arguments);var opts=Object.assign({},init||{});if(!opts.credentials)opts.credentials='same-origin';var h=new Headers(opts.headers||(typeof Request!=='undefined'&&input instanceof Request?input.headers:undefined));if(!h.has('X-OpenEstate-Token'))h.set('X-OpenEstate-Token',t);opts.headers=h;return origFetch.call(this,input,opts);};}var XHR=window.XMLHttpRequest;if(XHR&&XHR.prototype){var oOpen=XHR.prototype.open;var oSend=XHR.prototype.send;XHR.prototype.open=function(method,url){this.__oeNeeds=needs(url);return oOpen.apply(this,arguments);};XHR.prototype.send=function(){if(this.__oeNeeds){try{this.setRequestHeader('X-OpenEstate-Token',t);}catch(e){}}return oSend.apply(this,arguments);};}}catch(e){}})();"#;

fn inject_csrf_meta_and_wrapper(html: String, csrf_token: &str) -> String {
    let escaped_tok = html_escape::encode_double_quoted_attribute(csrf_token);
    let snippet = format!(
        r#"<meta name="oe-csrf-token" content="{escaped_tok}"><script>{OE_CSRF_FETCH_WRAPPER_JS}</script>"#
    );
    let lower = html.to_ascii_lowercase();
    if let Some(pos) = lower.rfind("</head>") {
        let mut out = String::with_capacity(html.len() + snippet.len());
        out.push_str(&html[..pos]);
        out.push_str(&snippet);
        out.push_str(&html[pos..]);
        out
    } else if let Some(pos) = lower.find("<body") {
        let mut out = String::with_capacity(html.len() + snippet.len());
        out.push_str(&html[..pos]);
        out.push_str(&snippet);
        out.push_str(&html[pos..]);
        out
    } else {
        format!("{snippet}{html}")
    }
}

/// Ensures the HTML response carries `<meta name="oe-csrf-token">` + fetch/XHR wrapper and returns
/// an optional `Set-Cookie: oe_session=...` header value if the request lacked a valid session.
pub fn prepare_oe_session_for_html(
    env: &EnvConfig,
    req_headers: &HeaderMap,
    html: String,
) -> (String, Option<HeaderValue>) {
    if let Some(existing) = resolve_verified_oe_session(env, req_headers) {
        return (inject_csrf_meta_and_wrapper(html, &existing.csrf_token), None);
    }

    let secret = session_signing_secret(env);
    let sid = Uuid::new_v4().simple().to_string();
    let iat = now_unix_secs();
    let cookie_val = sign_oe_session_cookie(&secret, &sid, iat);
    let csrf_tok = csrf_token_for(&secret, &sid, iat);
    let secure_attr = if is_https_request(env, req_headers) {
        "; Secure"
    } else {
        ""
    };
    let set_cookie_str = format!(
        "{OE_SESSION_COOKIE_NAME}={cookie_val}; Path=/; HttpOnly; SameSite=Lax; Max-Age={OE_SESSION_TTL_SECS}{secure_attr}"
    );
    let set_cookie = HeaderValue::from_str(&set_cookie_str).ok();
    (inject_csrf_meta_and_wrapper(html, &csrf_tok), set_cookie)
}

pub fn attach_optional_set_cookie(resp: &mut Response, set_cookie: Option<HeaderValue>) {
    if let Some(hv) = set_cookie {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
}

fn is_subscription_id_token_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/subscriptions/") else {
        return false;
    };
    if rest.is_empty() || rest == "unsubscribe" || rest == "manage-link" {
        return false;
    }
    let id_part = rest.strip_suffix("/matches").unwrap_or(rest);
    !id_part.is_empty() && !id_part.contains('/')
}

fn has_non_empty_query_param(raw_query: &str, keys: &[&str]) -> bool {
    if raw_query.trim().is_empty() {
        return false;
    }
    for pair in raw_query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if keys.iter().any(|cand| k.eq_ignore_ascii_case(cand)) && !v.trim().is_empty() {
            return true;
        }
    }
    false
}

fn is_exempt_from_internal_auth(method: &Method, path: &str, headers: &HeaderMap, raw_query: &str) -> bool {
    if path == "/healthz" || path == "/readyz" {
        return true;
    }
    if (*method == Method::GET || *method == Method::HEAD) && path == "/api/health" {
        return true;
    }
    if (*method == Method::GET || *method == Method::HEAD) && path.starts_with("/api/i18n/") {
        return true;
    }
    if *method == Method::POST && path == "/api/billing/webhook" {
        return true;
    }
    if (*method == Method::GET || *method == Method::HEAD)
        && (path == "/auth/verify" || path == "/api/auth/verify")
    {
        return true;
    }
    if path == "/api/subscriptions/unsubscribe" {
        return true;
    }
    if is_subscription_id_token_path(path) {
        let has_sub_token = has_non_empty_query_param(raw_query, &["token", "manage"])
            || headers
                .get("x-subscription-token")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .is_some_and(|s| !s.is_empty())
            || headers
                .get("x-manage-token")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .is_some_and(|s| !s.is_empty());
        if has_sub_token {
            return true;
        }
    }
    if path == "/admin"
        || path.starts_with("/admin/")
        || path == "/api/admin"
        || path.starts_with("/api/admin/")
        || path == "/ops"
        || path.starts_with("/ops/")
        || path == "/api/ops"
        || path.starts_with("/api/ops/")
    {
        return true;
    }
    false
}

fn extract_report_slug_for_share_check(path: &str, raw_query: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix("/r/") {
        if let Some(slug) = rest.strip_suffix("/report.json") {
            let clean = slug.trim();
            if !clean.is_empty() && !clean.contains('/') {
                return Some(clean.to_string());
            }
        }
    }
    for prefix in [
        "/api/house3d/",
        "/api/media/",
        "/api/buyer-diligence/",
        "/api/report/",
    ] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if let Some(seg) = rest.split('/').next().map(str::trim).filter(|s| !s.is_empty()) {
                return Some(seg.to_string());
            }
        }
    }
    if path == "/api/report" || path == "/api/extension/summary" {
        for pair in raw_query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k.eq_ignore_ascii_case("slug") && !v.trim().is_empty() {
                return urlencoding::decode(v)
                    .map(|c| c.into_owned())
                    .ok()
                    .or_else(|| Some(v.trim().to_string()));
            }
        }
    }
    None
}

fn is_cron_capable_path(method: &Method, path: &str) -> bool {
    if *method == Method::POST {
        if matches!(path, "/api/sync" | "/api/listings/ingest") {
            return true;
        }
        if path.starts_with("/api/property/") && path.ends_with("/platform-signals") {
            return true;
        }
    }
    if (*method == Method::GET || *method == Method::HEAD) && path == "/api/listings/shop" {
        return true;
    }
    false
}

fn is_batch_scan_path(path: &str) -> bool {
    path == "/api/scan/metrics" || path == "/api/scan/batch" || path.starts_with("/api/scan/batch/")
}

fn unauthorized_internal_auth_response() -> Response {
    let mut resp = json_response(
        StatusCode::UNAUTHORIZED,
        json!({
            "ok": false,
            "code": "INTERNAL_AUTH_REQUIRED",
            "error": "Authentication or valid session token is required.",
        }),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    resp
}

/// Checks whether the incoming request holds valid internal authentication credentials
/// (internal API key, authenticated admin/proxy user, Cloud SaaS user, valid report share token,
/// or signed `oe_session` cookie + CSRF/same-origin). Does NOT enforce itself unless called.
pub async fn has_valid_internal_credential_for_report_page(
    state: &AppState,
    headers: &HeaderMap,
) -> bool {
    if has_valid_internal_api_key(headers, &state.env) {
        return true;
    }
    if let Some(sess) = resolve_verified_oe_session(&state.env, headers) {
        let req_host = req_host_str(headers);
        let csrf_ok = headers
            .get("x-openestate-token")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .is_some_and(|tok| constant_time_eq(tok.as_bytes(), sess.csrf_token.as_bytes()));
        let sec_same = headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .is_some_and(|s| s.eq_ignore_ascii_case("same-origin"));
        let has_origin_or_ref = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
            || headers
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .is_some_and(|s| !s.is_empty());
        if (csrf_ok || sec_same || has_origin_or_ref)
            && verify_same_origin_no_ct(headers, req_host, &state.env).is_ok()
        {
            return true;
        }
    }
    state
        .hooks
        .resolve_authenticated_actor(state, headers)
        .await
        .is_some()
}

/// Unified Axum middleware enforcing `OPEN_ESTATE_REQUIRE_INTERNAL_AUTH` across `/api/*` and
/// `/r/:slug/report.json`.
pub async fn internal_auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if !internal_auth_required(&state.env) {
        return next.run(req).await;
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let raw_query = req.uri().query().unwrap_or("").to_string();

    let is_protected = path == "/api"
        || path.starts_with("/api/")
        || (path.starts_with("/r/") && path.ends_with("/report.json"));
    if !is_protected {
        return next.run(req).await;
    }

    let headers = req.headers();
    if is_exempt_from_internal_auth(&method, &path, headers, &raw_query) {
        return next.run(req).await;
    }

    if is_automated_scraper_ua(headers)
        && !has_valid_internal_api_key(headers, &state.env)
        && !(is_cron_capable_path(&method, &path)
            && has_valid_cron_or_internal_key(headers, &state.env))
        && !(is_batch_scan_path(&path) && has_valid_batch_or_internal_key(headers, &state.env))
    {
        return guardrail_error_to_response(GuardrailError::new(
            "AUTOMATED_SCRAPER_BLOCKED",
            format!(
                "Automated scripts and scrapers are prohibited on {} AI endpoints.",
                state.env.brand_name()
            ),
            403,
        ));
    }

    // Credential path (a1): Internal/API key or route-scoped cron/batch key
    if has_valid_internal_api_key(headers, &state.env)
        || (is_cron_capable_path(&method, &path)
            && has_valid_cron_or_internal_key(headers, &state.env))
        || (is_batch_scan_path(&path) && has_valid_batch_or_internal_key(headers, &state.env))
    {
        return next.run(req).await;
    }

    // Credential path (a2): Explicit non-loopback admin credentials
    if let Ok(admin_ctx) =
        crate::web::admin_console::verify_admin_access(&state, headers, req.extensions()).await
    {
        if admin_ctx.via_token || admin_ctx.actor.starts_with("proxy:") {
            return next.run(req).await;
        }
    }

    // Credential path (a3): Valid ?share_token= on report routes
    if has_non_empty_query_param(&raw_query, &["share_token", "shareToken"]) {
        if let Some(slug) = extract_report_slug_for_share_check(&path, &raw_query) {
            if state
                .hooks
                .has_valid_share_access(&state, &slug, &raw_query)
                .await
            {
                return next.run(req).await;
            }
        }
    }

    // Credential path (a4): Valid Cloud SaaS API key / session
    if state
        .hooks
        .resolve_authenticated_actor(&state, headers)
        .await
        .is_some()
    {
        let uses_explicit_bearer_or_api_key = extract_bearer_token(headers).is_some()
            || headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .is_some_and(|s| !s.is_empty());
        let is_mutating_or_compute = !matches!(method, Method::GET | Method::HEAD)
            || (path == "/api/report" && matches!(method, Method::GET | Method::HEAD));
        if is_mutating_or_compute && !uses_explicit_bearer_or_api_key {
            if let Err(ge) = verify_same_origin_no_ct(headers, req_host_str(headers), &state.env) {
                return guardrail_error_to_response(ge);
            }
        }
        return next.run(req).await;
    }

    // Credential path (b): Signed `oe_session` cookie + CSRF / same-origin binding
    if let Some(sess) = resolve_verified_oe_session(&state.env, headers) {
        let req_host = req_host_str(headers);
        let csrf_ok = headers
            .get("x-openestate-token")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .is_some_and(|tok| constant_time_eq(tok.as_bytes(), sess.csrf_token.as_bytes()));
        let sec_fetch_site = headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .unwrap_or("");
        let sec_same = sec_fetch_site.eq_ignore_ascii_case("same-origin");
        let has_origin_or_ref = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
            || headers
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .is_some_and(|s| !s.is_empty());

        let is_mutating_or_compute =
            !matches!(method, Method::GET | Method::HEAD) || path == "/api/report";
        if is_mutating_or_compute {
            if !csrf_ok {
                return unauthorized_internal_auth_response();
            }
            if let Err(ge) = verify_same_origin_no_ct(headers, req_host, &state.env) {
                return guardrail_error_to_response(ge);
            }
            return next.run(req).await;
        } else {
            if sec_fetch_site.eq_ignore_ascii_case("cross-site") {
                return guardrail_error_to_response(GuardrailError::new(
                    "CROSS_ORIGIN_BLOCKED",
                    format!(
                        "Cross-site requests to {} endpoints are prohibited.",
                        state.env.brand_name()
                    ),
                    403,
                ));
            }
            if has_origin_or_ref {
                if let Err(ge) = verify_same_origin_no_ct(headers, req_host, &state.env) {
                    return guardrail_error_to_response(ge);
                }
            }
            if csrf_ok || sec_same || has_origin_or_ref {
                return next.run(req).await;
            }
            return unauthorized_internal_auth_response();
        }
    }

    unauthorized_internal_auth_response()
}

/// Resolves a non-spoofable browser/client ID for per-viewer UX state (likes, saved doc reviews,
/// agent match requests).
/// - Authenticated users bind to a deterministic `user-<uid>` ID.
/// - When `OPEN_ESTATE_REQUIRE_INTERNAL_AUTH=true` (and not calling with an explicit
///   `OPEN_ESTATE_INTERNAL_API_KEY` server key), anonymous browsers bind strictly to their signed
///   `oe_session` `sid`, ignoring spoofable `X-OpenEstate-Client-Id`.
pub async fn resolve_trusted_client_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(uid) = state.hooks.resolve_authenticated_actor(state, headers).await {
        let safe_uid: String = uid
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(40)
            .collect();
        if !safe_uid.is_empty() {
            return Some(format!("user-{safe_uid}"));
        }
    }
    let internal_enforced = internal_auth_required(&state.env);
    let has_internal_key = has_valid_internal_api_key(headers, &state.env);
    if internal_enforced && !has_internal_key {
        return resolve_verified_oe_session(&state.env, headers)
            .map(|sess| format!("oe-{}", &sess.sid[..sess.sid.len().min(40)]));
    }
    extract_browser_client_id(headers).or_else(|| {
        resolve_verified_oe_session(&state.env, headers)
            .map(|sess| format!("oe-{}", &sess.sid[..sess.sid.len().min(40)]))
    })
}
