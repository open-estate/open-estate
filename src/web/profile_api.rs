//! Account Profile & Feedback HTTP Handlers (Phase 9):
//! - `GET  /api/me/profile` (`handle_get_my_profile`)
//! - `PUT  /api/me/profile` (`handle_put_my_profile`)
//! - `POST /api/match/feedback` (`handle_post_match_feedback`)
//!
//! Stores strictly objective, anonymous preference profiles (`open_estate::scoring::profile::Profile`)
//! with zero protected demographic fields, and computes deterministic weight-adjustment suggestions
//! from saved vs dismissed home feedback.

use crate::db::proto::{buyer_profile_from_proto, buyer_profile_to_proto, unix_ts_to_iso};
use crate::db::MatchFeedbackRow;
use crate::guardrails::{
    clean_untrusted_text, enforce_allowed_body_keys, verify_same_origin, GuardrailError,
};
use crate::scoring::profile::{Profile, ALLOWED_PROFILE_KEYS};
use crate::search::matcher::{
    parse_dimension_snapshot, suggest_weight_adjustments, FeedbackAction, FeedbackSample,
};
use crate::shared::{now_iso, now_unix_secs};
use crate::web::server::{
    json_response, map_anyhow, map_guardrail, parse_json_body, req_host_str, ApiResult, AppState,
};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

const ALLOWED_FEEDBACK_KEYS: &[&str] = &[
    "userId",
    "user_id",
    "sessionKey",
    "session_key",
    "listingId",
    "listing_id",
    "action",
    "dimensionSnapshot",
    "dimension_snapshot",
    "dimensions",
    "reason",
    "profile",
];

#[derive(Deserialize, Default)]
pub struct MyProfileQuery {
    #[serde(
        alias = "userId",
        alias = "user_id",
        alias = "sessionKey",
        alias = "session_key",
        alias = "sessionId",
        alias = "session_id"
    )]
    pub user_id: Option<String>,
}

/// Validates an anonymous user or session identifier (alphanumeric, `-`, `_`, length 2..=128).
pub fn validate_user_id(raw: &str) -> std::result::Result<String, GuardrailError> {
    let clean = raw.trim();
    if clean.is_empty() {
        return Err(GuardrailError::new(
            "MISSING_USER_ID",
            "Provide a user or session identifier via `X-User-Id` header or `?userId=` / `?sessionKey=`.",
            400,
        ));
    }
    if clean.len() < 2
        || clean.len() > 128
        || !clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || !clean.chars().any(|c| c.is_ascii_alphanumeric())
    {
        return Err(GuardrailError::new(
            "INVALID_USER_ID",
            "User/session ID must be 2–128 characters using letters, digits, `-`, or `_` only.",
            400,
        ));
    }
    Ok(clean.to_string())
}

fn validate_listing_id(raw: &str) -> std::result::Result<String, GuardrailError> {
    let clean = raw.trim();
    if clean.is_empty()
        || clean.len() > 180
        || clean.contains("..")
        || !clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(GuardrailError::new(
            "INVALID_LISTING_ID",
            "A valid listingId (1–180 safe characters) is required.",
            400,
        ));
    }
    Ok(clean.to_string())
}

