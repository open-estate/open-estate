//! `LazyExecutor` (real `ExtensionHub` execution under `MeterScope`) and spawnable
//! background `LazyWorker` loop with lease heartbeats and graceful shutdown.

use crate::db::{DbStore, LazyExtensionJobRow};
use crate::extensions::{
    to_report_json_patch, AggregatedPropertyBundleProto, ExtensionHub, ExtensionQueryProto,
};
use crate::lazy::runtime::{
    claim_next, complete, fail, heartbeat, mark_awaiting_external_response,
    resolve_extension_policy,
};
use crate::lazy::types::{CompleteLazyJobOutcome, LazyExtensionPolicy, SectionNotificationOutcome};
use crate::metering::{InvocationContext, InvocationTrigger, MeterScope};
use crate::shared::{now_unix_secs, EnvConfig, HttpClient};
use crate::web::hooks::{NoopExtensionHooks, ServerExtensionHooks};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum LazyExecResult {
    Completed(Box<CompleteLazyJobOutcome>),
    AwaitingExternalResponse,
    PolicySkipped(&'static str),
    RetriedOrFailed(Option<LazyExtensionJobRow>),
}

/// Executes queued lazy extension jobs against the live [`ExtensionHub`].
#[derive(Clone)]
pub struct LazyExecutor {
    pub db: DbStore,
    pub http: HttpClient,
    pub env: EnvConfig,
    pub hub: Arc<ExtensionHub>,
    pub hooks: Arc<dyn ServerExtensionHooks>,
    pub hook_state: Option<crate::web::server::AppState>,
    pub base_origin: String,
    pub base_backoff_secs: i64,
    pub max_backoff_secs: i64,
}

impl LazyExecutor {
    pub fn new(db: DbStore, http: HttpClient, env: EnvConfig, hub: Arc<ExtensionHub>) -> Self {
        let base_origin = env.get("PUBLIC_BASE_URL").unwrap_or("").trim().to_string();
        Self {
            db,
            http,
            env,
            hub,
            hooks: Arc::new(NoopExtensionHooks),
            hook_state: None,
            base_origin,
            base_backoff_secs: 15,
            max_backoff_secs: 3_600,
        }
    }

    pub fn with_hooks(mut self, hooks: Arc<dyn ServerExtensionHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_hook_state(mut self, state: crate::web::server::AppState) -> Self {
        self.hooks = Arc::clone(&state.hooks);
        self.hook_state = Some(state);
        self
    }

    pub fn with_base_origin(mut self, base_origin: impl Into<String>) -> Self {
        self.base_origin = base_origin.into();
        self
    }

    pub fn with_backoff(mut self, base_secs: i64, max_secs: i64) -> Self {
        self.base_backoff_secs = base_secs.max(1);
        self.max_backoff_secs = max_secs.max(self.base_backoff_secs);
        self
    }

    /// Builds an [`ExtensionQueryProto`] from `job.query_json` enriched with any
    /// cached report facts/geo for `job.slug`.
    pub async fn build_query_for_job(&self, job: &LazyExtensionJobRow) -> ExtensionQueryProto {
        let q_val: Value = job
            .query_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Null);

        let cached_rep = self
            .db
            .get_cached_report(&job.slug)
            .await
            .ok()
            .flatten()
            .map(|r| r.report);

        let geo_val = cached_rep
            .as_ref()
            .and_then(|r| r.get("geo"))
            .unwrap_or(&Value::Null);
        let facts_val = cached_rep
            .as_ref()
            .and_then(|r| r.get("facts"))
            .unwrap_or(&Value::Null);
        let meta_val = cached_rep
            .as_ref()
            .and_then(|r| r.get("meta"))
            .unwrap_or(&Value::Null);

        let pick_str = |k1: &str, k2: &str| -> String {
            q_val
                .get(k1)
                .or_else(|| q_val.get(k2))
                .and_then(Value::as_str)
                .or_else(|| geo_val.get(k1).and_then(Value::as_str))
                .or_else(|| geo_val.get(k2).and_then(Value::as_str))
                .or_else(|| facts_val.get(k1).and_then(Value::as_str))
                .or_else(|| meta_val.get(k1).and_then(Value::as_str))
                .unwrap_or("")
                .trim()
                .to_string()
        };

        let pick_opt_str = |k1: &str, k2: &str| -> Option<String> {
            let s = pick_str(k1, k2);
            (!s.is_empty()).then_some(s)
        };

        let pick_f64 = |k: &str| -> Option<f64> {
            q_val
                .get(k)
                .and_then(Value::as_f64)
                .or_else(|| geo_val.get(k).and_then(Value::as_f64))
                .filter(|v| v.is_finite())
        };

        let pick_i64 = |k1: &str, k2: &str| -> Option<i64> {
            q_val
                .get(k1)
                .or_else(|| q_val.get(k2))
                .and_then(Value::as_i64)
                .or_else(|| facts_val.get(k1).and_then(Value::as_i64))
                .or_else(|| facts_val.get(k2).and_then(Value::as_i64))
        };

        let pick_opt_bool = |k1: &str, k2: &str| -> Option<bool> {
            q_val
                .get(k1)
                .or_else(|| q_val.get(k2))
                .and_then(Value::as_bool)
                .or_else(|| facts_val.get(k1).and_then(Value::as_bool))
                .or_else(|| facts_val.get(k2).and_then(Value::as_bool))
        };

        let pick_opt_f64 = |k1: &str, k2: &str| -> Option<f64> {
            q_val
                .get(k1)
                .or_else(|| q_val.get(k2))
                .and_then(Value::as_f64)
                .or_else(|| facts_val.get(k1).and_then(Value::as_f64))
                .or_else(|| facts_val.get(k2).and_then(Value::as_f64))
                .filter(|v| v.is_finite())
        };

        let mut extra_params: HashMap<String, String> = HashMap::new();
        if let Some(ep) = q_val
            .get("extraParams")
            .or_else(|| q_val.get("extra_params"))
            .and_then(Value::as_object)
        {
            for (k, v) in ep {
                if let Some(s) = v.as_str() {
                    extra_params.insert(k.clone(), s.to_string());
                } else if v.is_number() || v.is_boolean() {
                    extra_params.insert(k.clone(), v.to_string());
                }
            }
        }
        extra_params.insert("slug".to_string(), job.slug.clone());
        extra_params.insert("section_id".to_string(), job.section_id.clone());
        extra_params.insert("lazy_job_id".to_string(), job.id.clone());
        extra_params.insert("lazy_trigger_kind".to_string(), job.trigger_kind.clone());
        if let Some(ref uid) = job.requested_by_user_id {
            extra_params.insert("requested_by_user_id".to_string(), uid.clone());
        }
        if !extra_params.contains_key("mls_remarks") {
            let mut remarks_parts: Vec<String> = Vec::new();
            for key in ["mlsRemarks", "privateRemarks", "publicRemarks", "description"] {
                if let Some(txt) = facts_val
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    remarks_parts.push(txt.to_string());
                }
            }
            if let Ok(Some(sk)) = self.db.resolve_listing_sk(&job.slug).await {
                if let Ok(Some(desc)) = self.db.get_listing_descriptions(sk).await {
                    if let Some(txt) = desc
                        .mls_remarks
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        if !remarks_parts.iter().any(|p| p == txt) {
                            remarks_parts.push(txt.to_string());
                        }
                    }
                }
            }
            if !remarks_parts.is_empty() {
                extra_params.insert("mls_remarks".to_string(), remarks_parts.join("\n"));
            }
        }

        let address = {
            let a = pick_str("address", "displayAddress");
            if a.is_empty() {
                pick_str("matchedAddress", "input")
            } else {
                a
            }
        };

        ExtensionQueryProto {
            address,
            street: pick_str("street", "streetAddress"),
            city: pick_str("city", "place"),
            state: pick_str("state", "stateCode"),
            zip: pick_str("zip", "zipCode"),
            county_fips: pick_str("countyFips", "county_fips"),
            lat: pick_f64("lat"),
            lon: pick_f64("lon"),
            extra_params,
            county_name: pick_opt_str("countyName", "county_name"),
            sqft: pick_i64("sqft", "livingAreaSqft"),
            year_built: pick_i64("yearBuilt", "year_built"),
            is_condo_or_attached: pick_opt_bool("isCondoOrAttached", "is_condo_or_attached"),
            hoa_covers_water_trash: pick_opt_bool("hoaCoversWaterTrash", "hoa_covers_water_trash"),
            is_likely_private_well: pick_opt_bool("isLikelyPrivateWell", "is_likely_private_well"),
            is_likely_private_septic: pick_opt_bool(
                "isLikelyPrivateSeptic",
                "is_likely_private_septic",
            ),
            sdwis_water_name: pick_opt_str("sdwisWaterName", "sdwis_water_name"),
            sdwis_pwsid: pick_opt_str("sdwisPwsid", "sdwis_pwsid"),
            gov_kwh_benchmark: pick_opt_f64("govKwhBenchmark", "gov_kwh_benchmark"),
            has_solar: pick_opt_bool("hasSolar", "has_solar"),
            acs_broadband_pct: pick_opt_f64("acsBroadbandPct", "acs_broadband_pct"),
            is_rural_area: pick_opt_bool("isRuralArea", "is_rural_area"),
            apn: pick_opt_str("apn", "parcelApn"),
            parent_address: pick_opt_str("parentAddress", "parent_address"),
            upstream_envelopes: Vec::new(),
        }
    }

    /// Executes a claimed [`LazyExtensionJobRow`] via `ExtensionHub` inside `MeterScope`.
    pub async fn execute_claimed_job(&self, job: &LazyExtensionJobRow) -> Result<LazyExecResult> {
        Box::pin(self.execute_claimed_job_inner(job)).await
    }

    async fn execute_claimed_job_inner(&self, job: &LazyExtensionJobRow) -> Result<LazyExecResult> {
        let policy = resolve_extension_policy(
            Some(&self.hub),
            &job.extension_id,
            &LazyExtensionPolicy::default(),
        );
        if !policy.enabled {
            let _ = self
                .db
                .record_lazy_job_policy_state(
                    &job.slug,
                    &job.extension_id,
                    &job.section_id,
                    "not_enabled",
                    job.requested_by_user_id.as_deref(),
                    &job.trigger_kind,
                    Some("Extension disabled before execution"),
                )
                .await;
            return Ok(LazyExecResult::PolicySkipped("not_enabled"));
        }

        if policy.requires_manual_approval && job.trigger_kind != "approved" {
            let _ = self
                .db
                .record_lazy_job_policy_state(
                    &job.slug,
                    &job.extension_id,
                    &job.section_id,
                    "awaiting_approval",
                    job.requested_by_user_id.as_deref(),
                    &job.trigger_kind,
                    Some("Extension requires manual approval"),
                )
                .await;
            return Ok(LazyExecResult::PolicySkipped("awaiting_approval"));
        }

        let query = Box::pin(self.build_query_for_job(job)).await;
        let lazy_trigger = match job.trigger_kind.trim().to_ascii_lowercase().as_str() {
            "periodic_scan" | "periodicscan" | "scan" => InvocationTrigger::PeriodicScan,
            "admin_test" | "admintest" | "test_fetch" => InvocationTrigger::AdminTest,
            "dependency" | "dep" => InvocationTrigger::Dependency,
            _ => InvocationTrigger::LazyJob,
        };
        let mut meter_ctx = InvocationContext::new(&job.extension_id)
            .with_trigger(lazy_trigger)
            .with_slug(&job.slug);
        if let Some(uid) = job
            .requested_by_user_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            meter_ctx = meter_ctx.with_actor_user_id(uid);
        }

        let (fetch_res, _usage_event) = Box::pin(MeterScope::run_result(meter_ctx, async {
            let fut =
                Box::pin(
                    self.hub
                        .test_fetch(&job.extension_id, &query, &self.http, &self.env),
                );
            let res = if let Some(ref st) = self.hook_state {
                Box::pin(ExtensionHub::with_hook_state(st.clone(), fut)).await?
            } else {
                fut.await?
            };
            if !res.ok {
                return Err(anyhow!(
                    "{}",
                    res.error.unwrap_or_else(|| format!(
                        "Extension '{}' execution failed",
                        job.extension_id
                    ))
                ));
            }
            Ok(res)
        }))
        .await;

        let test_res = match fetch_res {
            Ok(r) => r,
            Err(err) => {
                let updated = fail(
                    &self.db,
                    &self.env,
                    &job.id,
                    &err.to_string(),
                    self.base_backoff_secs,
                    self.max_backoff_secs,
                )
                .await?;
                return Ok(LazyExecResult::RetriedOrFailed(updated));
            }
        };

        // If a domain pipeline inside `test_fetch` (e.g. `run_disclosure_pipeline`) already
        // persisted a rich `sectionData` artifact or transitioned the job to `awaiting_external_response`,
        // preserve that artifact instead of overwriting it with generic envelope JSON.
        if let Ok(Some(reloaded_job)) = self.db.get_lazy_job_by_id(&job.id).await {
            if reloaded_job.status == "awaiting_external_response" {
                return Ok(LazyExecResult::AwaitingExternalResponse);
            }
            if reloaded_job.status == "succeeded" {
                if let Ok(Some(artifact)) =
                    self.db.get_lazy_artifact(&job.slug, &job.extension_id).await
                {
                    if artifact.status == "awaiting_external_response" {
                        return Ok(LazyExecResult::AwaitingExternalResponse);
                    }
                    return Ok(LazyExecResult::Completed(Box::new(
                        CompleteLazyJobOutcome {
                            job: Some(reloaded_job),
                            artifact,
                            report_patched: true,
                            new_report_updated_at: None,
                            notifications: SectionNotificationOutcome {
                                checked: 0,
                                claimed: 0,
                                delivered_via_hook: 0,
                                delivered_via_smtp: 0,
                                skipped_no_smtp: 0,
                                failed: 0,
                            },
                        },
                    )));
                }
            }
        }

        let bundle_val = test_res
            .canonical_json
            .get("aggregatedBundle")
            .cloned()
            .unwrap_or(Value::Null);

        if bundle_val
            .get("awaitingExternalResponse")
            .and_then(Value::as_bool)
            == Some(true)
        {
            mark_awaiting_external_response(
                &self.db,
                &self.env,
                &job.slug,
                &job.extension_id,
                &job.section_id,
                json!({
                    "extensionId": job.extension_id,
                    "sectionId": job.section_id,
                    "awaitingExternalResponse": true,
                }),
                policy.min_plan_tier.as_deref(),
                None,
            )
            .await?;
            return Ok(LazyExecResult::AwaitingExternalResponse);
        }

        let report_patch = match serde_json::from_value::<AggregatedPropertyBundleProto>(bundle_val)
        {
            Ok(bundle) => to_report_json_patch(&bundle),
            Err(_) => json!({}),
        };

        let payload = json!({
            "extensionId": job.extension_id,
            "sectionId": job.section_id,
            "reportPatch": report_patch,
            "envelopes": test_res.canonical_json.get("envelopes").cloned().unwrap_or_else(|| json!([])),
            "attributions": report_patch.get("attributions").cloned().unwrap_or_else(|| json!([])),
            "latencyMs": test_res.latency_ms,
            "envelopeCount": test_res.envelope_count,
        });

        let expires_at = policy
            .cache_ttl_secs
            .filter(|&ttl| ttl > 0)
            .map(|ttl| now_unix_secs() + ttl as i64);

        let completed = Box::pin(complete(
            &self.db,
            &self.http,
            &self.env,
            Some(self.hooks.as_ref()),
            &job.id,
            payload,
            None,
            policy.min_plan_tier.as_deref(),
            expires_at,
            &self.base_origin,
        ))
        .await?;

        Ok(LazyExecResult::Completed(Box::new(completed)))
    }
}

