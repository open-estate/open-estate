//! Admin HTTP API and self-contained Admin Console page (`/admin/scheduler`,
//! `/api/admin/scheduler/*`) for managing database-backed cron/interval/one-shot schedules.
//!
//! All routes require sanctioned admin authentication (`verify_admin_access`) and CSRF
//! protection (`verify_admin_csrf`) on mutations.

use crate::guardrails::{apply_security_headers, enforce_allowed_body_keys};
use crate::scheduler::cron::{ScheduleKind, ScheduleSpec};
use crate::scheduler::runner::{
    registered_job_kinds, wake_active_scheduler, RunNowError, SchedulerRunner,
};
use crate::scheduler::store::{
    create_scheduled_job, delete_scheduled_job, get_scheduled_job_by_id, list_job_runs,
    list_recent_runs, list_scheduled_jobs, pause_scheduled_job, resume_scheduled_job,
    scheduler_stats_json, update_scheduled_job,
};
use crate::scheduler::{
    CreateScheduledJobInput, RetryPolicy, ScheduledJob, UpdateScheduledJobPatch,
};
use crate::shared::now_unix_secs;
use crate::web::admin_console::{verify_admin_access, verify_admin_csrf};
use crate::web::http_util::{
    html_response, json_response, map_anyhow, map_guardrail, parse_json_body,
};
use crate::web::ops_dashboard::{admin_auth_error_json_response, AdminAuthContext};
use crate::web::server::AppState;
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path as AxumPath, Query, State},
    http::{header, Extensions, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STD, Engine as _};
use html_escape::encode_text;
use prost::Message;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListJobsQuery {
    kind: Option<String>,
    enabled: Option<String>,
    search: Option<String>,
    limit: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListRunsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    kind: Option<String>,
    status: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RunNowQuery {
    force: Option<bool>,
    #[serde(rename = "async")]
    async_mode: Option<bool>,
}

fn no_store_json(status: StatusCode, body: Value) -> Response {
    let mut resp = json_response(status, body);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    resp
}

async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> std::result::Result<AdminAuthContext, Response> {
    verify_admin_access(state, headers, extensions)
        .await
        .map_err(|e| admin_auth_error_json_response(&e))
}

async fn require_admin_mutation(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> std::result::Result<AdminAuthContext, Response> {
    let ctx = require_admin(state, headers, extensions).await?;
    verify_admin_csrf(&state.env, headers, &ctx)?;
    Ok(ctx)
}

async fn audit_scheduler_action(
    state: &AppState,
    auth: &AdminAuthContext,
    action: &str,
    job_id: &str,
    details: &Value,
) {
    let _ = state
        .db
        .append_admin_audit_log(&auth.actor, action, "scheduled_job", job_id, details)
        .await;
}

/// Admin-authenticated router for database scheduler management.
pub fn admin_scheduler_routes() -> Router<AppState> {
    Router::new()
        .route("/admin/scheduler", get(handle_admin_scheduler_page))
        .route(
            "/api/admin/scheduler/kinds",
            get(handle_scheduler_kinds_get),
        )
        .route(
            "/api/admin/scheduler/jobs",
            get(handle_scheduler_jobs_list).post(handle_scheduler_job_create),
        )
        .route(
            "/api/admin/scheduler/jobs/:id",
            get(handle_scheduler_job_get)
                .patch(handle_scheduler_job_update)
                .put(handle_scheduler_job_update)
                .delete(handle_scheduler_job_delete),
        )
        .route(
            "/api/admin/scheduler/jobs/:id/pause",
            post(handle_scheduler_job_pause),
        )
        .route(
            "/api/admin/scheduler/jobs/:id/resume",
            post(handle_scheduler_job_resume),
        )
        .route(
            "/api/admin/scheduler/jobs/:id/run-now",
            post(handle_scheduler_job_run_now),
        )
        .route(
            "/api/admin/scheduler/jobs/:id/runs",
            get(handle_scheduler_job_runs_get),
        )
        .route(
            "/api/admin/scheduler/runs",
            get(handle_scheduler_recent_runs_get),
        )
}

fn opt_str_field<'a>(body: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = body.get(*k).and_then(Value::as_str) {
            return Some(s);
        }
    }
    None
}

