//! Multi-instance safe database scheduler runner with lease heartbeats, `max_runtime_secs`
//! enforcement, exponential-backoff retries, `MeterScope` attribution, and pluggable job-kind
//! handler registry (`register_job_kind`).

use crate::db::{metering_store, DbStore};
use crate::extensions::ExtensionHub;
use crate::metering::{mark_failed, InvocationContext, InvocationTrigger, MeterScope};
use crate::scheduler::store::{
    claim_job_by_id, claim_next_due_job, complete_job_run, get_scheduled_job_by_id,
    heartbeat_job_lease, insert_job_run_start,
};
use crate::scheduler::{ScheduledJob, ScheduledJobRun, ScheduledJobUsageSummary};
use crate::shared::{now_unix_secs, EnvConfig, HttpClient};
use crate::web::server::AppState;
use anyhow::Result;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;
use uuid::Uuid;

pub type SchedulerBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Structured output returned by a [`ScheduledJobHandler`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRunOutput {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub extension_id: Option<String>,
}

impl JobRunOutput {
    pub fn ok(summary: impl Into<String>, data: Value) -> Self {
        Self {
            summary: Some(summary.into()),
            data,
            extension_id: None,
        }
    }

    pub fn to_output_value(&self) -> Value {
        json!({
            "summary": self.summary,
            "data": self.data,
        })
    }
}

/// Execution context passed to each [`ScheduledJobHandler`].
#[derive(Clone)]
pub struct SchedulerContext {
    pub db: DbStore,
    pub http: HttpClient,
    pub env: EnvConfig,
    pub extension_hub: Arc<ExtensionHub>,
    pub app_state: Option<AppState>,
    pub job: ScheduledJob,
    pub run_id: String,
    pub attempt: u32,
}

impl SchedulerContext {
    pub fn params_value(&self) -> Value {
        self.job.params_value()
    }

    pub fn params_proto(&self) -> Result<Option<crate::scheduler::proto::ScheduledJobParamsProto>> {
        self.job.decode_params_proto()
    }

    pub fn param_str(&self, key: &str) -> Option<String> {
        self.params_value()
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn param_u64(&self, key: &str) -> Option<u64> {
        let v = self.params_value();
        v.get(key).and_then(Value::as_u64).or_else(|| {
            v.get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.trim().parse().ok())
        })
    }

    pub fn param_i64(&self, key: &str) -> Option<i64> {
        let v = self.params_value();
        v.get(key).and_then(Value::as_i64).or_else(|| {
            v.get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.trim().parse().ok())
        })
    }

    pub fn param_bool(&self, key: &str) -> Option<bool> {
        let v = self.params_value();
        v.get(key).and_then(Value::as_bool).or_else(|| {
            v.get(key).and_then(Value::as_str).and_then(|s| {
                match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "on" => Some(true),
                    "false" | "0" | "no" | "off" => Some(false),
                    _ => None,
                }
            })
        })
    }
}

/// Async handler trait implemented by job kinds registered via [`register_job_kind`].
pub trait ScheduledJobHandler: Send + Sync + 'static {
    fn run<'a>(&'a self, ctx: SchedulerContext) -> SchedulerBoxFuture<'a, Result<JobRunOutput>>;
}

struct ClosureJobHandler<F>(F);

impl<F, Fut> ScheduledJobHandler for ClosureJobHandler<F>
where
    F: Fn(SchedulerContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<JobRunOutput>> + Send + 'static,
{
    fn run<'a>(&'a self, ctx: SchedulerContext) -> SchedulerBoxFuture<'a, Result<JobRunOutput>> {
        Box::pin((self.0)(ctx))
    }
}

static GLOBAL_HANDLER_MAP: Lazy<DashMap<String, Arc<dyn ScheduledJobHandler>>> =
    Lazy::new(DashMap::new);
static GLOBAL_SCHEDULER_REGISTRY: Lazy<SchedulerRegistry> =
    Lazy::new(SchedulerRegistry::global_backing);
static ACTIVE_SCHEDULER_WAKE: Lazy<RwLock<Option<Arc<Notify>>>> = Lazy::new(|| RwLock::new(None));
static DETACHED_SCHEDULER_HANDLES: Lazy<Mutex<Vec<SchedulerHandle>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Clone)]
enum RegistryBacking {
    Global,
    Local(Arc<DashMap<String, Arc<dyn ScheduledJobHandler>>>),
}

/// Thread-safe registry mapping `scheduled_jobs.kind` -> [`ScheduledJobHandler`].
#[derive(Clone)]
pub struct SchedulerRegistry {
    backing: RegistryBacking,
}

