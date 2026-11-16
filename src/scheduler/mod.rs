//! Database-backed cron/interval/one-shot scheduler for Open Estate & downstream hosts.
//!
//! All recurring schedules and background refresh jobs live in `scheduled_jobs` and
//! record execution history + `MeterScope` usage summaries in `scheduled_job_runs`.

pub mod cron;
pub mod handlers;
pub mod routes;
pub mod runner;
pub mod store;

pub use cron::{
    compute_next_run_at, deterministic_jitter_secs, CronExpr, LocalResolution, ScheduleKind,
    ScheduleSpec, SchedulerTimeZone,
};
pub use handlers::{
    register_builtin_public_handlers, seed_public_default_jobs, EXTENSION_REFRESH_JOB_KIND,
    OSRM_HEALTH_CHECK_JOB_KIND,
};
pub use routes::admin_scheduler_routes;
pub use runner::{
    global_handler_registry, register_job_kind, register_job_kind_fn, registered_job_kinds,
    spawn_scheduler_runner, wake_active_scheduler, JobRunOutput, ScheduledJobHandler,
    SchedulerBoxFuture, SchedulerConfig, SchedulerContext, SchedulerHandle, SchedulerRegistry,
    SchedulerRunner,
};
pub use store::{
    claim_job_by_id, claim_next_due_job, complete_job_run, create_scheduled_job,
    delete_scheduled_job, ensure_scheduler_schema, ensure_scheduler_schema_mysql_pool,
    ensure_scheduler_schema_sqlite_conn, get_scheduled_job_by_id, get_scheduled_job_by_kind_name,
    get_scheduled_job_run, heartbeat_job_lease, insert_job_run_start, list_job_runs,
    list_recent_runs, list_scheduled_jobs, pause_scheduled_job, resume_scheduled_job,
    scheduler_mysql_ddl, scheduler_sqlite_ddl, seed_default_job, seed_default_jobs,
    update_scheduled_job, SCHEDULER_TABLE_NAMES,
};

use crate::metering::{ExtensionUsageEvent, UsageDimensions};
use anyhow::{anyhow, Context, Result};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Generated `openestate.scheduler.v1` Protobuf types from `public/proto/openestate/scheduler/v1/scheduler.proto`.
pub use crate::extensions::proto::scheduler_v1 as proto;

/// Retry and exponential backoff configuration for a [`ScheduledJob`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_backoff")]
    pub initial_backoff_secs: u64,
    #[serde(default = "default_max_backoff")]
    pub max_backoff_secs: u64,
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    #[serde(default = "default_true")]
    pub retry_on_timeout: bool,
}

fn default_max_attempts() -> u32 {
    3
}
fn default_initial_backoff() -> u64 {
    30
}
fn default_max_backoff() -> u64 {
    3_600
}
fn default_backoff_multiplier() -> f64 {
    2.0
}
fn default_true() -> bool {
    true
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_secs: default_initial_backoff(),
            max_backoff_secs: default_max_backoff(),
            backoff_multiplier: default_backoff_multiplier(),
            retry_on_timeout: true,
        }
    }
}

impl RetryPolicy {
    pub fn normalized(&self) -> Self {
        let max_attempts = self.max_attempts.clamp(1, 50);
        let initial_backoff_secs = self.initial_backoff_secs.clamp(1, 86_400);
        let max_backoff_secs = self
            .max_backoff_secs
            .max(initial_backoff_secs)
            .min(7 * 86_400);
        let mult = if self.backoff_multiplier.is_finite() && self.backoff_multiplier >= 1.0 {
            self.backoff_multiplier.min(10.0)
        } else {
            2.0
        };
        Self {
            max_attempts,
            initial_backoff_secs,
            max_backoff_secs,
            backoff_multiplier: mult,
            retry_on_timeout: self.retry_on_timeout,
        }
    }

    /// Computes retry backoff delay in seconds for 1-based failed attempt index `failed_attempt`.
    pub fn backoff_secs_for_attempt(&self, failed_attempt: u32) -> i64 {
        let norm = self.normalized();
        let exp = failed_attempt.saturating_sub(1).min(20) as i32;
        let scaled = (norm.initial_backoff_secs as f64) * norm.backoff_multiplier.powi(exp);
        let clamped = if scaled.is_finite() {
            scaled.round().clamp(1.0, norm.max_backoff_secs as f64) as i64
        } else {
            norm.max_backoff_secs as i64
        };
        clamped.max(1)
    }