fn opt_u64_field(body: &Value, keys: &[&str]) -> Option<u64> {
    for k in keys {
        if let Some(v) = body.get(*k) {
            if let Some(n) = v.as_u64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.trim().parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn opt_i64_field(body: &Value, keys: &[&str]) -> Option<i64> {
    for k in keys {
        if let Some(v) = body.get(*k) {
            if let Some(n) = v.as_i64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn opt_bool_field(body: &Value, keys: &[&str]) -> Option<bool> {
    for k in keys {
        if let Some(v) = body.get(*k) {
            if let Some(b) = v.as_bool() {
                return Some(b);
            }
            if let Some(s) = v.as_str() {
                match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => return Some(true),
                    "false" | "0" | "no" => return Some(false),
                    _ => {}
                }
            }
        }
    }
    None
}

fn parse_retry_policy_val(v: &Value) -> std::result::Result<RetryPolicy, Response> {
    let mut rp = RetryPolicy::default();
    if let Some(ma) = opt_u64_field(v, &["maxAttempts", "max_attempts"]) {
        rp.max_attempts = ma.clamp(1, 50) as u32;
    }
    if let Some(ib) = opt_u64_field(v, &["initialBackoffSecs", "initial_backoff_secs"]) {
        rp.initial_backoff_secs = ib.clamp(1, 86_400);
    }
    if let Some(mb) = opt_u64_field(v, &["maxBackoffSecs", "max_backoff_secs"]) {
        rp.max_backoff_secs = mb.clamp(1, 7 * 86_400);
    }
    if let Some(mult) = v
        .get("backoffMultiplier")
        .or_else(|| v.get("backoff_multiplier"))
        .and_then(Value::as_f64)
    {
        if !mult.is_finite() || mult < 1.0 {
            return Err(no_store_json(
                StatusCode::BAD_REQUEST,
                json!({
                    "ok": false,
                    "code": "INVALID_RETRY_POLICY",
                    "error": "backoffMultiplier must be finite and >= 1.0",
                }),
            ));
        }
        rp.backoff_multiplier = mult;
    }
    if let Some(rt) = opt_bool_field(v, &["retryOnTimeout", "retry_on_timeout"]) {
        rp.retry_on_timeout = rt;
    }
    Ok(rp.normalized())
}

fn decode_optional_proto_b64(body: &Value) -> std::result::Result<Option<Vec<u8>>, Response> {
    let Some(raw) = opt_str_field(body, &["paramsProtoBase64", "params_proto_base64"]) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let bytes = BASE64_STD.decode(trimmed).map_err(|_| {
        no_store_json(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_PROTO_BASE64",
                "error": "paramsProtoBase64 is not valid base64",
            }),
        )
    })?;
    Ok(Some(bytes))
}

fn parse_params_value(body: &Value) -> std::result::Result<Option<Value>, Response> {
    if let Some(p) = body.get("params") {
        if p.is_null() {
            return Ok(Some(json!({})));
        }
        if p.is_object() {
            return Ok(Some(p.clone()));
        }
        return Err(no_store_json(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_PARAMS",
                "error": "params must be a JSON object",
            }),
        ));
    }
    if let Some(raw) = opt_str_field(body, &["paramsJson", "params_json"]) {
        let parsed: Value = serde_json::from_str(raw).map_err(|_| {
            no_store_json(
                StatusCode::BAD_REQUEST,
                json!({
                    "ok": false,
                    "code": "INVALID_PARAMS_JSON",
                    "error": "paramsJson must be valid JSON",
                }),
            )
        })?;
        return Ok(Some(parsed));
    }
    Ok(None)
}

async fn handle_scheduler_kinds_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "kinds": registered_job_kinds(),
        }),
    )
}

async fn handle_scheduler_jobs_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<ListJobsQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let enabled_filter =
        q.enabled
            .as_deref()
            .map(str::trim)
            .and_then(|s| match s.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            });
    let jobs = match list_scheduled_jobs(
        &state.db,
        q.kind.as_deref(),
        enabled_filter,
        q.search.as_deref(),
        q.limit.unwrap_or(500),
    )
    .await
    {
        Ok(list) => list,
        Err(e) => return map_anyhow(e),
    };
    let stats = scheduler_stats_json(&state.db)
        .await
        .unwrap_or_else(|_| json!({}));
    let items: Vec<Value> = jobs.iter().map(ScheduledJob::to_admin_json).collect();
    no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "count": items.len(),
            "jobs": items,
            "stats": stats,
            "registeredKinds": registered_job_kinds(),
        }),
    )
}

async fn handle_scheduler_job_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let job = match get_scheduled_job_by_id(&state.db, &id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            return no_store_json(
                StatusCode::NOT_FOUND,
                json!({
                    "ok": false,
                    "code": "NOT_FOUND",
                    "error": format!("Scheduled job '{}' not found", id.trim()),
                }),
            )
        }
        Err(e) => return map_anyhow(e),
    };
    let runs = list_job_runs(&state.db, &job.id, 25, 0)
        .await
        .unwrap_or_default();
    let proto_b64 = BASE64_STD.encode(job.to_proto().encode_to_vec());
    no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "job": job.to_admin_json(),
            "protoBase64": proto_b64,
            "recentRuns": runs.iter().map(|r| r.to_admin_json()).collect::<Vec<_>>(),
        }),
    )
}

const ALLOWED_CREATE_KEYS: &[&str] = &[
    "kind",
    "name",
    "ownerExtensionId",
    "owner_extension_id",
    "scheduleKind",
    "schedule_kind",
    "cronExpr",
    "cron_expr",
    "intervalSecs",
    "interval_secs",
    "runAtUnix",
    "run_at_unix",
    "jitterSecs",
    "jitter_secs",
    "timezone",
    "params",
    "paramsJson",
    "params_json",
    "paramsProtoBase64",
    "params_proto_base64",
    "enabled",
    "priority",
    "maxRuntimeSecs",
    "max_runtime_secs",
    "retryPolicy",
    "retry_policy",
];