#[derive(Debug, Clone)]
pub struct LazyWorkerConfig {
    pub worker_id: String,
    pub concurrency: usize,
    pub poll_interval: Duration,
    pub lease_secs: i64,
    pub heartbeat_interval: Duration,
    pub base_backoff_secs: i64,
    pub max_backoff_secs: i64,
    /// When `true`, `spawn_lazy_worker` sleeps on event-driven `Notify` / shutdown only,
    /// delegating periodic recovery sweeps to `open_estate::scheduler` (`lazy_worker_poll`).
    pub wake_notify_only: bool,
}

impl Default for LazyWorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: format!("lazy-worker-{}", &Uuid::new_v4().simple().to_string()[..8]),
            concurrency: 4,
            poll_interval: Duration::from_millis(1_500),
            lease_secs: 120,
            heartbeat_interval: Duration::from_secs(30),
            base_backoff_secs: 15,
            max_backoff_secs: 3_600,
            wake_notify_only: false,
        }
    }
}

impl LazyWorkerConfig {
    pub fn from_env(env: &EnvConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(c) = env
            .get("LAZY_WORKER_CONCURRENCY")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1)
        {
            cfg.concurrency = c.min(64);
        }
        if let Some(ms) = env
            .get("LAZY_WORKER_POLL_MS")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n >= 25)
        {
            cfg.poll_interval = Duration::from_millis(ms);
        }
        if let Some(ls) = env
            .get("LAZY_JOB_LEASE_SECS")
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|&n| n >= 10)
        {
            cfg.lease_secs = ls.min(3_600);
            cfg.heartbeat_interval = Duration::from_secs(((ls / 3).max(3)) as u64);
        }
        cfg
    }

    pub fn with_notify_only(mut self, notify_only: bool) -> Self {
        self.wake_notify_only = notify_only;
        self
    }
}

