//! Tokio task-local `MeterScope`, DAG parent rollup, `MeterHandle` for `tokio::spawn`,
//! active extension LLM spec propagation, and thread-local fallback for explicit sync scopes.

use crate::ai::llm::LlmUsage;
use crate::metering::pricing::{resolve_external_cost_table, resolve_model_price_table};
use crate::metering::{
    ActiveLlmSpec, ExtensionTier, ExtensionUsageEvent, InvocationContext, InvocationOutcome,
    InvocationTrigger, LlmUsageRecord, UsageDimensions,
};
use crate::shared::{now_iso, EnvConfig};
use std::cell::RefCell;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct MeterScopeState {
    pub(crate) invocation_id: String,
    pub(crate) extension_id: String,
    pub(crate) parent: Option<Arc<MeterScopeState>>,
    pub(crate) dims: Mutex<UsageDimensions>,
    pub(crate) outcome: Mutex<Option<InvocationOutcome>>,
    pub(crate) llm_spec: Mutex<Option<ActiveLlmSpec>>,
    pub(crate) llm_calls: Mutex<Vec<LlmUsageRecord>>,
}

tokio::task_local! {
    static ACTIVE_METER_SCOPE: Arc<MeterScopeState>;
}

tokio::task_local! {
    static ACTIVE_LLM_SPEC: Arc<ActiveLlmSpec>;
}

thread_local! {
    static THREAD_METER_SCOPE: RefCell<Option<Arc<MeterScopeState>>> = const { RefCell::new(None) };
    static THREAD_LLM_SPEC: RefCell<Option<Arc<ActiveLlmSpec>>> = const { RefCell::new(None) };
}

struct ThreadScopeGuard {
    prev_scope: Option<Arc<MeterScopeState>>,
    prev_llm: Option<Arc<ActiveLlmSpec>>,
}

impl Drop for ThreadScopeGuard {
    fn drop(&mut self) {
        let prev_scope = self.prev_scope.take();
        let prev_llm = self.prev_llm.take();
        THREAD_METER_SCOPE.with(|cell| {
            *cell.borrow_mut() = prev_scope;
        });
        THREAD_LLM_SPEC.with(|cell| {
            *cell.borrow_mut() = prev_llm;
        });
    }
}

fn push_thread_scope(
    state: Arc<MeterScopeState>,
    llm_spec: Option<Arc<ActiveLlmSpec>>,
) -> ThreadScopeGuard {
    let prev_scope = THREAD_METER_SCOPE.with(|cell| cell.borrow_mut().replace(state));
    let prev_llm =
        THREAD_LLM_SPEC.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), llm_spec));
    ThreadScopeGuard {
        prev_scope,
        prev_llm,
    }
}

fn current_scope_state() -> Option<Arc<MeterScopeState>> {
    if let Ok(state) = ACTIVE_METER_SCOPE.try_with(Arc::clone) {
        return Some(state);
    }
    THREAD_METER_SCOPE.with(|cell| cell.borrow().clone())
}

fn current_standalone_llm_spec() -> Option<ActiveLlmSpec> {
    if let Ok(spec) = ACTIVE_LLM_SPEC.try_with(|s| (**s).clone()) {
        return Some(spec);
    }
    THREAD_LLM_SPEC.with(|cell| cell.borrow().as_ref().map(|s| (**s).clone()))
}

fn with_active_dims<F>(f: F)
where
    F: FnOnce(&mut UsageDimensions),
{
    if let Some(state) = current_scope_state() {
        let mut dims = state.dims.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut dims);
    }
}

/// Runs `fut` with `spec` bound as the active task-local extension LLM spec (also updating any
/// enclosing [`MeterScope`] when appropriate).
pub async fn with_active_llm_spec<F, T>(spec: ActiveLlmSpec, fut: F) -> T
where
    F: Future<Output = T>,
{
    ACTIVE_LLM_SPEC.scope(Arc::new(spec), fut).await
}

/// Returns the active extension ID from the active LLM spec or [`MeterScope`], if any.
pub fn active_extension_id() -> Option<String> {
    if let Some(spec) = MeterScope::active_llm_spec() {
        if !spec.extension_id.trim().is_empty() {
            return Some(spec.extension_id);
        }
    }
    current_scope_state()
        .map(|s| s.extension_id.clone())
        .filter(|id| !id.trim().is_empty())
}