impl Default for SchedulerRegistry {
    fn default() -> Self {
        Self::global_backing()
    }
}

impl SchedulerRegistry {
    fn global_backing() -> Self {
        Self {
            backing: RegistryBacking::Global,
        }
    }

    /// Creates an isolated in-memory handler registry (useful for multi-runner tests).
    pub fn new() -> Self {
        Self {
            backing: RegistryBacking::Local(Arc::new(DashMap::new())),
        }
    }

    fn map(&self) -> &DashMap<String, Arc<dyn ScheduledJobHandler>> {
        match &self.backing {
            RegistryBacking::Global => &GLOBAL_HANDLER_MAP,
            RegistryBacking::Local(m) => m,
        }
    }

    pub fn register(&self, kind: &str, handler: Arc<dyn ScheduledJobHandler>) {
        let key = kind.trim().to_string();
        if !key.is_empty() {
            self.map().insert(key, handler);
        }
    }

    pub fn register_fn<F, Fut>(&self, kind: &str, f: F)
    where
        F: Fn(SchedulerContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<JobRunOutput>> + Send + 'static,
    {
        self.register(kind, Arc::new(ClosureJobHandler(f)));
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn ScheduledJobHandler>> {
        self.map().get(kind.trim()).map(|e| Arc::clone(e.value()))
    }

    pub fn kinds(&self) -> Vec<String> {
        let mut list: Vec<String> = self.map().iter().map(|e| e.key().clone()).collect();
        list.sort();
        list
    }
}

/// Returns the shared global handler registry used by the server runner and Admin API.
pub fn global_handler_registry() -> &'static SchedulerRegistry {
    &GLOBAL_SCHEDULER_REGISTRY
}

/// Registers a [`ScheduledJobHandler`] for `kind` on the global registry (usable from `public` and `cloud`).
pub fn register_job_kind(kind: &str, handler: Arc<dyn ScheduledJobHandler>) {
    GLOBAL_SCHEDULER_REGISTRY.register(kind, handler);
}

/// Convenience helper registering an async closure/function as the handler for `kind`.
pub fn register_job_kind_fn<F, Fut>(kind: &str, f: F)
where
    F: Fn(SchedulerContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<JobRunOutput>> + Send + 'static,
{
    GLOBAL_SCHEDULER_REGISTRY.register_fn(kind, f);
}

/// Lists all registered handler kind names on the global registry.
pub fn registered_job_kinds() -> Vec<String> {
    GLOBAL_SCHEDULER_REGISTRY.kinds()
}

/// Wakes the active background scheduler loop (if spawned) to pick up newly created/resumed/`run-now` jobs.
pub fn wake_active_scheduler() {
    if let Ok(guard) = ACTIVE_SCHEDULER_WAKE.read() {
        if let Some(ref notify) = *guard {
            notify.notify_one();
        }
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub worker_id: String,
    pub poll_interval: Duration,
    pub max_concurrency: usize,
    pub lease_secs: i64,
    pub heartbeat_interval: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            worker_id: format!("sched_{}", &Uuid::new_v4().simple().to_string()[..12]),
            poll_interval: Duration::from_secs(2),
            max_concurrency: 8,
            lease_secs: 60,
            heartbeat_interval: Duration::from_secs(15),
        }
    }
}

impl SchedulerConfig {
    pub fn from_env(env: &EnvConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(w) = env
            .get("SCHEDULER_WORKER_ID")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            cfg.worker_id = w.to_string();
        }
        if let Some(ms) = env
            .get("SCHEDULER_POLL_MS")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&v| v >= 50)
        {
            cfg.poll_interval = Duration::from_millis(ms.min(60_000));
        }
        if let Some(conc) = env
            .get("SCHEDULER_MAX_CONCURRENCY")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&v| v >= 1)
        {
            cfg.max_concurrency = conc.min(64);
        }
        if let Some(lease) = env
            .get("SCHEDULER_LEASE_SECS")
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|&v| v >= 5)
        {
            cfg.lease_secs = lease.min(3_600);
        }
        let hb_secs = (cfg.lease_secs as u64 / 3).clamp(1, 30);
        cfg.heartbeat_interval = Duration::from_secs(hb_secs);
        cfg
    }
}

#[derive(Debug, Clone)]
pub enum RunNowError {
    NotFound,
    Paused,
    AlreadyRunning,
    Internal(String),
}

/// Multi-instance safe database scheduler runner.
#[derive(Clone)]
pub struct SchedulerRunner {
    pub db: DbStore,
    pub http: HttpClient,
    pub env: EnvConfig,
    pub extension_hub: Arc<ExtensionHub>,
    pub app_state: Option<AppState>,
    pub registry: SchedulerRegistry,
    pub config: SchedulerConfig,
}

