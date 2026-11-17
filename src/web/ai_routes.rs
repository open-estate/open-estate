//! Axum HTTP handlers for interactive AI report endpoints (`POST /api/report/:slug/slides`
//! and `POST /api/report/:slug/artifact`), keeping web/HTTP concerns in `crate::web` rather
//! than inside domain modules under `crate::ai`.

use crate::ai::artifact_canvas::{
    detect_lang, generate_llm_artifact_with_prompt, validate_artifact_prompt,
};
use crate::ai::slides::{
    generate_llm_walkthrough_slides_with_prompt, is_valid_walkthrough_deck,
    scrub_walkthrough_deck_value,
};
use crate::db::{
    extract_section_json, make_report_section_envelope, CachedReportRecord, ReportHeaderProto,
    ReportSectionKind,
};
use crate::guardrails::{enforce_allowed_body_keys, validate_slug_param, verify_same_origin};
use crate::shared::{is_truthy_flag, now_unix_secs};
use crate::web::cdn_cache::raw_query_has_share_or_api_key;
use crate::web::hooks::{authorize_cached_report, authorize_narrow_report};
use crate::web::http_util::{
    guardrail_error_to_response, json_response, parse_json_body, req_host_str,
};
use crate::web::orchestrator::DbLlmCache;
use crate::web::server::{resolve_client_ip_from_req, AppState};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path as AxumPath, RawQuery, State},
    http::{Extensions, HeaderMap, StatusCode},
    response::Response,
};
use serde_json::{json, Value};

/// Validates same-origin, JSON body bounds, allowed keys, and slug parameter for
/// `POST /api/report/:slug/*` AI endpoints without decompressing the 409 KB report up front.
fn validate_report_json_post_preamble(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    raw_body: std::result::Result<Bytes, BytesRejection>,
    allowed_keys: &[&str],
) -> std::result::Result<(String, Value), Response> {
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
    Ok((clean_slug, body))
}

fn extract_walkthrough_slides_from_section(sec_val: &Value) -> Option<&Value> {
    sec_val
        .pointer("/root/walkthroughSlides")
        .or_else(|| sec_val.get("walkthroughSlides"))
        .or_else(|| sec_val.get("slides").map(|_| sec_val))
}