/// Cloneable handle to propagate an active [`MeterScope`] (and active LLM spec) across `tokio::spawn` or threads.
#[derive(Clone, Debug, Default)]
pub struct MeterHandle {
    state: Option<Arc<MeterScopeState>>,
    llm_spec: Option<Arc<ActiveLlmSpec>>,
}

impl MeterHandle {
    /// Captures the currently active [`MeterScope`] and LLM spec, or an inert no-op handle if none is active.
    pub fn current() -> Self {
        let state = current_scope_state();
        let llm_spec = current_standalone_llm_spec()
            .or_else(|| {
                state
                    .as_ref()
                    .and_then(|s| s.llm_spec.lock().unwrap_or_else(|e| e.into_inner()).clone())
            })
            .map(Arc::new);
        Self { state, llm_spec }
    }

    pub fn is_active(&self) -> bool {
        self.state.is_some()
    }

    pub fn invocation_id(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.invocation_id.as_str())
    }

    /// Runs `fut` on another task while preserving the captured [`MeterScope`] and LLM spec.
    pub async fn scope<F, T>(self, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        match (self.state, self.llm_spec) {
            (Some(state), Some(spec)) => {
                ACTIVE_METER_SCOPE
                    .scope(state, ACTIVE_LLM_SPEC.scope(spec, fut))
                    .await
            }
            (Some(state), None) => ACTIVE_METER_SCOPE.scope(state, fut).await,
            (None, Some(spec)) => ACTIVE_LLM_SPEC.scope(spec, fut).await,
            (None, None) => fut.await,
        }
    }

    /// Runs a synchronous closure inside this handle's scope on the current thread (useful in `spawn_blocking`).
    pub fn run_sync<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        match &self.state {
            Some(state) => {
                let _guard = push_thread_scope(Arc::clone(state), self.llm_spec.clone());
                f()
            }
            None => f(),
        }
    }
}

/// Task-local execution meter tracking multi-dimensional usage and DAG parent rollups.
pub struct MeterScope;