async fn handle_scheduler_job_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 12, 256) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(ge) = enforce_allowed_body_keys(&body, ALLOWED_CREATE_KEYS) {
        return map_guardrail(ge);
    }

    let kind = opt_str_field(&body, &["kind"])
        .unwrap_or("")
        .trim()
        .to_string();
    let name = opt_str_field(&body, &["name"])
        .unwrap_or("")
        .trim()
        .to_string();
    if kind.is_empty() || name.is_empty() {
        return no_store_json(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_INPUT",
                "error": "Both 'kind' and 'name' are required",
            }),
        );
    }

    let cron_expr = opt_str_field(&body, &["cronExpr", "cron_expr"])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let interval_secs = opt_u64_field(&body, &["intervalSecs", "interval_secs"]);
    let run_at_unix = opt_i64_field(&body, &["runAtUnix", "run_at_unix"]);

    let sched_kind_str = opt_str_field(&body, &["scheduleKind", "schedule_kind"])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cron_expr.is_some() {
                "cron"
            } else if interval_secs.is_some() {
                "interval"
            } else {
                "once"
            }
        });
    let schedule_kind = match ScheduleKind::from_str_strict(sched_kind_str) {
        Ok(k) => k,
        Err(e) => {
            return no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_SCHEDULE", "error": e.to_string() }),
            )
        }
    };

    let jitter_secs = opt_u64_field(&body, &["jitterSecs", "jitter_secs"]).unwrap_or(0);
    let timezone = opt_str_field(&body, &["timezone"])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("UTC")
        .to_string();

    let schedule = ScheduleSpec {
        kind: schedule_kind,
        cron_expr,
        interval_secs,
        run_at_unix,
        jitter_secs,
        timezone,
    };
    if let Err(e) = schedule.validate() {
        return no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "INVALID_SCHEDULE", "error": e.to_string() }),
        );
    }

    let params = match parse_params_value(&body) {
        Ok(Some(v)) => v,
        Ok(None) => json!({}),
        Err(r) => return r,
    };
    let params_proto = match decode_optional_proto_b64(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let retry_policy =
        if let Some(rp_val) = body.get("retryPolicy").or_else(|| body.get("retry_policy")) {
            match parse_retry_policy_val(rp_val) {
                Ok(rp) => rp,
                Err(r) => return r,
            }
        } else {
            RetryPolicy::default()
        };

    let input = CreateScheduledJobInput {
        kind,
        name,
        owner_extension_id: opt_str_field(&body, &["ownerExtensionId", "owner_extension_id"])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        schedule,
        params,
        params_proto,
        enabled: opt_bool_field(&body, &["enabled"]).unwrap_or(true),
        priority: opt_i64_field(&body, &["priority"]).unwrap_or(0) as i32,
        max_runtime_secs: opt_u64_field(&body, &["maxRuntimeSecs", "max_runtime_secs"])
            .unwrap_or(300)
            .max(1),
        retry_policy,
        created_by: Some(auth.actor.clone()),
    };

    match create_scheduled_job(&state.db, &input).await {
        Ok(job) => {
            audit_scheduler_action(
                &state,
                &auth,
                "scheduler.create",
                &job.id,
                &json!({ "kind": job.kind, "name": job.name }),
            )
            .await;
            wake_active_scheduler();
            no_store_json(
                StatusCode::CREATED,
                json!({ "ok": true, "job": job.to_admin_json() }),
            )
        }
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.to_ascii_lowercase().contains("unique")
                || msg.to_ascii_lowercase().contains("duplicate")
            {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            no_store_json(
                status,
                json!({ "ok": false, "code": "CREATE_JOB_FAILED", "error": msg }),
            )
        }
    }
}

const ALLOWED_UPDATE_KEYS: &[&str] = &[
    "ownerExtensionId",
    "owner_extension_id",
    "scheduleKind",
    "schedule_kind",
    "cronExpr",
    "cron_expr",
    "intervalSecs",
    "interval_secs",
    "runAtUnix",
    "run_at_unix",
    "jitterSecs",
    "jitter_secs",
    "timezone",
    "params",
    "paramsJson",
    "params_json",
    "paramsProtoBase64",
    "params_proto_base64",
    "enabled",
    "priority",
    "maxRuntimeSecs",
    "max_runtime_secs",
    "retryPolicy",
    "retry_policy",
];