    pub fn to_proto(&self) -> proto::RetryPolicyProto {
        let n = self.normalized();
        proto::RetryPolicyProto {
            max_attempts: n.max_attempts,
            initial_backoff_secs: n.initial_backoff_secs,
            max_backoff_secs: n.max_backoff_secs,
            backoff_multiplier: n.backoff_multiplier,
            retry_on_timeout: n.retry_on_timeout,
        }
    }
}

/// MeterScope usage summary captured for one scheduled job execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledJobUsageSummary {
    pub invocation_id: String,
    pub outcome: String,
    pub billable: bool,
    pub dims: UsageDimensions,
    #[serde(default)]
    pub llm_model: Option<String>,
    #[serde(default)]
    pub llm_effort: Option<String>,
}

impl From<&ExtensionUsageEvent> for ScheduledJobUsageSummary {
    fn from(ev: &ExtensionUsageEvent) -> Self {
        Self {
            invocation_id: ev.invocation_id.clone(),
            outcome: ev.outcome.as_str().to_string(),
            billable: ev.billable,
            dims: ev.dims.clone(),
            llm_model: ev.llm_model.clone(),
            llm_effort: ev.llm_effort.clone(),
        }
    }
}

impl ScheduledJobUsageSummary {
    pub fn to_proto(&self) -> proto::ScheduledJobUsageSummaryProto {
        let d = &self.dims;
        proto::ScheduledJobUsageSummaryProto {
            invocations: d.invocations,
            invocations_ok: d.invocations_ok,
            invocations_failed: d.invocations_failed,
            invocations_cached: d.invocations_cached,
            invocations_denied: d.invocations_denied,
            wall_time_ms: d.wall_time_ms,
            llm_input_tokens: d.llm_input_tokens,
            llm_cached_input_tokens: d.llm_cached_input_tokens,
            llm_cache_write_tokens: d.llm_cache_write_tokens,
            llm_output_tokens: d.llm_output_tokens,
            llm_reasoning_tokens: d.llm_reasoning_tokens,
            llm_cost_micros: d.llm_cost_micros,
            browser_session_ms: d.browser_session_ms,
            browser_page_loads: d.browser_page_loads,
            emails_sent: d.emails_sent,
            emails_ingested: d.emails_ingested,
            documents_ingested: d.documents_ingested,
            pages_ocr: d.pages_ocr,
            bytes_downloaded: d.bytes_downloaded,
            bytes_stored: d.bytes_stored,
            external_api_calls: d.external_api_calls,
            external_api_cost_micros: d.external_api_cost_micros,
            llm_model: self.llm_model.clone(),
            llm_effort: self.llm_effort.clone(),
            outcome: self.outcome.clone(),
            billable: self.billable,
            invocation_id: self.invocation_id.clone(),
        }
    }

    pub fn from_proto(p: &proto::ScheduledJobUsageSummaryProto) -> Self {
        Self {
            invocation_id: p.invocation_id.clone(),
            outcome: p.outcome.clone(),
            billable: p.billable,
            dims: UsageDimensions {
                invocations: p.invocations,
                invocations_ok: p.invocations_ok,
                invocations_failed: p.invocations_failed,
                invocations_cached: p.invocations_cached,
                invocations_denied: p.invocations_denied,
                wall_time_ms: p.wall_time_ms,
                llm_input_tokens: p.llm_input_tokens,
                llm_cached_input_tokens: p.llm_cached_input_tokens,
                llm_cache_write_tokens: p.llm_cache_write_tokens,
                llm_output_tokens: p.llm_output_tokens,
                llm_reasoning_tokens: p.llm_reasoning_tokens,
                llm_cost_micros: p.llm_cost_micros,
                browser_session_ms: p.browser_session_ms,
                browser_page_loads: p.browser_page_loads,
                emails_sent: p.emails_sent,
                emails_ingested: p.emails_ingested,
                documents_ingested: p.documents_ingested,
                pages_ocr: p.pages_ocr,
                bytes_downloaded: p.bytes_downloaded,
                bytes_stored: p.bytes_stored,
                external_api_calls: p.external_api_calls,
                external_api_cost_micros: p.external_api_cost_micros,
            },
            llm_model: p.llm_model.clone(),
            llm_effort: p.llm_effort.clone(),
        }
    }
}