impl MeterScope {
    /// Captures a [`MeterHandle`] for the active scope, if one is running.
    pub fn current() -> Option<MeterHandle> {
        current_scope_state().map(|state| {
            let llm_spec = current_standalone_llm_spec()
                .or_else(|| {
                    state
                        .llm_spec
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .map(Arc::new);
            MeterHandle {
                state: Some(state),
                llm_spec,
            }
        })
    }

    /// Captures a [`MeterHandle`] (inert no-op handle if no scope is active).
    pub fn handle() -> MeterHandle {
        MeterHandle::current()
    }

    /// Sets or replaces the [`ActiveLlmSpec`] on the currently active [`MeterScope`], if any.
    pub fn set_active_llm_spec(spec: ActiveLlmSpec) {
        if let Some(state) = current_scope_state() {
            *state.llm_spec.lock().unwrap_or_else(|e| e.into_inner()) = Some(spec);
        }
    }

    /// Returns the active [`ActiveLlmSpec`] for the current task (checking explicit task-local spec
    /// and active [`MeterScope`]).
    pub fn active_llm_spec() -> Option<ActiveLlmSpec> {
        if let Some(spec) = current_standalone_llm_spec() {
            return Some(spec);
        }
        current_scope_state()
            .and_then(|s| s.llm_spec.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Spawns a Tokio task that inherits the current [`MeterScope`].
    pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = MeterHandle::current();
        tokio::spawn(handle.scope(fut))
    }

    fn prepare_scope(
        ctx: &InvocationContext,
    ) -> (
        Arc<MeterScopeState>,
        Option<String>,
        String,
        String,
        Instant,
        Option<ActiveLlmSpec>,
    ) {
        let parent = current_scope_state();
        let invocation_id = ctx
            .invocation_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let parent_invocation_id = ctx
            .parent_invocation_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| parent.as_ref().map(|p| p.invocation_id.clone()));

        let inherited_llm_spec = ctx
            .llm_spec
            .clone()
            .map(|mut spec| {
                if spec.extension_id.trim().is_empty() {
                    spec.extension_id = ctx.extension_id.clone();
                }
                spec
            })
            .or_else(|| {
                parent
                    .as_ref()
                    .and_then(|p| p.llm_spec.lock().unwrap_or_else(|e| e.into_inner()).clone())
            })
            .or_else(current_standalone_llm_spec);

        let state = Arc::new(MeterScopeState {
            invocation_id: invocation_id.clone(),
            extension_id: ctx.extension_id.clone(),
            parent,
            dims: Mutex::new(UsageDimensions::default()),
            outcome: Mutex::new(None),
            llm_spec: Mutex::new(inherited_llm_spec.clone()),
            llm_calls: Mutex::new(Vec::new()),
        });
        let started_at = now_iso();
        let started_instant = Instant::now();
        (
            state,
            parent_invocation_id,
            invocation_id,
            started_at,
            started_instant,
            inherited_llm_spec,
        )
    }

    fn finish_scope(
        ctx: InvocationContext,
        state: Arc<MeterScopeState>,
        parent_invocation_id: Option<String>,
        invocation_id: String,
        started_at: String,
        started_instant: Instant,
    ) -> ExtensionUsageEvent {
        let elapsed_ms = started_instant.elapsed().as_millis() as u64;
        let finished_at = now_iso();
        let outcome = state
            .outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(InvocationOutcome::Ok);

        let dims_snapshot = {
            let mut dims = state.dims.lock().unwrap_or_else(|e| e.into_inner());
            dims.wall_time_ms = elapsed_ms;
            dims.invocations = dims.invocations.saturating_add(1);
            match outcome {
                InvocationOutcome::Ok => {
                    dims.invocations_ok = dims.invocations_ok.saturating_add(1)
                }
                InvocationOutcome::Failed => {
                    dims.invocations_failed = dims.invocations_failed.saturating_add(1)
                }
                InvocationOutcome::Cached => {
                    dims.invocations_cached = dims.invocations_cached.saturating_add(1)
                }
                InvocationOutcome::Denied => {
                    dims.invocations_denied = dims.invocations_denied.saturating_add(1)
                }
            }
            dims.clone()
        };

        let llm_calls_snapshot = {
            state
                .llm_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        let llm_model = llm_calls_snapshot.last().map(|c| c.model.clone());
        let llm_effort = llm_calls_snapshot.last().and_then(|c| c.effort.clone());

        if let Some(ref parent) = state.parent {
            let mut p_dims = parent.dims.lock().unwrap_or_else(|e| e.into_inner());
            p_dims.rollup_child(&dims_snapshot);
            drop(p_dims);
            if !llm_calls_snapshot.is_empty() {
                let mut p_calls = parent.llm_calls.lock().unwrap_or_else(|e| e.into_inner());
                p_calls.extend(llm_calls_snapshot.iter().cloned());
            }
        }

        let billable = ctx.billable.unwrap_or(
            ctx.tier == ExtensionTier::Premium
                && ctx.trigger == InvocationTrigger::UserRequest
                && outcome != InvocationOutcome::Cached
                && outcome != InvocationOutcome::Denied,
        );

        ExtensionUsageEvent {
            invocation_id,
            parent_invocation_id,
            extension_id: ctx.extension_id,
            version: if ctx.version.trim().is_empty() {
                "1.0.0".to_string()
            } else {
                ctx.version
            },
            tier: ctx.tier,
            trigger: ctx.trigger,
            actor_user_id: ctx.actor_user_id,
            org_id: ctx.org_id,
            slug: ctx.slug,
            started_at,
            finished_at,
            outcome,
            dims: dims_snapshot,
            billable,
            llm_model,
            llm_effort,
            llm_calls: llm_calls_snapshot,
        }
    }

    /// Runs `fut` inside a new task-local `MeterScope`, rolling up resource & invocation counters
    /// into any enclosing parent `MeterScope` and returning `(output, event)`.
    pub async fn run<F, T>(ctx: InvocationContext, fut: F) -> (T, ExtensionUsageEvent)
    where
        F: Future<Output = T>,
    {
        let (state, parent_invocation_id, invocation_id, started_at, started_instant, llm_spec) =
            Self::prepare_scope(&ctx);
        let output = match llm_spec {
            Some(spec) => {
                ACTIVE_METER_SCOPE
                    .scope(
                        Arc::clone(&state),
                        ACTIVE_LLM_SPEC.scope(Arc::new(spec), fut),
                    )
                    .await
            }
            None => ACTIVE_METER_SCOPE.scope(Arc::clone(&state), fut).await,
        };
        let event = Self::finish_scope(
            ctx,
            state,
            parent_invocation_id,
            invocation_id,
            started_at,
            started_instant,
        );
        (output, event)
    }

    /// Convenience wrapper around [`Self::run`] for `Result<T, E>` futures: automatically sets
    /// `InvocationOutcome::Failed` when `fut` resolves to `Err` (unless already marked `Denied`/`Cached`).
    pub async fn run_result<F, T, E>(
        ctx: InvocationContext,
        fut: F,
    ) -> (std::result::Result<T, E>, ExtensionUsageEvent)
    where
        F: Future<Output = std::result::Result<T, E>>,
    {
        Self::run(ctx, async move {
            let res = fut.await;
            if res.is_err() {
                mark_failed_if_unset();
            }
            res
        })
        .await
    }

    /// Synchronous counterpart to [`Self::run`] for purely synchronous adapters (`execute_sync`).
    pub fn run_sync<F, T>(ctx: InvocationContext, f: F) -> (T, ExtensionUsageEvent)
    where
        F: FnOnce() -> T,
    {
        let (state, parent_invocation_id, invocation_id, started_at, started_instant, llm_spec) =
            Self::prepare_scope(&ctx);
        let output = {
            let _guard = push_thread_scope(Arc::clone(&state), llm_spec.map(Arc::new));
            f()
        };
        let event = Self::finish_scope(
            ctx,
            state,
            parent_invocation_id,
            invocation_id,
            started_at,
            started_instant,
        );
        (output, event)
    }
}

/// Sets the outcome of the active `MeterScope` (no-op outside a scope).
pub fn mark_outcome(outcome: InvocationOutcome) {
    if let Some(state) = current_scope_state() {
        *state.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    }
}

fn mark_failed_if_unset() {
    if let Some(state) = current_scope_state() {
        let mut guard = state.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(InvocationOutcome::Failed);
        }
    }
}

pub fn mark_failed() {
    mark_outcome(InvocationOutcome::Failed);
}

pub fn mark_cached() {
    mark_outcome(InvocationOutcome::Cached);
}

pub fn mark_denied() {
    mark_outcome(InvocationOutcome::Denied);
}

/// Returns a snapshot of the active scope's accumulated dimensions, if any.
pub fn current_dims() -> Option<UsageDimensions> {
    current_scope_state().map(|s| s.dims.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// Returns the active scope's `invocation_id`, if any.
pub fn current_invocation_id() -> Option<String> {
    current_scope_state().map(|s| s.invocation_id.clone())
}

/// Records LLM token usage and calculated micro-USD cost into the active scope.
pub fn add_llm_usage(usage: &LlmUsage, model: &str) {
    add_llm_usage_detailed(None, usage, model, None, None, None, None);
}

/// Records LLM token usage using pricing from `env` (falling back to process env and defaults),
/// checking `fallback_model` if `model` has no direct pricing match.
pub fn add_llm_usage_with_env(
    env: Option<&EnvConfig>,
    usage: &LlmUsage,
    model: &str,
    fallback_model: Option<&str>,
) {
    add_llm_usage_detailed(env, usage, model, fallback_model, None, None, None);
}

/// Records LLM token usage, calculated micro-USD cost, and resolved `(model, effort, profile, task)`
/// into the active [`MeterScope`].
pub fn add_llm_usage_detailed(
    env: Option<&EnvConfig>,
    usage: &LlmUsage,
    model: &str,
    fallback_model: Option<&str>,
    effort: Option<&str>,
    profile: Option<&str>,
    task: Option<&str>,
) {
    let Some(state) = current_scope_state() else {
        return;
    };
    let table = resolve_model_price_table(env);
    let mut cost = table.cost_micros(usage, model);
    if cost == 0 {
        if let Some(fb) = fallback_model {
            cost = table.cost_micros(usage, fb);
        }
    }
    let input_tokens = usage.input_tokens.unwrap_or(0);
    let cached_input_tokens = usage.cached_input_tokens.min(input_tokens);
    let cache_write_tokens = usage.cache_write_tokens;
    let output_tokens = usage.output_tokens.unwrap_or(0);
    let reasoning_tokens = usage.reasoning_tokens;
    {
        let mut d = state.dims.lock().unwrap_or_else(|e| e.into_inner());
        d.llm_input_tokens = d.llm_input_tokens.saturating_add(input_tokens);
        d.llm_cached_input_tokens = d
            .llm_cached_input_tokens
            .saturating_add(cached_input_tokens);
        d.llm_cache_write_tokens = d.llm_cache_write_tokens.saturating_add(cache_write_tokens);
        d.llm_output_tokens = d.llm_output_tokens.saturating_add(output_tokens);
        d.llm_reasoning_tokens = d.llm_reasoning_tokens.saturating_add(reasoning_tokens);
        d.llm_cost_micros = d.llm_cost_micros.saturating_add(cost);
    }
    let mut calls = state.llm_calls.lock().unwrap_or_else(|e| e.into_inner());
    calls.push(LlmUsageRecord {
        model: model.to_string(),
        effort: effort
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        profile: profile
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        task: task
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        output_tokens,
        reasoning_tokens,
        cost_micros: cost,
    });
}

/// Records one external API call to `host` (or URL) and its configured micro-USD cost.
pub fn add_external_call(host: &str) {
    add_external_call_with_env(None, host);
}

/// Records one external API call using per-host costs from `env` (or process env).
pub fn add_external_call_with_env(env: Option<&EnvConfig>, host_or_url: &str) {
    if current_scope_state().is_none() {
        return;
    }
    let table = resolve_external_cost_table(env);
    let cost = table.cost_micros_for_host(host_or_url);
    with_active_dims(|d| {
        d.external_api_calls = d.external_api_calls.saturating_add(1);
        d.external_api_cost_micros = d.external_api_cost_micros.saturating_add(cost);
    });
}

/// Records downloaded payload bytes into the active scope.
pub fn add_bytes_downloaded(bytes: u64) {
    if bytes == 0 {
        return;
    }
    with_active_dims(|d| {
        d.bytes_downloaded = d.bytes_downloaded.saturating_add(bytes);
    });
}

/// Records one external API call plus downloaded bytes into the active scope.
pub fn add_external_call_with_bytes(env: Option<&EnvConfig>, host_or_url: &str, bytes: u64) {
    add_external_call_with_env(env, host_or_url);
    add_bytes_downloaded(bytes);
}

/// Records browser automation active session duration in milliseconds.
pub fn add_browser_ms(ms: u64) {
    if ms == 0 {
        return;
    }
    with_active_dims(|d| {
        d.browser_session_ms = d.browser_session_ms.saturating_add(ms);
    });
}

/// Records browser page loads.
pub fn add_browser_page_loads(pages: u64) {
    if pages == 0 {
        return;
    }
    with_active_dims(|d| {
        d.browser_page_loads = d.browser_page_loads.saturating_add(pages);
    });
}

/// Records browser session duration and page loads together.
pub fn add_browser_activity(ms: u64, page_loads: u64) {
    if ms == 0 && page_loads == 0 {
        return;
    }
    with_active_dims(|d| {
        d.browser_session_ms = d.browser_session_ms.saturating_add(ms);
        d.browser_page_loads = d.browser_page_loads.saturating_add(page_loads);
    });
}

/// Records sent outbound emails.
pub fn add_email_sent(count: u64) {
    if count == 0 {
        return;
    }
    with_active_dims(|d| {
        d.emails_sent = d.emails_sent.saturating_add(count);
    });
}

/// Records ingested inbound emails.
pub fn add_email_ingested(count: u64) {
    if count == 0 {
        return;
    }
    with_active_dims(|d| {
        d.emails_ingested = d.emails_ingested.saturating_add(count);
    });
}

/// Records one ingested document with its OCR page count and downloaded/ingested byte size.
/// Pass `bytes = 0` if `HttpClient` already recorded the download bytes.
pub fn add_document(pages_ocr: u64, bytes: u64) {
    with_active_dims(|d| {
        d.documents_ingested = d.documents_ingested.saturating_add(1);
        d.pages_ocr = d.pages_ocr.saturating_add(pages_ocr);
        d.bytes_downloaded = d.bytes_downloaded.saturating_add(bytes);
    });
}

/// Records persisted storage bytes (e.g. encrypted disclosure vault).
pub fn add_bytes_stored(bytes: u64) {
    if bytes == 0 {
        return;
    }
    with_active_dims(|d| {
        d.bytes_stored = d.bytes_stored.saturating_add(bytes);
    });
}