impl SchedulerRunner {
    pub fn new(
        db: DbStore,
        http: HttpClient,
        env: EnvConfig,
        extension_hub: Arc<ExtensionHub>,
        config: SchedulerConfig,
    ) -> Self {
        Self {
            db,
            http,
            env,
            extension_hub,
            app_state: None,
            registry: SchedulerRegistry::global_backing(),
            config,
        }
    }

    pub fn from_app_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            http: state.http.clone(),
            env: state.env.clone(),
            extension_hub: Arc::clone(&state.extension_hub),
            app_state: Some(state.clone()),
            registry: SchedulerRegistry::global_backing(),
            config: SchedulerConfig::from_env(&state.env),
        }
    }

    pub fn with_app_state(mut self, state: AppState) -> Self {
        self.app_state = Some(state);
        self
    }

    pub fn with_registry(mut self, registry: SchedulerRegistry) -> Self {
        self.registry = registry;
        self
    }

    pub fn with_config(mut self, config: SchedulerConfig) -> Self {
        self.config = config;
        self
    }

    /// Claims and executes one due job at `now_unix`, returning the updated job and run record.
    pub async fn process_one_at(
        &self,
        now_unix: i64,
    ) -> Result<Option<(Option<ScheduledJob>, Option<ScheduledJobRun>)>> {
        let Some(job) = claim_next_due_job(
            &self.db,
            &self.config.worker_id,
            self.config.lease_secs,
            now_unix,
        )
        .await?
        else {
            return Ok(None);
        };
        let res = self.execute_claimed_job(job).await?;
        Ok(Some(res))
    }

    /// Claims and executes one due job at the current timestamp.
    pub async fn process_one(
        &self,
    ) -> Result<Option<(Option<ScheduledJob>, Option<ScheduledJobRun>)>> {
        self.process_one_at(now_unix_secs()).await
    }

    /// Drains up to `max_jobs` due jobs sequentially.
    pub async fn drain_due_jobs(&self, max_jobs: usize) -> Result<usize> {
        let mut count = 0usize;
        while count < max_jobs {
            if self.process_one().await?.is_none() {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Immediately claims and runs a specific `job_id` (used by Admin `run-now`).
    pub async fn run_job_now(
        &self,
        job_id: &str,
        allow_paused: bool,
    ) -> std::result::Result<(Option<ScheduledJob>, Option<ScheduledJobRun>), RunNowError> {
        let existing = get_scheduled_job_by_id(&self.db, job_id)
            .await
            .map_err(|e| RunNowError::Internal(e.to_string()))?
            .ok_or(RunNowError::NotFound)?;

        if !existing.enabled && !allow_paused {
            return Err(RunNowError::Paused);
        }

        let claimed = claim_job_by_id(
            &self.db,
            job_id,
            &self.config.worker_id,
            self.config.lease_secs,
            now_unix_secs(),
            allow_paused,
        )
        .await
        .map_err(|e| RunNowError::Internal(e.to_string()))?
        .ok_or(RunNowError::AlreadyRunning)?;

        self.execute_claimed_job(claimed)
            .await
            .map_err(|e| RunNowError::Internal(e.to_string()))
    }

    /// Executes an already-leased job with heartbeat, `max_runtime_secs` timeout, `MeterScope`,
    /// run history persistence, and retry/next-run advancement.
    pub async fn execute_claimed_job(
        &self,
        job: ScheduledJob,
    ) -> Result<(Option<ScheduledJob>, Option<ScheduledJobRun>)> {
        let attempt = (job.consecutive_failures.max(0) as u32) + 1;
        let run_row = insert_job_run_start(&self.db, &job, &self.config.worker_id, attempt).await?;

        let hb_db = self.db.clone();
        let hb_job_id = job.id.clone();
        let hb_worker = self.config.worker_id.clone();
        let hb_lease = self.config.lease_secs;
        let hb_interval = self.config.heartbeat_interval;
        let (stop_tx, mut stop_rx) = watch::channel(false);

        let hb_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(hb_interval);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        if let Ok(false) = heartbeat_job_lease(&hb_db, &hb_job_id, &hb_worker, hb_lease).await {
                            break;
                        }
                    }
                }
            }
        });

        let ext_id = job
            .owner_extension_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("scheduler:{}", job.kind));
        let inv_ctx = InvocationContext::new(ext_id)
            .with_trigger(InvocationTrigger::PeriodicScan)
            .with_billable(false);

        let handler_opt = self.registry.get(&job.kind);
        let sched_ctx = SchedulerContext {
            db: self.db.clone(),
            http: self.http.clone(),
            env: self.env.clone(),
            extension_hub: Arc::clone(&self.extension_hub),
            app_state: self.app_state.clone(),
            job: job.clone(),
            run_id: run_row.run_id.clone(),
            attempt,
        };
        let max_rt = Duration::from_secs(job.max_runtime_secs.max(1) as u64);
        let app_state_opt = self.app_state.clone();

        let (exec_outcome, usage_event) = MeterScope::run(inv_ctx, async move {
            let Some(handler) = handler_opt else {
                mark_failed();
                return (
                    "failed",
                    None,
                    Some(format!(
                        "No handler registered for job kind '{}'",
                        sched_ctx.job.kind
                    )),
                );
            };
            let run_fut = handler.run(sched_ctx);
            let timed = match app_state_opt {
                Some(st) => {
                    ExtensionHub::with_hook_state(st, tokio::time::timeout(max_rt, run_fut)).await
                }
                None => tokio::time::timeout(max_rt, run_fut).await,
            };
            match timed {
                Ok(Ok(out)) => ("succeeded", Some(out.to_output_value()), None),
                Ok(Err(err)) => {
                    mark_failed();
                    ("failed", None, Some(err.to_string()))
                }
                Err(_) => {
                    mark_failed();
                    (
                        "timed_out",
                        None,
                        Some(format!(
                            "Scheduled job timed out after {}s",
                            max_rt.as_secs()
                        )),
                    )
                }
            }
        })
        .await;

        let _ = stop_tx.send(true);
        let _ = hb_task.await;

        let (status_str, output_val, err_opt) = exec_outcome;
        let usage_summary = ScheduledJobUsageSummary::from(&usage_event);

        // If no child ExtensionHub execution recorded its own metering rollup inside this scope,
        // persist the job-level MeterScope rollup directly.
        if usage_event.dims.invocations <= 1 {
            let _ = metering_store::record_local_usage_rollup(&self.db, &usage_event).await;
        }

        complete_job_run(
            &self.db,
            &job.id,
            &run_row.run_id,
            &self.config.worker_id,
            status_str,
            &usage_summary,
            output_val.as_ref(),
            err_opt.as_deref(),
        )
        .await
    }

    /// Spawns a bounded-concurrency background scheduler loop polling `scheduled_jobs`.
    pub fn spawn(self) -> SchedulerHandle {
        let wake = Arc::new(Notify::new());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let wake_loop = Arc::clone(&wake);
        if let Ok(mut guard) = ACTIVE_SCHEDULER_WAKE.write() {
            *guard = Some(Arc::clone(&wake));
        }

        let join_handle = tokio::spawn(async move {
            let concurrency = self.config.max_concurrency.max(1);
            let sem = Arc::new(Semaphore::new(concurrency));
            let mut interval = tokio::time::interval(self.config.poll_interval);

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

                loop {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    let Ok(permit) = Arc::clone(&sem).try_acquire_owned() else {
                        break;
                    };
                    match claim_next_due_job(
                        &self.db,
                        &self.config.worker_id,
                        self.config.lease_secs,
                        now_unix_secs(),
                    )
                    .await
                    {
                        Ok(Some(job)) => {
                            let runner = self.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                if let Err(err) = runner.execute_claimed_job(job).await {
                                    tracing::warn!(error = %err, "Scheduled job execution error");
                                }
                            });
                        }
                        Ok(None) => {
                            drop(permit);
                            break;
                        }
                        Err(err) => {
                            drop(permit);
                            tracing::warn!(error = %err, "Scheduler claim_next_due_job error");
                            break;
                        }
                    }
                }

                tokio::select! {
                    res = shutdown_rx.changed() => {
                        if res.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = wake_loop.notified() => {}
                    _ = interval.tick() => {}
                }
            }
        });

        SchedulerHandle {
            wake,
            shutdown_tx,
            join_handle: Some(join_handle),
        }
    }
}

/// Spawns [`SchedulerRunner`] using the global handler registry.
pub fn spawn_scheduler_runner(runner: SchedulerRunner) -> SchedulerHandle {
    runner.spawn()
}

/// Handle controlling a spawned background [`SchedulerRunner`] loop.
pub struct SchedulerHandle {
    wake: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    join_handle: Option<JoinHandle<()>>,
}

impl SchedulerHandle {
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Keeps this scheduler task running for the lifetime of the server process.
    pub fn detach(self) {
        if let Ok(mut guard) = DETACHED_SCHEDULER_HANDLES.lock() {
            guard.push(self);
        }
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        self.wake.notify_waiters();
        if let Some(jh) = self.join_handle.take() {
            let _ = jh.await;
        }
    }
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.wake.notify_waiters();
    }
}