/// Persisted row in `scheduled_jobs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledJob {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub owner_extension_id: Option<String>,
    pub schedule_kind: ScheduleKind,
    pub cron_expr: Option<String>,
    pub interval_secs: Option<i64>,
    pub jitter_secs: i64,
    pub timezone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params_proto: Option<Vec<u8>>,
    pub params_json: String,
    pub enabled: bool,
    pub priority: i32,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub max_runtime_secs: i64,
    pub retry_policy: RetryPolicy,
    pub consecutive_failures: i64,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl ScheduledJob {
    pub fn schedule_spec(&self) -> ScheduleSpec {
        ScheduleSpec {
            kind: self.schedule_kind,
            cron_expr: self.cron_expr.clone(),
            interval_secs: self.interval_secs.and_then(|v| (v > 0).then_some(v as u64)),
            run_at_unix: if self.schedule_kind == ScheduleKind::Once {
                self.next_run_at
            } else {
                None
            },
            jitter_secs: self.jitter_secs.max(0) as u64,
            timezone: if self.timezone.trim().is_empty() {
                "UTC".to_string()
            } else {
                self.timezone.clone()
            },
        }
    }

    pub fn params_value(&self) -> Value {
        serde_json::from_str(&self.params_json).unwrap_or_else(|_| json!({}))
    }

    pub fn decode_params_proto(&self) -> Result<Option<proto::ScheduledJobParamsProto>> {
        match self.params_proto.as_deref() {
            Some(bytes) if !bytes.is_empty() => proto::ScheduledJobParamsProto::decode(bytes)
                .map(Some)
                .context("Failed to decode params_proto"),
            _ => Ok(None),
        }
    }

    pub fn to_proto(&self) -> proto::ScheduledJobProto {
        let sched_kind = match self.schedule_kind {
            ScheduleKind::Cron => proto::ScheduleKind::Cron as i32,
            ScheduleKind::Interval => proto::ScheduleKind::Interval as i32,
            ScheduleKind::Once => proto::ScheduleKind::Once as i32,
        };
        proto::ScheduledJobProto {
            id: self.id.clone(),
            kind: self.kind.clone(),
            name: self.name.clone(),
            owner_extension_id: self.owner_extension_id.clone(),
            schedule: Some(proto::ScheduleSpecProto {
                kind: sched_kind,
                cron_expr: self.cron_expr.clone(),
                interval_secs: self.interval_secs.and_then(|v| (v > 0).then_some(v as u64)),
                run_at_unix: if self.schedule_kind == ScheduleKind::Once {
                    self.next_run_at
                } else {
                    None
                },
                run_at_rfc3339: self.next_run_at.and_then(unix_to_rfc3339),
                jitter_secs: self.jitter_secs.max(0) as u64,
                timezone: self.timezone.clone(),
            }),
            params_proto: self.params_proto.clone().unwrap_or_default(),
            params_json: self.params_json.clone(),
            enabled: self.enabled,
            priority: self.priority,
            next_run_at_unix: self.next_run_at,
            next_run_at_rfc3339: self.next_run_at.and_then(unix_to_rfc3339),
            last_run_at_unix: self.last_run_at,
            last_run_at_rfc3339: self.last_run_at.and_then(unix_to_rfc3339),
            last_status: self.last_status.clone(),
            last_error: self.last_error.clone(),
            lease_owner: self.lease_owner.clone(),
            lease_expires_at_unix: self.lease_expires_at,
            max_runtime_secs: self.max_runtime_secs.max(1) as u64,
            retry_policy: Some(self.retry_policy.to_proto()),
            created_by: self.created_by.clone(),
            updated_by: self.updated_by.clone(),
            created_at_rfc3339: self.created_at.clone(),
            updated_at_rfc3339: self.updated_at.clone(),
        }
    }

    pub fn to_admin_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "name": self.name,
            "ownerExtensionId": self.owner_extension_id,
            "scheduleKind": self.schedule_kind.as_str(),
            "cronExpr": self.cron_expr,
            "intervalSecs": self.interval_secs,
            "jitterSecs": self.jitter_secs,
            "timezone": self.timezone,
            "params": self.params_value(),
            "paramsJson": self.params_json,
            "paramsProtoBytes": self.params_proto.as_ref().map(|b| b.len()).unwrap_or(0),
            "enabled": self.enabled,
            "priority": self.priority,
            "nextRunAtUnix": self.next_run_at,
            "nextRunAt": self.next_run_at.and_then(unix_to_rfc3339),
            "lastRunAtUnix": self.last_run_at,
            "lastRunAt": self.last_run_at.and_then(unix_to_rfc3339),
            "lastStatus": self.last_status,
            "lastError": self.last_error,
            "leaseOwner": self.lease_owner,
            "leaseExpiresAtUnix": self.lease_expires_at,
            "leaseExpiresAt": self.lease_expires_at.and_then(unix_to_rfc3339),
            "maxRuntimeSecs": self.max_runtime_secs,
            "retryPolicy": self.retry_policy,
            "consecutiveFailures": self.consecutive_failures,
            "createdBy": self.created_by,
            "updatedBy": self.updated_by,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

/// Persisted execution history row in `scheduled_job_runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledJobRun {
    pub run_id: String,
    pub job_id: String,
    pub job_kind: String,
    pub job_name: String,
    pub worker_id: Option<String>,
    pub started_at: String,
    pub started_at_unix: i64,
    pub finished_at: Option<String>,
    pub finished_at_unix: Option<i64>,
    pub status: String,
    pub attempt: i64,
    pub usage_summary_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_summary_proto: Option<Vec<u8>>,
    pub output_json: Option<String>,
    pub error: Option<String>,
}

