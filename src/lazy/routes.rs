//! Axum HTTP routes for lazy report status polling and section-ready email subscriptions:
//! - `GET /api/report/:slug/lazy-status`
//! - `POST /api/report/:slug/subscriptions` (`{ sectionId, email }`)
//! - `POST /api/report/:slug/subscriptions/unsubscribe`
//! - `GET /u/section/:token` (and `POST /u/section/:token` for RFC 8058 one-click)

use crate::db::ReportSectionSubscriptionRow;
use crate::guardrails::{
    enforce_allowed_body_keys, validate_slug_param, verify_same_origin, GuardrailError,
};
use crate::lazy::runtime::lazy_section_status;
use crate::lazy::types::{humanize_section_label, SectionSubscriptionPublicView};
use crate::search::alerts::validate_email_input;
use crate::web::http_util::{
    html_response, json_response, map_anyhow, map_guardrail, parse_json_body, req_host_str,
    ApiResult,
};
use crate::web::server::AppState;
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    routing::{get, post},
    Router,
};
use html_escape::encode_safe;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, Default)]
struct UnsubscribeTokenQuery {
    token: Option<String>,
}

fn validate_section_id(raw: &str) -> std::result::Result<String, GuardrailError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(GuardrailError::new(
            "INVALID_SECTION_ID",
            "sectionId must be 1..=128 alphanumeric, '_' or '-' characters.",
            400,
        ));
    }
    Ok(trimmed.to_string())
}

pub fn lazy_routes() -> Router<AppState> {
    Router::new()
        .route("/api/report/:slug/lazy-status", get(handle_lazy_status_get))
        .route(
            "/api/report/:slug/subscriptions",
            post(handle_report_section_subscribe_post),
        )
        .route(
            "/api/report/:slug/subscriptions/unsubscribe",
            post(handle_report_section_unsubscribe_post),
        )
        .route(
            "/u/section/:token",
            get(handle_section_unsubscribe_get).post(handle_section_unsubscribe_post),
        )
}

async fn handle_lazy_status_get(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
) -> ApiResult {
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;
    let status = lazy_section_status(&state.db, &clean_slug)
        .await
        .map_err(map_anyhow)?;
    Ok(json_response(
        StatusCode::OK,
        serde_json::to_value(status).unwrap_or_else(|_| json!({ "ok": true, "slug": clean_slug })),
    ))
}

async fn handle_report_section_subscribe_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;

    let body = parse_json_body(raw_body, 6, 64)?;
    enforce_allowed_body_keys(&body, &["sectionId", "section_id", "email"])
        .map_err(map_guardrail)?;

    let raw_section = body
        .get("sectionId")
        .or_else(|| body.get("section_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let section_id = validate_section_id(raw_section).map_err(map_guardrail)?;

    let raw_email = body.get("email").and_then(Value::as_str).unwrap_or("");
    let clean_email = validate_email_input(raw_email).map_err(|e| {
        map_guardrail(GuardrailError::new(
            e.code.clone(),
            e.message.clone(),
            e.status,
        ))
    })?;

    let actor_user_id = state
        .hooks
        .resolve_authenticated_actor(&state, &headers)
        .await;

    let (row, created) = state
        .db
        .upsert_section_subscription(
            &clean_slug,
            &section_id,
            actor_user_id.as_deref(),
            &clean_email,
        )
        .await
        .map_err(map_anyhow)?;

    let lazy_stat = lazy_section_status(&state.db, &clean_slug)
        .await
        .map_err(map_anyhow)?;
    let section_status = lazy_stat.sections.get(&section_id);

    let status_code = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(json_response(
        status_code,
        json!({
            "ok": true,
            "created": created,
            "alreadySubscribed": !created,
            "subscription": SectionSubscriptionPublicView::from(&row),
            "sectionStatus": section_status,
        }),
    ))
}

fn extract_token_from_body_or_query(
    headers: &HeaderMap,
    q_token: Option<&str>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> String {
    if let Some(t) = q_token.map(str::trim).filter(|s| !s.is_empty()) {
        return t.to_string();
    }
    if let Ok(bytes) = raw_body {
        let txt = std::str::from_utf8(&bytes).unwrap_or("").trim();
        let ct = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ct.contains("application/json") || txt.starts_with('{') {
            if let Ok(j) = serde_json::from_str::<Value>(txt) {
                if let Some(t) = j.get("token").and_then(Value::as_str) {
                    return t.trim().to_string();
                }
            }
        } else {
            for (k, v) in url::form_urlencoded::parse(txt.as_bytes()) {
                if k == "token" && !v.trim().is_empty() {
                    return v.trim().to_string();
                }
            }
        }
    }
    String::new()
}

async fn handle_report_section_unsubscribe_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
    Query(q): Query<UnsubscribeTokenQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;
    let token = extract_token_from_body_or_query(&headers, q.token.as_deref(), raw_body);
    let updated = state
        .db
        .unsubscribe_section_by_token(&token, Some(&clean_slug))
        .await
        .map_err(map_anyhow)?;

    match updated {
        Some(row) => Ok(json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "found": true,
                "unsubscribed": true,
                "subscription": SectionSubscriptionPublicView::from(&row),
            }),
        )),
        None => Ok(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "found": false,
                "unsubscribed": false,
                "error": "Subscription token not found",
            }),
        )),
    }
}

