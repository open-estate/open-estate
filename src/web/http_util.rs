//! Shared Axum HTTP response builders, error mappers, JSON body parsers, report cache lookup,
//! and origin helpers consolidated across `web::server`, `web::match_api`, `web::profile_api`,
//! `web::media_api`, `web::house3d_api`, and `web::ai_routes`.

use crate::guardrails::{
    apply_security_headers, enforce_json_depth_and_size, sanitize_public_error, GuardrailError,
    PublicError,
};
use crate::shared::EnvConfig;
use axum::{
    body::Bytes,
    extract::rejection::BytesRejection,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

pub type ApiResult = std::result::Result<Response, Response>;

pub fn public_error_to_response(pub_err: PublicError) -> Response {
    let status = StatusCode::from_u16(pub_err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let payload = json!({
        "ok": false,
        "code": pub_err.code,
        "error": pub_err.message,
    });
    let mut resp = (status, Json(payload)).into_response();
    if let Some(ra) = pub_err.retry_after_sec {
        if let Ok(hv) = HeaderValue::from_str(&ra.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, hv);
        }
    }
    apply_security_headers(resp.headers_mut());
    resp
}

pub fn guardrail_error_to_response(ge: GuardrailError) -> Response {
    public_error_to_response(sanitize_public_error(&anyhow::Error::new(ge)))
}

pub fn anyhow_error_to_response(err: anyhow::Error) -> Response {
    public_error_to_response(sanitize_public_error(&err))
}

#[inline]
pub fn map_guardrail(ge: GuardrailError) -> Response {
    guardrail_error_to_response(ge)
}

#[inline]
pub fn map_anyhow(err: anyhow::Error) -> Response {
    anyhow_error_to_response(err)
}

pub fn json_response(status: StatusCode, body: Value) -> Response {
    let mut resp = (status, Json(body)).into_response();
    apply_security_headers(resp.headers_mut());
    resp
}

pub fn json_response_with_cache(status: StatusCode, body: Value, hit: bool) -> Response {
    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-openestate-cache"),
        HeaderValue::from_static(if hit { "HIT" } else { "MISS" }),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

pub fn html_response(status: StatusCode, html: String) -> Response {
    let mut resp = (status, Html(html)).into_response();
    apply_security_headers(resp.headers_mut());
    resp
}

pub fn req_host_str(headers: &HeaderMap) -> &str {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("localhost")
}

pub fn resolve_base_origin_with_env(env: &EnvConfig, headers: &HeaderMap) -> String {
    if let Some(pub_url) = env.get("PUBLIC_BASE_URL") {
        return pub_url.trim_end_matches('/').to_string();
    }
    let host = req_host_str(headers);
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| *s == "https" || *s == "http")
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

pub fn parse_json_body(
    raw_res: std::result::Result<Bytes, BytesRejection>,
    max_depth: usize,
    max_keys_total: usize,
) -> std::result::Result<Value, Response> {
    let bytes = raw_res.map_err(|rej| {
        if rej.status() == StatusCode::PAYLOAD_TOO_LARGE {
            map_guardrail(GuardrailError::new(
                "PAYLOAD_TOO_LARGE",
                "Request payload exceeds maximum allowed size.",
                413,
            ))
        } else {
            map_guardrail(GuardrailError::new(
                "MALFORMED_REQUEST",
                "Failed to read HTTP request body.",
                400,
            ))
        }
    })?;

    let trimmed = std::str::from_utf8(&bytes)
        .map_err(|_| {
            map_guardrail(GuardrailError::new(
                "MALFORMED_REQUEST",
                "Request body must be valid UTF-8.",
                400,
            ))
        })?
        .trim();

    let val: Value = if trimmed.is_empty() {
        json!({})
    } else {
        serde_json::from_str(trimmed).map_err(|_| {
            map_guardrail(GuardrailError::new(
                "INVALID_JSON_BODY",
                "Malformed JSON request body.",
                400,
            ))
        })?
    };

    enforce_json_depth_and_size(&val, max_depth, max_keys_total).map_err(map_guardrail)?;
    Ok(val)
}

pub fn report_not_found_html(brand_name: &str) -> String {
    let eff_brand = if brand_name.trim().is_empty() {
        "Open Estate"
    } else {
        brand_name.trim()
    };
    let brand = html_escape::encode_text(eff_brand);
    let brand_wordmark = crate::shared::format_brand_wordmark_html(eff_brand);
    let fav = crate::web::favicon::FAVICON_HEAD_TAGS.as_str();
    let mark = crate::web::favicon::logo_mark_img(32);
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light dark"><title>{brand} — Report Not Found</title>{fav}<style>:root,html[data-theme="light"]{{color-scheme:light;--bg:#f5f6f8;--surface:#ffffff;--ink:#111827;--muted:#4b5563;--line:#e5e7eb;--brand:#0f766e;--brand-solid:#0f766e;--brand-hover:#0d655e;--shadow:0 12px 32px -18px rgba(17,24,39,.18)}}@media (prefers-color-scheme: dark){{:root:not([data-theme="light"]){{color-scheme:dark;--bg:#0b1120;--surface:#111827;--ink:#f1f5f9;--muted:#cbd5e1;--line:#334155;--brand:#14b8a6;--brand-solid:#0f766e;--brand-hover:#0d9488;--shadow:0 16px 36px -16px rgba(0,0,0,.65)}}}}html[data-theme="dark"]{{color-scheme:dark;--bg:#0b1120;--surface:#111827;--ink:#f1f5f9;--muted:#cbd5e1;--line:#334155;--brand:#14b8a6;--brand-solid:#0f766e;--brand-hover:#0d9488;--shadow:0 16px 36px -16px rgba(0,0,0,.65)}}*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--ink);font:15px/1.55 Inter,system-ui,-apple-system,sans-serif;display:grid;place-items:center;min-height:100vh;padding:20px}}.card{{background:var(--surface);border:1px solid var(--line);border-radius:16px;padding:28px 32px;max-width:500px;width:100%;box-shadow:var(--shadow)}}.logo{{display:inline-flex;align-items:center;gap:9px;font-weight:800;font-size:19px;margin-bottom:16px;color:var(--ink);text-decoration:none}}.logo .logo-mark{{width:32px;height:32px;flex:0 0 32px;display:block}}.logo b{{color:var(--brand)}}h1{{font-size:21px;margin:0 0 8px}}.msg{{color:var(--muted);margin:0 0 18px;font-size:14.5px}}.btn{{display:inline-block;background:var(--brand-solid);color:#fff;font-weight:700;font-size:14px;padding:10px 18px;border-radius:10px;text-decoration:none}}.btn:hover{{background:var(--brand-hover)}}</style></head><body><main class="card"><a class="logo" href="/">{mark}<span>{brand_wordmark}</span></a><h1>Report not found</h1><p class="msg">The requested property report is unavailable or no longer cached.</p><p style="margin:0"><a class="btn" href="/">Back to {brand}</a></p></main></body></html>"#
    )
}

pub async fn resolve_cached_report(
    state: &crate::web::server::AppState,
    slug: &str,
    html_404: bool,
) -> std::result::Result<crate::db::CachedReportRecord, Response> {
    if let Some(rec) = crate::web::server::get_cached_or_disk_report(state, slug).await {
        return Ok(rec);
    }
    if html_404 {
        return Err(html_response(
            StatusCode::NOT_FOUND,
            report_not_found_html(&state.env.brand_name()),
        ));
    }
    Err(json_response(
        StatusCode::NOT_FOUND,
        json!({ "ok": false, "code": "NOT_FOUND", "error": "not_found" }),
    ))
}