impl ScheduledJobRun {
    pub fn usage_summary(&self) -> Option<ScheduledJobUsageSummary> {
        if let Some(ref raw) = self.usage_summary_json {
            if let Ok(s) = serde_json::from_str::<ScheduledJobUsageSummary>(raw) {
                return Some(s);
            }
        }
        if let Some(ref bytes) = self.usage_summary_proto {
            if let Ok(p) = proto::ScheduledJobUsageSummaryProto::decode(bytes.as_slice()) {
                return Some(ScheduledJobUsageSummary::from_proto(&p));
            }
        }
        None
    }

    pub fn to_admin_json(&self) -> Value {
        let usage = self
            .usage_summary_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        let output = self
            .output_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        json!({
            "runId": self.run_id,
            "jobId": self.job_id,
            "jobKind": self.job_kind,
            "jobName": self.job_name,
            "workerId": self.worker_id,
            "startedAt": self.started_at,
            "startedAtUnix": self.started_at_unix,
            "finishedAt": self.finished_at,
            "finishedAtUnix": self.finished_at_unix,
            "status": self.status,
            "attempt": self.attempt,
            "usageSummary": usage,
            "usageSummaryProtoBytes": self.usage_summary_proto.as_ref().map(|b| b.len()).unwrap_or(0),
            "output": output,
            "error": self.error,
        })
    }

    pub fn to_proto(&self) -> proto::ScheduledJobRunProto {
        let status = match self.status.as_str() {
            "running" => proto::ScheduledRunStatus::Running as i32,
            "succeeded" => proto::ScheduledRunStatus::Succeeded as i32,
            "failed" => proto::ScheduledRunStatus::Failed as i32,
            "timed_out" => proto::ScheduledRunStatus::TimedOut as i32,
            "cancelled" => proto::ScheduledRunStatus::Cancelled as i32,
            "skipped" => proto::ScheduledRunStatus::Skipped as i32,
            _ => proto::ScheduledRunStatus::Unspecified as i32,
        };
        proto::ScheduledJobRunProto {
            run_id: self.run_id.clone(),
            job_id: self.job_id.clone(),
            job_kind: self.job_kind.clone(),
            job_name: self.job_name.clone(),
            started_at_unix: self.started_at_unix,
            started_at_rfc3339: self.started_at.clone(),
            finished_at_unix: self.finished_at_unix,
            finished_at_rfc3339: self.finished_at.clone(),
            status,
            attempt: self.attempt.max(1) as u32,
            worker_id: self.worker_id.clone(),
            usage_summary: self.usage_summary().map(|u| u.to_proto()),
            usage_summary_json: self.usage_summary_json.clone(),
            error: self.error.clone(),
            output_json: self.output_json.clone(),
        }
    }
}