#[derive(Clone)]
pub struct LazyWorker {
    executor: LazyExecutor,
    config: LazyWorkerConfig,
}

impl LazyWorker {
    pub fn new(executor: LazyExecutor, config: LazyWorkerConfig) -> Self {
        let executor = executor.with_backoff(config.base_backoff_secs, config.max_backoff_secs);
        Self { executor, config }
    }

    /// Claims and executes one due job (with active lease heartbeat). Returns `Ok(true)` if a job was processed.
    pub async fn process_one(&self) -> Result<bool> {
        let Some(job) = claim_next(
            &self.executor.db,
            &self.config.worker_id,
            self.config.lease_secs,
        )
        .await?
        else {
            return Ok(false);
        };

        let hb_db = self.executor.db.clone();
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
                        if let Ok(false) = heartbeat(&hb_db, &hb_job_id, &hb_worker, hb_lease).await {
                            break;
                        }
                    }
                }
            }
        });

        let res = self.executor.execute_claimed_job(&job).await;
        let _ = stop_tx.send(true);
        let _ = hb_task.await;

        if let Err(err) = res {
            tracing::warn!(
                job_id = %job.id,
                slug = %job.slug,
                extension_id = %job.extension_id,
                error = %err,
                "Lazy extension execution error"
            );
            let _ = fail(
                &self.executor.db,
                &self.executor.env,
                &job.id,
                &err.to_string(),
                self.config.base_backoff_secs,
                self.config.max_backoff_secs,
            )
            .await;
        }

        Ok(true)
    }

    /// Drains up to `max_jobs` currently due jobs sequentially (useful for tests or CLI sweeps).
    pub async fn drain_due_jobs(&self, max_jobs: usize) -> Result<usize> {
        let mut processed = 0usize;
        while processed < max_jobs {
            if !self.process_one().await? {
                break;
            }
            processed += 1;
        }
        Ok(processed)
    }
}

