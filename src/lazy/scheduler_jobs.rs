//! DB-backed scheduled job handlers for periodic lazy report sweeps (`scanner_periodic_lazy_sweep`)
//! and lazy worker recovery polling (`lazy_worker_poll`), integrated with [`crate::scheduler`].

use crate::db::DbStore;
use crate::lazy::executor::{LazyExecutor, LazyWorker, LazyWorkerConfig};
use crate::scanner::lazy_hook::{
    enqueue_periodic_lazy_sweep_from_cached_reports, PeriodicLazyExtensionSpec,
};
use crate::scheduler::{
    register_job_kind_fn, seed_default_jobs, JobRunOutput, RetryPolicy, ScheduleKind, ScheduleSpec,
    SchedulerContext, SeedJobSpec,
};
use crate::shared::EnvConfig;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::{Arc, Once};

pub const SCANNER_PERIODIC_LAZY_SWEEP_JOB_KIND: &str = "scanner_periodic_lazy_sweep";
pub const LAZY_WORKER_POLL_JOB_KIND: &str = "lazy_worker_poll";

static LAZY_SCHED_HANDLERS_ONCE: Once = Once::new();

/// Registers the lazy extension / scanner periodic sweep handlers on the global scheduler registry.
pub fn register_lazy_scheduled_jobs() {
    LAZY_SCHED_HANDLERS_ONCE.call_once(|| {
        register_job_kind_fn(
            SCANNER_PERIODIC_LAZY_SWEEP_JOB_KIND,
            handle_scanner_periodic_lazy_sweep,
        );
        register_job_kind_fn(LAZY_WORKER_POLL_JOB_KIND, handle_lazy_worker_poll);
    });
}

fn resolve_sweep_specs(ctx: &SchedulerContext) -> Vec<PeriodicLazyExtensionSpec> {
    let params = ctx.params_value();
    if let Some(arr) = params.get("specs").and_then(Value::as_array) {
        let mut custom = Vec::new();
        for item in arr {
            let ext_id = item
                .get("extensionId")
                .or_else(|| item.get("extension_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let sec_id = item
                .get("sectionId")
                .or_else(|| item.get("section_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("publicResearch");
            if let Some(eid) = ext_id {
                custom.push(PeriodicLazyExtensionSpec::new(eid, sec_id));
            }
        }
        if !custom.is_empty() {
            return custom;
        }
    }
    PeriodicLazyExtensionSpec::discover_from_hub(&ctx.extension_hub, Some(&ctx.env))
}

async fn handle_scanner_periodic_lazy_sweep(ctx: SchedulerContext) -> Result<JobRunOutput> {
    Box::pin(async move {
        let specs = resolve_sweep_specs(&ctx);
        let limit = ctx.param_i64("limit").unwrap_or(200).clamp(1, 500);
        if specs.is_empty() {
            return Ok(JobRunOutput::ok(
                "Periodic lazy sweep skipped (no lazy extension specs configured)",
                json!({ "targetsExamined": 0, "queued": 0, "specs": 0 }),
            ));
        }

        let summary = enqueue_periodic_lazy_sweep_from_cached_reports(
            &ctx.db,
            Some(&ctx.extension_hub),
            &specs,
            limit,
        )
        .await?;

        Ok(JobRunOutput::ok(
            format!(
                "Swept {} cached reports ({} queued, {} deduplicated)",
                summary.targets_examined, summary.queued, summary.deduplicated
            ),
            serde_json::to_value(&summary).unwrap_or_else(|_| json!({})),
        ))
    })
    .await
}

async fn handle_lazy_worker_poll(ctx: SchedulerContext) -> Result<JobRunOutput> {
    Box::pin(async move {
        let max_jobs = ctx.param_u64("maxJobs").unwrap_or(32).clamp(1, 500) as usize;
        let mut executor = LazyExecutor::new(
            ctx.db.clone(),
            ctx.http.clone(),
            ctx.env.clone(),
            Arc::clone(&ctx.extension_hub),
        );
        if let Some(ref state) = ctx.app_state {
            executor = executor.with_hook_state(state.clone());
        }
        let mut worker_cfg = LazyWorkerConfig::from_env(&ctx.env);
        worker_cfg.worker_id = format!("sched-lazy-{}", &ctx.run_id);
        let worker = LazyWorker::new(executor, worker_cfg);
        let processed = Box::pin(worker.drain_due_jobs(max_jobs)).await?;

        Ok(JobRunOutput::ok(
            format!("Lazy worker poll processed {} due jobs", processed),
            json!({
                "processed": processed,
                "maxJobs": max_jobs,
            }),
        ))
    })
    .await
}

/// Idempotently seeds default `scanner_periodic_lazy_sweep` and `lazy_worker_poll` DB schedules
/// using environment variables (`LAZY_SCAN_SWEEP_INTERVAL_SECS`, `LAZY_WORKER_POLL_SECS`) ONLY
/// for initial seed creation — never overwriting operator edits in `scheduled_jobs`.
pub async fn seed_lazy_default_jobs(db: &DbStore, env: &EnvConfig) -> Result<usize> {
    register_lazy_scheduled_jobs();

    let sweep_interval = env
        .get("LAZY_SCAN_SWEEP_INTERVAL_SECS")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(900)
        .clamp(60, 86_400);

    let poll_interval = env
        .get("LAZY_WORKER_POLL_SECS")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(30)
        .clamp(5, 3_600);

    let seeds = vec![
        SeedJobSpec::new(
            SCANNER_PERIODIC_LAZY_SWEEP_JOB_KIND,
            "default",
            ScheduleSpec {
                kind: ScheduleKind::Interval,
                cron_expr: None,
                interval_secs: Some(sweep_interval),
                run_at_unix: None,
                jitter_secs: 30,
                timezone: "UTC".to_string(),
            },
        )
        .with_params(json!({ "limit": 200 }))
        .with_initial_delay_secs(60)
        .with_max_runtime_secs(300)
        .with_retry_policy(RetryPolicy {
            max_attempts: 2,
            initial_backoff_secs: 30,
            max_backoff_secs: 300,
            backoff_multiplier: 2.0,
            retry_on_timeout: true,
        }),
        SeedJobSpec::new(
            LAZY_WORKER_POLL_JOB_KIND,
            "default",
            ScheduleSpec {
                kind: ScheduleKind::Interval,
                cron_expr: None,
                interval_secs: Some(poll_interval),
                run_at_unix: None,
                jitter_secs: 5,
                timezone: "UTC".to_string(),
            },
        )
        .with_params(json!({ "maxJobs": 32 }))
        .with_initial_delay_secs(10)
        .with_max_runtime_secs(600)
        .with_retry_policy(RetryPolicy {
            max_attempts: 2,
            initial_backoff_secs: 10,
            max_backoff_secs: 120,
            backoff_multiplier: 2.0,
            retry_on_timeout: true,
        }),
    ];

    seed_default_jobs(db, &seeds).await
}