/// Idempotent seed definition for built-in or plugin background jobs.
#[derive(Debug, Clone)]
pub struct SeedJobSpec {
    pub kind: String,
    pub name: String,
    pub owner_extension_id: Option<String>,
    pub schedule: ScheduleSpec,
    pub params: Value,
    pub params_proto: Option<Vec<u8>>,
    pub enabled: bool,
    pub priority: i32,
    pub max_runtime_secs: u64,
    pub retry_policy: RetryPolicy,
    /// Optional startup delay (seconds from now) before first execution of a newly seeded job.
    pub initial_delay_secs: Option<i64>,
}

impl SeedJobSpec {
    pub fn new(kind: impl Into<String>, name: impl Into<String>, schedule: ScheduleSpec) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            owner_extension_id: None,
            schedule,
            params: json!({}),
            params_proto: None,
            enabled: true,
            priority: 0,
            max_runtime_secs: 300,
            retry_policy: RetryPolicy::default(),
            initial_delay_secs: None,
        }
    }

    pub fn with_owner_extension_id(mut self, ext_id: impl Into<String>) -> Self {
        let s = ext_id.into();
        if !s.trim().is_empty() {
            self.owner_extension_id = Some(s);
        }
        self
    }

    pub fn with_params(mut self, params: Value) -> Self {
        self.params = params;
        self
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_max_runtime_secs(mut self, secs: u64) -> Self {
        self.max_runtime_secs = secs.max(1);
        self
    }

    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    pub fn with_initial_delay_secs(mut self, secs: i64) -> Self {
        self.initial_delay_secs = Some(secs.max(0));
        self
    }
}

/// Input for creating a new job via API or programmatic call.
#[derive(Debug, Clone)]
pub struct CreateScheduledJobInput {
    pub kind: String,
    pub name: String,
    pub owner_extension_id: Option<String>,
    pub schedule: ScheduleSpec,
    pub params: Value,
    pub params_proto: Option<Vec<u8>>,
    pub enabled: bool,
    pub priority: i32,
    pub max_runtime_secs: u64,
    pub retry_policy: RetryPolicy,
    pub created_by: Option<String>,
}

/// Partial update patch for an existing scheduled job.
#[derive(Debug, Clone, Default)]
pub struct UpdateScheduledJobPatch {
    pub owner_extension_id: Option<Option<String>>,
    pub schedule_kind: Option<ScheduleKind>,
    pub cron_expr: Option<Option<String>>,
    pub interval_secs: Option<Option<u64>>,
    pub run_at_unix: Option<Option<i64>>,
    pub jitter_secs: Option<u64>,
    pub timezone: Option<String>,
    pub params: Option<Value>,
    pub params_proto: Option<Vec<u8>>,
    pub enabled: Option<bool>,
    pub priority: Option<i32>,
    pub max_runtime_secs: Option<u64>,
    pub retry_policy: Option<RetryPolicy>,
    pub next_run_at_override: Option<Option<i64>>,
    pub updated_by: Option<String>,
}

pub fn unix_to_rfc3339(ts: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).map(|dt| dt.to_rfc3339())
}

/// Encodes JSON `params` + optional custom binary bytes into a canonical `ScheduledJobParamsProto` wire buffer.
pub fn encode_job_params_proto(
    params: &Value,
    custom_bytes: Option<&[u8]>,
) -> Result<(String, Vec<u8>)> {
    let norm_val = if params.is_null() {
        json!({})
    } else {
        params.clone()
    };
    let payload_json = serde_json::to_string(&norm_val).context("Serialize job params JSON")?;
    if let Some(raw) = custom_bytes {
        if !raw.is_empty() {
            return Ok((payload_json, raw.to_vec()));
        }
    }
    let mut attributes = HashMap::new();
    if let Some(obj) = norm_val.as_object() {
        for (k, v) in obj {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Null => continue,
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            attributes.insert(k.clone(), s);
        }
    }
    let msg = proto::ScheduledJobParamsProto {
        proto_type: "openestate.scheduler.v1.ScheduledJobParamsProto".to_string(),
        payload_bytes: Vec::new(),
        payload_json: payload_json.clone(),
        attributes,
    };
    Ok((payload_json, msg.encode_to_vec()))
}

pub fn validate_job_ident(raw: &str, field: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 160
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    {
        return Err(anyhow!(
            "{field} must be 1..=160 alphanumeric, '_', '-', '.', or ':' characters"
        ));
    }
    Ok(trimmed.to_string())
}