async fn resolve_user_id(
    state: &AppState,
    headers: &HeaderMap,
    query: &MyProfileQuery,
    body: Option<&Value>,
) -> std::result::Result<String, GuardrailError> {
    // 1. Always bind authenticated principals directly to their verified account ID first (prevent IDOR).
    if let Some(uid) = state.hooks.resolve_authenticated_actor(state, headers).await {
        return validate_user_id(&uid);
    }

    // 2. When internal auth is enforced, bind anonymous browser users strictly to their signed
    // `oe_session` ID unless the request came from a verified `OPEN_ESTATE_INTERNAL_API_KEY` proxy.
    let internal_enforced = crate::web::internal_auth::internal_auth_required(&state.env);
    let has_internal_key =
        crate::web::internal_auth::has_valid_internal_api_key(headers, &state.env);
    if internal_enforced && !has_internal_key {
        if let Some(sess) =
            crate::web::internal_auth::resolve_verified_oe_session(&state.env, headers)
        {
            return validate_user_id(&format!("oe_{}", sess.sid));
        }
        return Err(GuardrailError::new(
            "INTERNAL_AUTH_REQUIRED",
            "Authenticated session or valid internal API key is required for profile access.",
            401,
        ));
    }

    let from_hdr = headers
        .get("x-user-id")
        .or_else(|| headers.get("x-session-key"))
        .or_else(|| headers.get("x-openestate-user-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let from_query = query
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let from_body = body.and_then(|b| {
        b.get("userId")
            .or_else(|| b.get("user_id"))
            .or_else(|| b.get("sessionKey"))
            .or_else(|| b.get("session_key"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    });

    if let Some(explicit) = from_hdr.or(from_query).or(from_body) {
        return validate_user_id(explicit);
    }

    if let Some(sess) = crate::web::internal_auth::resolve_verified_oe_session(&state.env, headers)
    {
        return validate_user_id(&format!("oe_{}", sess.sid));
    }

    validate_user_id("")
}

fn parse_and_validate_profile(profile_val: &Value) -> std::result::Result<Profile, Response> {
    enforce_allowed_body_keys(profile_val, ALLOWED_PROFILE_KEYS).map_err(map_guardrail)?;
    let json_text = serde_json::to_string(profile_val).map_err(|e| {
        map_guardrail(GuardrailError::new(
            "INVALID_JSON_BODY",
            format!("Invalid profile JSON: {e}"),
            400,
        ))
    })?;
    Profile::from_json(&json_text).map_err(|errors| {
        let msg = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_PROFILE",
                "error": msg,
                "errors": errors,
            }),
        )
    })
}

/// `GET /api/me/profile`
/// Reads `X-User-Id` (or `?userId=` / `?sessionKey=`), returning the saved profile JSON from typed
/// `user_profiles` + `user_profile_places` tables or a 404 payload with `promptUploadLocalProfile: true`.
pub async fn handle_get_my_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MyProfileQuery>,
) -> ApiResult {
    let user_id = resolve_user_id(&state, &headers, &q, None)
        .await
        .map_err(map_guardrail)?;
    state.db.ensure_schema().await.map_err(map_anyhow)?;

    let Some(up) = state
        .db
        .get_user_profile_proto(&user_id)
        .await
        .map_err(map_anyhow)?
    else {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "found": false,
                "code": "PROFILE_NOT_FOUND",
                "userId": user_id,
                "profile": Value::Null,
                "promptUploadLocalProfile": true,
                "message": "No saved profile found for this user. Upload your local browser profile to sync across devices.",
            }),
        ));
    };

    let profile = buyer_profile_from_proto(&up);

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "found": true,
            "userId": up.user_id,
            "version": up.version,
            "promptUploadLocalProfile": false,
            "profile": profile,
            "updatedAt": unix_ts_to_iso(up.updated_ts),
            "createdAt": unix_ts_to_iso(up.created_ts),
        }),
    ))
}

/// `PUT /api/me/profile`
/// Validates `Profile::from_json` / `Profile::validate()`, persists directly as typed `UserProfileProto`
/// into `user_profiles` + `user_profile_places`, and returns the saved profile document.
pub async fn handle_put_my_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MyProfileQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 10, 1_000)?;
    let user_id = resolve_user_id(&state, &headers, &q, Some(&body))
        .await
        .map_err(map_guardrail)?;

    let profile_val = if body.get("profile").is_some()
        && body.get("version").is_none()
        && body.get("weights").is_none()
    {
        enforce_allowed_body_keys(
            &body,
            &["profile", "userId", "user_id", "sessionKey", "session_key"],
        )
        .map_err(map_guardrail)?;
        body.get("profile").unwrap()
    } else {
        &body
    };

    let profile = parse_and_validate_profile(profile_val)?;
    let existing = state
        .db
        .get_user_profile_proto(&user_id)
        .await
        .map_err(map_anyhow)?;
    let now_ts = now_unix_secs();
    let created_ts = existing
        .as_ref()
        .map(|e| e.created_ts)
        .filter(|&t| t > 0)
        .unwrap_or(now_ts);
    let user_sk = existing.as_ref().map(|e| e.user_sk).unwrap_or(0);
    let up = buyer_profile_to_proto(user_sk, &user_id, &profile, created_ts, now_ts);

    let _ = state
        .db
        .upsert_user_profile_proto(&up)
        .await
        .map_err(map_anyhow)?;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "userId": up.user_id,
            "version": up.version,
            "profile": profile,
            "updatedAt": unix_ts_to_iso(now_ts),
            "createdAt": unix_ts_to_iso(created_ts),
        }),
    ))
}

