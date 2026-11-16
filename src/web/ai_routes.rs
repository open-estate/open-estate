//! Axum HTTP handlers for interactive AI report endpoints (`POST /api/report/:slug/slides`
//! and `POST /api/report/:slug/artifact`), keeping web/HTTP concerns in `crate::web` rather
//! than inside domain modules under `crate::ai`.

use crate::ai::artifact_canvas::{
    detect_lang, generate_llm_artifact_with_client, validate_artifact_prompt,
};
use crate::ai::slides::{
    generate_llm_walkthrough_slides_with_client, is_valid_walkthrough_deck,
    scrub_walkthrough_deck_value,
};
use crate::db::CachedReportRecord;
use crate::guardrails::{enforce_allowed_body_keys, validate_slug_param, verify_same_origin};
use crate::web::hooks::authorize_cached_report;
use crate::web::http_util::{
    guardrail_error_to_response, json_response, parse_json_body, req_host_str,
};
use crate::web::orchestrator::DbLlmCache;
use crate::web::render::render_report_html_with_brand;
use crate::web::server::{resolve_client_ip_from_req, AppState};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path as AxumPath, RawQuery, State},
    http::{Extensions, HeaderMap, StatusCode},
    response::Response,
};
use serde_json::{json, Value};

/// Shared same-origin + JSON body + allowed-keys + slug + report lookup & ACL preamble for
/// `POST /api/report/:slug/*` AI endpoints.
async fn authorize_report_json_post(
    state: &AppState,
    headers: &HeaderMap,
    raw_query: &str,
    slug: &str,
    raw_body: std::result::Result<Bytes, BytesRejection>,
    allowed_keys: &[&str],
) -> std::result::Result<(String, Value, CachedReportRecord), Response> {
    if let Err(ge) = verify_same_origin(headers, req_host_str(headers), &state.env) {
        return Err(guardrail_error_to_response(ge));
    }
    let body = parse_json_body(raw_body, 8, 200)?;
    if let Err(ge) = enforce_allowed_body_keys(&body, allowed_keys) {
        return Err(guardrail_error_to_response(ge));
    }
    let clean_slug = match validate_slug_param(slug) {
        Ok(s) => s,
        Err(ge) => return Err(guardrail_error_to_response(ge)),
    };
    let rec = authorize_cached_report(state, &clean_slug, headers, raw_query, false)
        .await?
        .record;
    Ok((clean_slug, body, rec))
}

/// Production Axum route handler for `POST /api/report/:slug/slides`.
pub async fn handle_report_slides_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let (_, body, rec) = match authorize_report_json_post(
        &state,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        &slug,
        raw_body,
        &["refresh"],
    )
    .await
    {
        Ok(res) => res,
        Err(resp) => return resp,
    };

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = match state.shield.acquire_ai(&ip, "slides") {
        Ok(p) => p,
        Err(ge) => return guardrail_error_to_response(ge),
    };

    let refresh = body
        .get("refresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !refresh {
        if let Some(existing) = rec.report.get("walkthroughSlides") {
            let is_ai = existing.get("synthesisMode").and_then(|v| v.as_str()) == Some("ai_enhanced");
            if is_ai && is_valid_walkthrough_deck(existing) {
                let mut deck = existing.clone();
                scrub_walkthrough_deck_value(&mut deck);
                return json_response(
                    StatusCode::OK,
                    json!({
                        "ok": true,
                        "slug": rec.slug,
                        "walkthroughSlides": deck
                    }),
                );
            }
        }
    }

    let llm_cache = DbLlmCache(state.db.clone());
    let mut deck = generate_llm_walkthrough_slides_with_client(
        &state.http,
        &state.env,
        &rec.report,
        Some(&llm_cache),
        refresh,
    )
    .await;
    scrub_walkthrough_deck_value(&mut deck);

    let mut updated = rec.report.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("walkthroughSlides".to_string(), deck.clone());
    }
    let html = render_report_html_with_brand(
        &updated,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    let _ = state
        .db
        .put_cached_report(&rec.slug, &updated, Some(&html))
        .await;

    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": rec.slug,
            "walkthroughSlides": deck
        }),
    )
}

/// Production Axum route handler for `POST /api/report/:slug/artifact`.
pub async fn handle_report_artifact_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let (clean_slug, body, rec) = match authorize_report_json_post(
        &state,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        &slug,
        raw_body,
        &["prompt", "lang"],
    )
    .await
    {
        Ok(res) => res,
        Err(resp) => return resp,
    };

    let raw_prompt = body
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let explicit_lang = body.get("lang").and_then(|v| v.as_str());
    let lang = detect_lang(explicit_lang, raw_prompt);
    let address = rec
        .report
        .pointer("/meta/displayAddress")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let clean_prompt = match validate_artifact_prompt(raw_prompt, address, lang) {
        Ok(p) => p,
        Err(ge) => return guardrail_error_to_response(ge),
    };

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _permit = match state.shield.acquire_ai(&ip, "artifact") {
        Ok(p) => p,
        Err(ge) => return guardrail_error_to_response(ge),
    };

    let cache = DbLlmCache(state.db.clone());
    let artifact = generate_llm_artifact_with_client(
        &state.http,
        &state.env,
        &rec.report,
        &clean_prompt,
        lang.code(),
        Some(&cache),
    )
    .await;

    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": clean_slug,
            "artifact": artifact,
        }),
    )
}