/// Production Axum route handler for `POST /api/report/:slug/slides`.
///
/// Uses narrow `get_report_header` (~240 B) + `get_report_section(WalkthroughSlides)` (~2.5 KB)
/// on cached hits and persists generated decks via `put_report_section` (~2.5 KB write instead of
/// 904 KB full-report RMW), falling back to full `authorize_cached_report` only when private or
/// generating slides from report modules.
pub async fn handle_report_slides_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let raw_q_str = raw_query.as_deref().unwrap_or("");
    let (clean_slug, body) = match validate_report_json_post_preamble(
        &state,
        &headers,
        &slug,
        raw_body,
        &["refresh"],
    ) {
        Ok(res) => res,
        Err(resp) => return resp,
    };

    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let mut hdr_opt: Option<ReportHeaderProto> =
        if !private_reports && !raw_query_has_share_or_api_key(raw_q_str) {
            state.db.get_report_header(&clean_slug).await.ok().flatten()
        } else {
            None
        };
    let mut preloaded_rec: Option<CachedReportRecord> = None;
    if let Some(ref hdr) = hdr_opt {
        if let Err(resp) = authorize_narrow_report(
            &state,
            &hdr.slug,
            Some(hdr),
            json!({ "meta": { "displayAddress": hdr.display_address } }),
            &headers,
            raw_q_str,
            false,
        )
        .await
        {
            return resp;
        }
    } else {
        match authorize_cached_report(&state, &clean_slug, &headers, raw_q_str, false).await {
            Ok(auth) => {
                let rec = auth.record;
                hdr_opt = state.db.get_report_header(&rec.slug).await.ok().flatten();
                preloaded_rec = Some(rec);
            }
            Err(resp) => return resp,
        }
    }

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
        if let Some(ref hdr) = hdr_opt {
            let slides_bit = 1_i32 << (ReportSectionKind::ReportSectionWalkthroughSlides as u32);
            if (hdr.section_mask & slides_bit) != 0 {
                if let Ok(Some(sec_env)) = state
                    .db
                    .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionWalkthroughSlides)
                    .await
                {
                    if let Ok(sec_val) = extract_section_json(&sec_env) {
                        if let Some(existing) = extract_walkthrough_slides_from_section(&sec_val) {
                            let is_ai = existing.get("synthesisMode").and_then(|v| v.as_str())
                                == Some("ai_enhanced");
                            if is_ai && is_valid_walkthrough_deck(existing) {
                                let mut deck = existing.clone();
                                scrub_walkthrough_deck_value(&mut deck);
                                return json_response(
                                    StatusCode::OK,
                                    json!({
                                        "ok": true,
                                        "slug": hdr.slug,
                                        "walkthroughSlides": deck
                                    }),
                                );
                            }
                        }
                    }
                }
            }
        } else if let Some(ref rec) = preloaded_rec {
            if let Some(existing) = rec.report.get("walkthroughSlides") {
                let is_ai =
                    existing.get("synthesisMode").and_then(|v| v.as_str()) == Some("ai_enhanced");
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
    }

    let lookup_slug = hdr_opt
        .as_ref()
        .map(|h| h.slug.as_str())
        .unwrap_or(&clean_slug);
    let rec = match preloaded_rec {
        Some(r) => r,
        None => match authorize_cached_report(&state, lookup_slug, &headers, raw_q_str, false).await {
            Ok(auth) => auth.record,
            Err(resp) => return resp,
        },
    };
    let ai_prompts = state.db.load_ai_prompt_settings().await;
    let llm_cache = DbLlmCache(state.db.clone());
    let mut deck = generate_llm_walkthrough_slides_with_prompt(
        &state.http,
        &state.env,
        &rec.report,
        Some(&llm_cache),
        refresh,
        ai_prompts.slides_system_prompt.as_deref(),
    )
    .await;
    scrub_walkthrough_deck_value(&mut deck);

    let target_hdr = match hdr_opt {
        Some(h) if h.report_id > 0 => Some(h),
        _ => state.db.get_report_header(&rec.slug).await.ok().flatten(),
    };
    if let Some(hdr) = target_hdr.filter(|h| h.report_id > 0) {
        if let Ok(sec_env) = make_report_section_envelope(
            ReportSectionKind::ReportSectionWalkthroughSlides,
            &[("walkthroughSlides", &deck)],
            &[],
            now_unix_secs(),
        ) {
            let _ = state
                .db
                .put_report_section(
                    hdr.report_id,
                    ReportSectionKind::ReportSectionWalkthroughSlides,
                    &sec_env,
                )
                .await;
        }
    } else {
        let mut updated = rec.report.clone();
        if let Some(obj) = updated.as_object_mut() {
            obj.insert("walkthroughSlides".to_string(), deck.clone());
        }
        let _ = state.db.put_cached_report(&rec.slug, &updated, None).await;
    }

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
    let raw_q_str = raw_query.as_deref().unwrap_or("");
    let (clean_slug, body) = match validate_report_json_post_preamble(
        &state,
        &headers,
        &slug,
        raw_body,
        &["prompt", "lang"],
    ) {
        Ok(res) => res,
        Err(resp) => return resp,
    };

    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let hdr_opt = if !private_reports && !raw_query_has_share_or_api_key(raw_q_str) {
        state.db.get_report_header(&clean_slug).await.ok().flatten()
    } else {
        None
    };
    let mut preloaded_rec: Option<CachedReportRecord> = None;
    let address_for_guard = if let Some(ref hdr) = hdr_opt {
        if let Err(resp) = authorize_narrow_report(
            &state,
            &hdr.slug,
            Some(hdr),
            json!({ "meta": { "displayAddress": hdr.display_address } }),
            &headers,
            raw_q_str,
            false,
        )
        .await
        {
            return resp;
        }
        hdr.display_address.clone()
    } else {
        match authorize_cached_report(&state, &clean_slug, &headers, raw_q_str, false).await {
            Ok(auth) => {
                let rec = auth.record;
                let addr = rec
                    .report
                    .pointer("/meta/displayAddress")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&rec.display_address)
                    .to_string();
                preloaded_rec = Some(rec);
                addr
            }
            Err(resp) => return resp,
        }
    };

    let raw_prompt = body
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let explicit_lang = body.get("lang").and_then(|v| v.as_str());
    let lang = detect_lang(explicit_lang, raw_prompt);

    let clean_prompt = match validate_artifact_prompt(raw_prompt, &address_for_guard, lang) {
        Ok(p) => p,
        Err(ge) => return guardrail_error_to_response(ge),
    };

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _permit = match state.shield.acquire_ai(&ip, "artifact") {
        Ok(p) => p,
        Err(ge) => return guardrail_error_to_response(ge),
    };

    let rec = match preloaded_rec {
        Some(r) => r,
        None => match authorize_cached_report(&state, &clean_slug, &headers, raw_q_str, false).await {
            Ok(auth) => auth.record,
            Err(resp) => return resp,
        },
    };
    let ai_prompts = state.db.load_ai_prompt_settings().await;
    let cache = DbLlmCache(state.db.clone());
    let artifact = generate_llm_artifact_with_prompt(
        &state.http,
        &state.env,
        &rec.report,
        &clean_prompt,
        lang.code(),
        Some(&cache),
        ai_prompts.artifact_system_prompt.as_deref(),
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