/// Handle controlling a spawned background [`LazyWorker`] task.
pub struct LazyWorkerHandle {
    wake: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    join_handle: Option<JoinHandle<()>>,
}

impl LazyWorkerHandle {
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        self.wake.notify_waiters();
        if let Some(jh) = self.join_handle.take() {
            let _ = jh.await;
        }
    }
}

impl Drop for LazyWorkerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.wake.notify_waiters();
    }
}

/// Spawns a bounded-concurrency background worker loop polling `lazy_extension_jobs`.
pub fn spawn_lazy_worker(worker: LazyWorker) -> LazyWorkerHandle {
    let wake = Arc::new(Notify::new());
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let wake_loop = Arc::clone(&wake);

    let join_handle = tokio::spawn(async move {
        let concurrency = worker.config.concurrency.max(1);
        let sem = Arc::new(Semaphore::new(concurrency));
        let mut interval = tokio::time::interval(worker.config.poll_interval);

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            // Greedily fill available concurrency permits while due jobs exist.
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                let Ok(permit) = Arc::clone(&sem).try_acquire_owned() else {
                    break;
                };
                match claim_next(
                    &worker.executor.db,
                    &worker.config.worker_id,
                    worker.config.lease_secs,
                )
                .await
                {
                    Ok(Some(job)) => {
                        let exec = worker.executor.clone();
                        let cfg = worker.config.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let hb_db = exec.db.clone();
                            let hb_job_id = job.id.clone();
                            let hb_worker = cfg.worker_id.clone();
                            let hb_lease = cfg.lease_secs;
                            let hb_interval = cfg.heartbeat_interval;
                            let (hb_stop_tx, mut hb_stop_rx) = watch::channel(false);

                            let hb_task = tokio::spawn(async move {
                                let mut t = tokio::time::interval(hb_interval);
                                t.tick().await;
                                loop {
                                    tokio::select! {
                                        _ = hb_stop_rx.changed() => {
                                            if *hb_stop_rx.borrow() { break; }
                                        }
                                        _ = t.tick() => {
                                            if let Ok(false) = heartbeat(&hb_db, &hb_job_id, &hb_worker, hb_lease).await {
                                                break;
                                            }
                                        }
                                    }
                                }
                            });

                            let res = exec.execute_claimed_job(&job).await;
                            let _ = hb_stop_tx.send(true);
                            let _ = hb_task.await;

                            if let Err(err) = res {
                                let _ = fail(
                                    &exec.db,
                                    &exec.env,
                                    &job.id,
                                    &err.to_string(),
                                    cfg.base_backoff_secs,
                                    cfg.max_backoff_secs,
                                )
                                .await;
                            }
                        });
                    }
                    Ok(None) => {
                        drop(permit);
                        break;
                    }
                    Err(err) => {
                        drop(permit);
                        tracing::warn!(error = %err, "Lazy worker claim_next error");
                        break;
                    }
                }
            }

            if worker.config.wake_notify_only {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = wake_loop.notified() => {}
                }
            } else {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = wake_loop.notified() => {}
                    _ = interval.tick() => {}
                }
            }
        }
    });

    LazyWorkerHandle {
        wake,
        shutdown_tx,
        join_handle: Some(join_handle),
    }
}