async fn handle_scheduler_job_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 12, 256) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(ge) = enforce_allowed_body_keys(&body, ALLOWED_UPDATE_KEYS) {
        return map_guardrail(ge);
    }

    let mut patch = UpdateScheduledJobPatch {
        updated_by: Some(auth.actor.clone()),
        ..Default::default()
    };

    if body.get("ownerExtensionId").is_some() || body.get("owner_extension_id").is_some() {
        patch.owner_extension_id = Some(
            opt_str_field(&body, &["ownerExtensionId", "owner_extension_id"])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    if let Some(sk) = opt_str_field(&body, &["scheduleKind", "schedule_kind"]) {
        match ScheduleKind::from_str_strict(sk) {
            Ok(k) => patch.schedule_kind = Some(k),
            Err(e) => {
                return no_store_json(
                    StatusCode::BAD_REQUEST,
                    json!({ "ok": false, "code": "INVALID_SCHEDULE", "error": e.to_string() }),
                )
            }
        }
    }
    if body.get("cronExpr").is_some() || body.get("cron_expr").is_some() {
        patch.cron_expr = Some(
            opt_str_field(&body, &["cronExpr", "cron_expr"])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    if body.get("intervalSecs").is_some() || body.get("interval_secs").is_some() {
        patch.interval_secs = Some(opt_u64_field(&body, &["intervalSecs", "interval_secs"]));
    }
    if body.get("runAtUnix").is_some() || body.get("run_at_unix").is_some() {
        patch.run_at_unix = Some(opt_i64_field(&body, &["runAtUnix", "run_at_unix"]));
    }
    if let Some(j) = opt_u64_field(&body, &["jitterSecs", "jitter_secs"]) {
        patch.jitter_secs = Some(j);
    }
    if let Some(tz) = opt_str_field(&body, &["timezone"]) {
        patch.timezone = Some(tz.trim().to_string());
    }
    match parse_params_value(&body) {
        Ok(Some(v)) => patch.params = Some(v),
        Ok(None) => {}
        Err(r) => return r,
    }
    match decode_optional_proto_b64(&body) {
        Ok(Some(b)) => patch.params_proto = Some(b),
        Ok(None) => {}
        Err(r) => return r,
    }
    if let Some(en) = opt_bool_field(&body, &["enabled"]) {
        patch.enabled = Some(en);
    }
    if let Some(pr) = opt_i64_field(&body, &["priority"]) {
        patch.priority = Some(pr as i32);
    }
    if let Some(mr) = opt_u64_field(&body, &["maxRuntimeSecs", "max_runtime_secs"]) {
        patch.max_runtime_secs = Some(mr.max(1));
    }
    if let Some(rp_val) = body.get("retryPolicy").or_else(|| body.get("retry_policy")) {
        match parse_retry_policy_val(rp_val) {
            Ok(rp) => patch.retry_policy = Some(rp),
            Err(r) => return r,
        }
    }

    match update_scheduled_job(&state.db, &id, &patch).await {
        Ok(Some(job)) => {
            audit_scheduler_action(&state, &auth, "scheduler.update", &job.id, &body).await;
            wake_active_scheduler();
            no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "job": job.to_admin_json() }),
            )
        }
        Ok(None) => no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
        ),
        Err(e) => no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "UPDATE_FAILED", "error": e.to_string() }),
        ),
    }
}

async fn handle_scheduler_job_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match delete_scheduled_job(&state.db, &id).await {
        Ok(true) => {
            audit_scheduler_action(&state, &auth, "scheduler.delete", &id, &json!({})).await;
            no_store_json(StatusCode::OK, json!({ "ok": true, "deleted": true }))
        }
        Ok(false) => no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
        ),
        Err(e) => map_anyhow(e),
    }
}

async fn handle_scheduler_job_pause(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match pause_scheduled_job(&state.db, &id, Some(&auth.actor)).await {
        Ok(Some(job)) => {
            audit_scheduler_action(&state, &auth, "scheduler.pause", &job.id, &json!({})).await;
            no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "job": job.to_admin_json() }),
            )
        }
        Ok(None) => no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
        ),
        Err(e) => map_anyhow(e),
    }
}

async fn handle_scheduler_job_resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match resume_scheduled_job(&state.db, &id, Some(&auth.actor)).await {
        Ok(Some(job)) => {
            audit_scheduler_action(&state, &auth, "scheduler.resume", &job.id, &json!({})).await;
            wake_active_scheduler();
            no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "job": job.to_admin_json() }),
            )
        }
        Ok(None) => no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
        ),
        Err(e) => map_anyhow(e),
    }
}

