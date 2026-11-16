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

pub async fn resolve_cached_report(
    state: &crate::web::server::AppState,
    slug: &str,
    html_404: bool,
) -> std::result::Result<crate::db::CachedReportRecord, Response> {
    if let Some(rec) = crate::web::server::get_cached_or_disk_report(state, slug).await {
        return Ok(rec);
    }
    if html_404 {
        let brand_name = state.env.brand_name();
        let brand = html_escape::encode_text(&brand_name);
        return Err(html_response(
            StatusCode::NOT_FOUND,
            format!("<h1>Report not found</h1><p><a href=\"/\">Back to {brand}</a></p>"),
        ));
    }
    Err(json_response(
        StatusCode::NOT_FOUND,
        json!({ "ok": false, "code": "NOT_FOUND", "error": "not_found" }),
    ))
}