fn render_section_unsub_page(
    brand_name: &str,
    sub: Option<&ReportSectionSubscriptionRow>,
    confirmed_unsub: bool,
) -> String {
    let wordmark = crate::shared::format_brand_wordmark_html(brand_name);
    let esc_brand = encode_safe(brand_name);

    let body_block = match sub {
        Some(row) => {
            let sec_label = humanize_section_label(&row.section_id);
            let esc_sec = encode_safe(&sec_label);
            let esc_email = encode_safe(&row.email);
            let esc_slug = encode_safe(&row.slug);
            let esc_sec_id = encode_safe(&row.section_id);
            let tok_enc = urlencoding::encode(&row.unsubscribe_token);
            let report_href = format!("/r/{esc_slug}/#{esc_sec_id}");

            if confirmed_unsub || row.status == "unsubscribed" {
                format!(
                    r#"<h1 style="margin:0 0 10px;font-size:20px;font-weight:800">Unsubscribed</h1>
<p style="margin:0 0 16px;color:#374151;line-height:1.55"><strong>{esc_email}</strong> will no longer receive updates for <strong>{esc_sec}</strong> on this report.</p>
<a href="{report_href}" style="display:inline-block;background:#0f766e;color:#fff;text-decoration:none;font-weight:700;padding:10px 16px;border-radius:9px">Return to Property Report</a>"#
                )
            } else {
                format!(
                    r#"<h1 style="margin:0 0 10px;font-size:20px;font-weight:800">Unsubscribe from Section Updates</h1>
<p style="margin:0 0 16px;color:#374151;line-height:1.55">Confirm unsubscribing <strong>{esc_email}</strong> from <strong>{esc_sec}</strong> notifications.</p>
<form method="post" action="/u/section/{tok_enc}" style="display:inline-block;margin-right:10px">
  <button type="submit" style="background:#0f766e;color:#fff;border:0;border-radius:9px;padding:10px 18px;font-weight:700;cursor:pointer">Confirm Unsubscribe</button>
</form>
<a href="{report_href}" style="display:inline-block;color:#0f766e;font-weight:600;text-decoration:none;padding:10px 6px">Cancel &amp; View Report</a>"#
                )
            }
        }
        None => r#"<h1 style="margin:0 0 10px;font-size:20px;font-weight:800">Subscription Not Found</h1>
<p style="margin:0 0 16px;color:#374151;line-height:1.55">This section notification link is invalid or no longer active.</p>
<a href="/" style="display:inline-block;background:#0f766e;color:#fff;text-decoration:none;font-weight:700;padding:10px 16px;border-radius:9px">Back to Home</a>"#.to_string(),
    };

    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Section Notifications — {esc_brand}</title></head>
<body style="margin:0;padding:24px;background:#f5f6f8;font-family:Inter,-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;color:#111827">
  <div style="max-width:560px;margin:24px auto;background:#fff;border:1px solid #e5e7eb;border-radius:14px;padding:24px">
    <div style="font-weight:800;font-size:20px;margin-bottom:16px">{wordmark}</div>
    {body_block}
  </div>
</body></html>"#
    )
}

async fn handle_section_unsubscribe_get(
    State(state): State<AppState>,
    AxumPath(token): AxumPath<String>,
) -> ApiResult {
    let row = state
        .db
        .get_section_subscription_by_token(&token)
        .await
        .map_err(map_anyhow)?;
    let status = if row.is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let html = render_section_unsub_page(&state.env.brand_name(), row.as_ref(), false);
    Ok(html_response(status, html))
}

async fn handle_section_unsubscribe_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> ApiResult {
    let row = state
        .db
        .unsubscribe_section_by_token(&token, None)
        .await
        .map_err(map_anyhow)?;
    let status = if row.is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/html") && !a.contains("application/json"))
        .unwrap_or(false);
    if wants_html {
        let html = render_section_unsub_page(&state.env.brand_name(), row.as_ref(), true);
        return Ok(html_response(status, html));
    }
    match row {
        Some(sub) => Ok(json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "found": true,
                "unsubscribed": true,
                "subscription": SectionSubscriptionPublicView::from(&sub),
            }),
        )),
        None => Ok(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "found": false,
                "unsubscribed": false,
                "error": "Subscription token not found",
            }),
        )),
    }
}