async fn handle_scheduler_job_run_now(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<RunNowQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 6, 32) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(ge) = enforce_allowed_body_keys(&body, &["force", "async"]) {
        return map_guardrail(ge);
    }
    let force = q
        .force
        .or_else(|| opt_bool_field(&body, &["force"]))
        .unwrap_or(false);
    let async_mode = q
        .async_mode
        .or_else(|| opt_bool_field(&body, &["async"]))
        .unwrap_or(false);

    if async_mode {
        let Some(existing) = (match get_scheduled_job_by_id(&state.db, &id).await {
            Ok(v) => v,
            Err(e) => return map_anyhow(e),
        }) else {
            return no_store_json(
                StatusCode::NOT_FOUND,
                json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
            );
        };
        if !existing.enabled && !force {
            return no_store_json(
                StatusCode::CONFLICT,
                json!({
                    "ok": false,
                    "code": "JOB_PAUSED",
                    "error": "Job is paused; resume it or pass force=true",
                }),
            );
        }
        let updated = match update_scheduled_job(
            &state.db,
            &id,
            &UpdateScheduledJobPatch {
                enabled: if force { Some(true) } else { None },
                next_run_at_override: Some(Some(now_unix_secs())),
                updated_by: Some(auth.actor.clone()),
                ..Default::default()
            },
        )
        .await
        {
            Ok(Some(j)) => j,
            Ok(None) => {
                return no_store_json(
                    StatusCode::NOT_FOUND,
                    json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
                )
            }
            Err(e) => return map_anyhow(e),
        };
        audit_scheduler_action(
            &state,
            &auth,
            "scheduler.run_now_async",
            &updated.id,
            &json!({ "force": force }),
        )
        .await;
        wake_active_scheduler();
        return no_store_json(
            StatusCode::ACCEPTED,
            json!({
                "ok": true,
                "queued": true,
                "job": updated.to_admin_json(),
            }),
        );
    }

    let runner = SchedulerRunner::from_app_state(&state);
    match runner.run_job_now(&id, force).await {
        Ok((job_opt, run_opt)) => {
            audit_scheduler_action(
                &state,
                &auth,
                "scheduler.run_now",
                &id,
                &json!({
                    "force": force,
                    "runId": run_opt.as_ref().map(|r| r.run_id.as_str()),
                    "status": run_opt.as_ref().map(|r| r.status.as_str()),
                }),
            )
            .await;
            no_store_json(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "job": job_opt.map(|j| j.to_admin_json()),
                    "run": run_opt.map(|r| r.to_admin_json()),
                }),
            )
        }
        Err(RunNowError::NotFound) => no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
        ),
        Err(RunNowError::Paused) => no_store_json(
            StatusCode::CONFLICT,
            json!({
                "ok": false,
                "code": "JOB_PAUSED",
                "error": "Job is paused; resume it or pass force=true",
            }),
        ),
        Err(RunNowError::AlreadyRunning) => no_store_json(
            StatusCode::CONFLICT,
            json!({
                "ok": false,
                "code": "JOB_ALREADY_RUNNING",
                "error": "Job currently holds an active lease",
            }),
        ),
        Err(RunNowError::Internal(msg)) => no_store_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "code": "RUN_FAILED", "error": msg }),
        ),
    }
}

async fn handle_scheduler_job_runs_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<ListRunsQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let Some(job) = (match get_scheduled_job_by_id(&state.db, &id).await {
        Ok(v) => v,
        Err(e) => return map_anyhow(e),
    }) else {
        return no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "NOT_FOUND", "error": "Scheduled job not found" }),
        );
    };
    let runs = match list_job_runs(
        &state.db,
        &job.id,
        q.limit.unwrap_or(50),
        q.offset.unwrap_or(0),
    )
    .await
    {
        Ok(list) => list,
        Err(e) => return map_anyhow(e),
    };
    no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "jobId": job.id,
            "runs": runs.iter().map(|r| r.to_admin_json()).collect::<Vec<_>>(),
        }),
    )
}

async fn handle_scheduler_recent_runs_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<ListRunsQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let runs = match list_recent_runs(
        &state.db,
        q.kind.as_deref(),
        q.status.as_deref(),
        q.limit.unwrap_or(100),
    )
    .await
    {
        Ok(list) => list,
        Err(e) => return map_anyhow(e),
    };
    no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "count": runs.len(),
            "runs": runs.iter().map(|r| r.to_admin_json()).collect::<Vec<_>>(),
        }),
    )
}