/// `POST /api/match/feedback`
/// Validates action (`save|dismiss|tour|hide`), persists to `match_feedback`, and returns
/// deterministic weight-adjustment suggestions computed from saved vs dismissed homes.
pub async fn handle_post_match_feedback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MyProfileQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 12, 2_000)?;
    enforce_allowed_body_keys(&body, ALLOWED_FEEDBACK_KEYS).map_err(map_guardrail)?;

    let user_id = resolve_user_id(&state, &headers, &q, Some(&body))
        .await
        .map_err(map_guardrail)?;
    let raw_listing_id = body
        .get("listingId")
        .or_else(|| body.get("listing_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let listing_id = validate_listing_id(raw_listing_id).map_err(map_guardrail)?;

    let raw_action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(action) = FeedbackAction::parse(raw_action) else {
        return Err(map_guardrail(GuardrailError::new(
            "INVALID_FEEDBACK_ACTION",
            "Feedback action must be one of: save, dismiss, tour, hide.",
            400,
        )));
    };

    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(|s| clean_untrusted_text(s, 280))
        .filter(|s| !s.is_empty());

    let snapshot_val = body
        .get("dimensionSnapshot")
        .or_else(|| body.get("dimension_snapshot"))
        .or_else(|| body.get("dimensions"));

    let dimension_snapshot_json = if let Some(sv) = snapshot_val.filter(|v| !v.is_null()) {
        let (utils, estimated) = parse_dimension_snapshot(sv).map_err(|msg| {
            map_guardrail(GuardrailError::new("INVALID_DIMENSION_SNAPSHOT", msg, 400))
        })?;
        Some(
            serde_json::to_string(&json!({
                "dimensionUtilities": utils,
                "estimatedDimensions": estimated,
            }))
            .map_err(|e| map_anyhow(e.into()))?,
        )
    } else {
        None
    };

    let inline_profile = if let Some(pv) = body.get("profile").filter(|v| !v.is_null()) {
        Some(parse_and_validate_profile(pv)?)
    } else {
        None
    };

    let row = MatchFeedbackRow {
        id: Uuid::new_v4().to_string(),
        user_id: user_id.clone(),
        listing_id: listing_id.clone(),
        action: action.as_str().to_string(),
        dimension_snapshot_json,
        reason: reason.clone(),
        created_at: now_iso(),
    };
    state
        .db
        .insert_match_feedback(&row)
        .await
        .map_err(map_anyhow)?;

    let history = state
        .db
        .list_match_feedback_for_user(&user_id, 200)
        .await
        .map_err(map_anyhow)?;
    let samples: Vec<FeedbackSample> = history
        .iter()
        .filter_map(|r| {
            let act = FeedbackAction::parse(&r.action)?;
            let (utils, est) = r
                .dimension_snapshot_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|v| parse_dimension_snapshot(&v).ok())
                .unwrap_or_default();
            Some(FeedbackSample {
                listing_id: r.listing_id.clone(),
                action: act,
                dimension_utilities: utils,
                estimated_dimensions: est,
                reason: r.reason.clone(),
            })
        })
        .collect();

    let active_profile = if let Some(p) = inline_profile {
        p
    } else if let Some(stored) = state
        .db
        .get_user_profile_proto(&user_id)
        .await
        .map_err(map_anyhow)?
    {
        buyer_profile_from_proto(&stored)
    } else {
        Profile::default()
    };

    let suggestions = suggest_weight_adjustments(&active_profile, &samples);

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "feedback": {
                "id": row.id,
                "userId": row.user_id,
                "listingId": row.listing_id,
                "action": row.action,
                "reason": row.reason,
                "createdAt": row.created_at,
            },
            "feedbackCount": samples.len(),
            "weightSuggestions": suggestions,
        }),
    ))
}