async fn handle_admin_scheduler_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    let auth = match verify_admin_access(&state, &headers, &extensions).await {
        Ok(a) => a,
        Err(err) => {
            let brand = encode_text(&state.env.brand_name()).into_owned();
            let msg = encode_text(err.message).into_owned();
            let gate = format!(
                r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Sign In — {brand} Scheduler</title><style>body{{font-family:Inter,-apple-system,sans-serif;background:#f8f9fa;color:#202124;display:grid;place-items:center;min-height:100vh;margin:0;padding:24px}}.card{{max-width:440px;width:100%;background:#fff;border:1px solid #dadce0;border-radius:8px;padding:28px}}.err{{background:#fce8e6;border:1px solid #fad2cf;color:#c5221f;padding:10px 12px;border-radius:4px;font-size:13px;margin:14px 0}}a{{color:#1a73e8}}</style></head><body><main class="card"><h1 style="margin:0 0 8px;font-size:18px">{brand} DB Scheduler</h1><div class="err">{msg}</div><p style="font-size:13px;color:#5f6368">Sign in on <a href="/admin">/admin</a> first or send <code style="font-family:monospace">Authorization: Bearer &lt;token&gt;</code>.</p></main></body></html>"#
            );
            let mut resp = html_response(err.status, gate);
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, max-age=0"),
            );
            return resp;
        }
    };

    let brand = encode_text(&state.env.brand_name()).into_owned();
    let actor = encode_text(&auth.actor).into_owned();
    let html = ADMIN_SCHEDULER_HTML
        .replace("__BRAND__", &brand)
        .replace("__ACTOR__", &actor);
    let mut resp = html_response(StatusCode::OK, html);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

const ADMIN_SCHEDULER_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>DB Scheduler — __BRAND__ Admin</title>
<style>
:root {
  color-scheme: light dark;
  --bg: #f8f9fa; --card: #ffffff; --subtle: #f1f3f4; --ink: #202124; --muted: #5f6368;
  --line: #dadce0; --brand: #1a73e8; --good: #1e8e3e; --bad: #c5221f; --warn: #b06000;
  --font: "Google Sans", Roboto, Inter, -apple-system, sans-serif;
  --mono: "Roboto Mono", ui-monospace, Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #131314; --card: #1e1f20; --subtle: #282a2c; --ink: #e8eaed; --muted: #9aa0a6;
    --line: #3c4043; --brand: #8ab4f8; --good: #81c995; --bad: #f28b82; --warn: #fdd663;
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink); font: 14px/1.5 var(--font); }
header { position: sticky; top: 0; z-index: 20; background: var(--card); border-bottom: 1px solid var(--line); padding: 12px 20px; display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
h1 { margin: 0; font-size: 17px; font-weight: 600; }
.nav { display: flex; gap: 10px; align-items: center; font-size: 13px; }
.nav a { color: var(--brand); text-decoration: none; font-weight: 500; }
.wrap { max-width: 1360px; margin: 0 auto; padding: 18px 20px 64px; }
.kpis { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 10px; margin-bottom: 16px; }
.kpi { background: var(--card); border: 1px solid var(--line); border-radius: 6px; padding: 12px 14px; }
.kpi .k { font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); }
.kpi .v { font-size: 22px; font-weight: 600; margin-top: 2px; }
.card { background: var(--card); border: 1px solid var(--line); border-radius: 6px; padding: 14px; margin-bottom: 16px; overflow-x: auto; }
.toolbar { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; justify-content: space-between; margin-bottom: 12px; }
.btn { padding: 6px 12px; border-radius: 4px; border: 1px solid var(--line); background: var(--card); color: var(--ink); cursor: pointer; font: 500 12.5px var(--font); }
.btn.primary { background: #1a73e8; border-color: #1a73e8; color: #fff; }
.btn.sm { padding: 4px 8px; font-size: 12px; }
.btn.danger { color: var(--bad); }
input, select, textarea { padding: 6px 8px; border: 1px solid var(--line); border-radius: 4px; background: var(--bg); color: var(--ink); font: 13px var(--font); }
textarea { font-family: var(--mono); font-size: 12px; width: 100%; }
table { width: 100%; border-collapse: collapse; font-size: 13px; }
th, td { text-align: left; padding: 8px 10px; border-bottom: 1px solid var(--line); vertical-align: top; }
th { font-size: 11.5px; text-transform: uppercase; letter-spacing: 0.03em; color: var(--muted); font-weight: 600; }
.mono { font-family: var(--mono); font-size: 12px; }
.pill { display: inline-block; padding: 2px 7px; border-radius: 999px; font-size: 11px; font-weight: 600; border: 1px solid var(--line); }
.pill.ok { color: var(--good); }
.pill.bad { color: var(--bad); }
.pill.warn { color: var(--warn); }
.modal-bg { position: fixed; inset: 0; background: rgba(0,0,0,0.45); display: none; align-items: center; justify-content: center; padding: 16px; z-index: 50; }
.modal-bg.open { display: flex; }
.modal { background: var(--card); border: 1px solid var(--line); border-radius: 8px; max-width: 640px; width: 100%; padding: 18px; max-height: 90vh; overflow-y: auto; }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
label { display: block; font-size: 12px; font-weight: 500; color: var(--muted); margin-bottom: 4px; }
</style>
</head>
<body>
<header>
  <div><h1>__BRAND__ — DB Scheduler</h1><div class="mono" style="font-size:11.5px;color:var(--muted)">Operator: __ACTOR__</div></div>
  <div class="nav">
    <a href="/admin">← Admin Console</a>
    <a href="/ops">Ops Telemetry</a>
    <button class="btn primary" id="newJobBtn">+ New Schedule</button>
    <button class="btn" id="refreshBtn">Refresh</button>
  </div>
</header>
<div class="wrap">
  <div class="kpis" id="kpis"></div>
  <div class="card">
    <div class="toolbar">
      <div style="display:flex;gap:8px;flex-wrap:wrap">
        <input id="searchInput" placeholder="Filter by kind, name, extension...">
        <select id="enabledFilter"><option value="">All states</option><option value="true">Enabled</option><option value="false">Paused</option></select>
      </div>
      <div class="mono" id="kindsSummary" style="font-size:11.5px;color:var(--muted)"></div>
    </div>
    <table>
      <thead><tr><th>Kind / Name</th><th>Schedule</th><th>Next Run (UTC)</th><th>Last Run &amp; Status</th><th>Lease / Priority</th><th>Actions</th></tr></thead>
      <tbody id="jobsBody"></tbody>
    </table>
  </div>
  <div class="card">
    <div class="toolbar"><h2 style="margin:0;font-size:15px">Recent Executions &amp; MeterScope Usage</h2></div>
    <table>
      <thead><tr><th>Job</th><th>Status</th><th>Attempt</th><th>Started / Finished</th><th>MeterScope Usage</th><th>Output / Error</th></tr></thead>
      <tbody id="runsBody"></tbody>
    </table>
  </div>
</div>

<div class="modal-bg" id="jobModal">
  <div class="modal">
    <h2 id="modalTitle" style="margin:0 0 12px;font-size:16px">Edit Scheduled Job</h2>
    <form id="jobForm">
      <input type="hidden" id="fId">
      <div class="grid2" style="margin-bottom:10px">
        <div><label>Job Kind</label><input id="fKind" required style="width:100%" placeholder="extension_refresh"></div>
        <div><label>Job Name</label><input id="fName" required style="width:100%" placeholder="default"></div>
      </div>
      <div class="grid2" style="margin-bottom:10px">
        <div><label>Schedule Kind</label><select id="fSchedKind" style="width:100%"><option value="cron">cron</option><option value="interval">interval</option><option value="once">once</option></select></div>
        <div><label>Timezone</label><input id="fTimezone" value="UTC" style="width:100%"></div>
      </div>
      <div class="grid2" style="margin-bottom:10px">
        <div><label>Cron Expr (5-field)</label><input id="fCron" placeholder="*/15 * * * *" style="width:100%"></div>
        <div><label>Interval Secs</label><input id="fInterval" type="number" min="1" placeholder="900" style="width:100%"></div>
      </div>
      <div class="grid2" style="margin-bottom:10px">
        <div><label>Jitter Secs</label><input id="fJitter" type="number" min="0" value="0" style="width:100%"></div>
        <div><label>Max Runtime Secs</label><input id="fMaxRuntime" type="number" min="1" value="300" style="width:100%"></div>
      </div>
      <div class="grid2" style="margin-bottom:10px">
        <div><label>Owner Extension ID (optional)</label><input id="fExtId" style="width:100%"></div>
        <div><label>Priority</label><input id="fPriority" type="number" value="0" style="width:100%"></div>
      </div>
      <div style="margin-bottom:10px">
        <label>Params (JSON)</label>
        <textarea id="fParams" rows="4">{}</textarea>
      </div>
      <div style="display:flex;justify-content:space-between;align-items:center;margin-top:12px">
        <label style="display:inline-flex;align-items:center;gap:6px;margin:0"><input type="checkbox" id="fEnabled" checked> Enabled</label>
        <div style="display:flex;gap:8px">
          <button type="button" class="btn" id="cancelModal">Cancel</button>
          <button type="submit" class="btn primary">Save</button>
        </div>
      </div>
    </form>
  </div>
</div>

<script>
(() => {
  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
  let jobsCache = [];
  async function api(url, opts) {
    const res = await fetch(url, Object.assign({ credentials: 'same-origin', headers: { 'Content-Type': 'application/json' } }, opts || {}));
    const data = await res.json();
    if (!res.ok || data.ok === false) throw new Error(data.error || ('HTTP ' + res.status));
    return data;
  }
  async function loadAll() {
    const q = new URLSearchParams();
    const s = document.getElementById('searchInput').value.trim();
    const en = document.getElementById('enabledFilter').value;
    if (s) q.set('search', s);
    if (en) q.set('enabled', en);
    const [jData, rData] = await Promise.all([
      api('/api/admin/scheduler/jobs?' + q.toString()),
      api('/api/admin/scheduler/runs?limit=40')
    ]);
    jobsCache = jData.jobs || [];
    const st = jData.stats || {};
    document.getElementById('kpis').innerHTML = [
      ['Total Jobs', st.jobsTotal || 0],
      ['Enabled', st.jobsEnabled || 0],
      ['Paused', st.jobsPaused || 0],
      ['Running Leases', st.jobsRunning || 0],
      ['Failing', st.jobsFailing || 0]
    ].map(([k, v]) => `<div class="kpi"><div class="k">${esc(k)}</div><div class="v">${esc(v)}</div></div>`).join('');
    document.getElementById('kindsSummary').textContent = 'Registered handlers: ' + (jData.registeredKinds || []).join(', ');
    document.getElementById('jobsBody').innerHTML = jobsCache.map(j => {
      const sched = j.scheduleKind === 'cron' ? `cron: ${j.cronExpr} (${j.timezone})`
        : j.scheduleKind === 'interval' ? `every ${j.intervalSecs}s` : `once`;
      const stPill = j.enabled ? '<span class="pill ok">enabled</span>' : '<span class="pill warn">paused</span>';
      const lastPill = j.lastStatus ? `<span class="pill ${j.lastStatus==='succeeded'?'ok':j.lastStatus==='running'?'warn':'bad'}">${esc(j.lastStatus)}</span>` : '-';
      return `<tr>
        <td><div style="font-weight:600">${esc(j.kind)} <span class="mono" style="color:var(--muted)">/${esc(j.name)}</span></div><div class="mono" style="color:var(--muted)">${esc(j.id)} ${j.ownerExtensionId ? '· ' + esc(j.ownerExtensionId) : ''}</div></td>
        <td class="mono">${esc(sched)}${j.jitterSecs ? ' +' + j.jitterSecs + 's' : ''}</td>
        <td class="mono">${esc(j.nextRunAt || '-')}</td>
        <td>${stPill} ${lastPill}<div class="mono" style="color:var(--muted)">${esc(j.lastRunAt || '')}</div>${j.lastError ? `<div class="mono" style="color:var(--bad)">${esc(j.lastError)}</div>` : ''}</td>
        <td class="mono">${esc(j.leaseOwner || '-')}<br>prio=${esc(j.priority)} max=${esc(j.maxRuntimeSecs)}s</td>
        <td style="white-space:nowrap">
          <button class="btn sm" onclick="window.__editJob('${esc(j.id)}')">Edit</button>
          <button class="btn sm" onclick="window.__toggleJob('${esc(j.id)}', ${j.enabled})">${j.enabled ? 'Pause' : 'Resume'}</button>
          <button class="btn sm primary" onclick="window.__runNow('${esc(j.id)}')">Run Now</button>
        </td>
      </tr>`;
    }).join('');
    document.getElementById('runsBody').innerHTML = (rData.runs || []).map(r => {
      const d = (r.usageSummary && r.usageSummary.dims) || {};
      const usageTxt = `inv=${d.invocations||0} wall=${d.wallTimeMs||0}ms ext=${d.externalApiCalls||0} bytes=${d.bytesDownloaded||0} llmTok=${(d.llmInputTokens||0)+(d.llmOutputTokens||0)}`;
      return `<tr>
        <td class="mono">${esc(r.jobKind)}/${esc(r.jobName)}<br><span style="color:var(--muted)">${esc(r.runId)}</span></td>
        <td><span class="pill ${r.status==='succeeded'?'ok':r.status==='running'?'warn':'bad'}">${esc(r.status)}</span></td>
        <td class="mono">#${esc(r.attempt)}</td>
        <td class="mono">${esc(r.startedAt)}<br>${esc(r.finishedAt || '-')}</td>
        <td class="mono">${esc(usageTxt)}</td>
        <td class="mono">${r.error ? `<span style="color:var(--bad)">${esc(r.error)}</span>` : esc(r.output && r.output.summary ? r.output.summary : 'ok')}</td>
      </tr>`;
    }).join('');
  }
  function openModal(job) {
    document.getElementById('fId').value = job ? job.id : '';
    document.getElementById('fKind').value = job ? job.kind : '';
    document.getElementById('fKind').disabled = !!job;
    document.getElementById('fName').value = job ? job.name : 'default';
    document.getElementById('fName').disabled = !!job;
    document.getElementById('fSchedKind').value = job ? job.scheduleKind : 'cron';
    document.getElementById('fTimezone').value = job ? job.timezone : 'UTC';
    document.getElementById('fCron').value = (job && job.cronExpr) || '';
    document.getElementById('fInterval').value = (job && job.intervalSecs) || '';
    document.getElementById('fJitter').value = (job && job.jitterSecs) || 0;
    document.getElementById('fMaxRuntime').value = (job && job.maxRuntimeSecs) || 300;
    document.getElementById('fExtId').value = (job && job.ownerExtensionId) || '';
    document.getElementById('fPriority').value = (job && job.priority) || 0;
    document.getElementById('fParams').value = JSON.stringify((job && job.params) || {}, null, 2);
    document.getElementById('fEnabled').checked = job ? !!job.enabled : true;
    document.getElementById('modalTitle').textContent = job ? 'Edit Scheduled Job' : 'Create Scheduled Job';
    document.getElementById('jobModal').classList.add('open');
  }
  window.__editJob = (id) => { const j = jobsCache.find(x => x.id === id); if (j) openModal(j); };
  window.__toggleJob = async (id, cur) => { await api(`/api/admin/scheduler/jobs/${encodeURIComponent(id)}/${cur ? 'pause' : 'resume'}`, { method: 'POST', body: '{}' }); loadAll(); };
  window.__runNow = async (id) => { await api(`/api/admin/scheduler/jobs/${encodeURIComponent(id)}/run-now`, { method: 'POST', body: '{}' }); loadAll(); };
  document.getElementById('newJobBtn').onclick = () => openModal(null);
  document.getElementById('cancelModal').onclick = () => document.getElementById('jobModal').classList.remove('open');
  document.getElementById('refreshBtn').onclick = loadAll;
  document.getElementById('searchInput').oninput = loadAll;
  document.getElementById('enabledFilter').onchange = loadAll;
  document.getElementById('jobForm').onsubmit = async (ev) => {
    ev.preventDefault();
    const id = document.getElementById('fId').value.trim();
    const payload = {
      scheduleKind: document.getElementById('fSchedKind').value,
      timezone: document.getElementById('fTimezone').value.trim() || 'UTC',
      cronExpr: document.getElementById('fCron').value.trim() || null,
      intervalSecs: document.getElementById('fInterval').value ? Number(document.getElementById('fInterval').value) : null,
      jitterSecs: Number(document.getElementById('fJitter').value || 0),
      maxRuntimeSecs: Number(document.getElementById('fMaxRuntime').value || 300),
      ownerExtensionId: document.getElementById('fExtId').value.trim() || null,
      priority: Number(document.getElementById('fPriority').value || 0),
      params: JSON.parse(document.getElementById('fParams').value || '{}'),
      enabled: document.getElementById('fEnabled').checked
    };
    if (id) {
      await api(`/api/admin/scheduler/jobs/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify(payload) });
    } else {
      payload.kind = document.getElementById('fKind').value.trim();
      payload.name = document.getElementById('fName').value.trim();
      await api('/api/admin/scheduler/jobs', { method: 'POST', body: JSON.stringify(payload) });
    }
    document.getElementById('jobModal').classList.remove('open');
    loadAll();
  };
  loadAll().catch(e => alert(e.message));
})();
</script>
</body>
</html>"#;
