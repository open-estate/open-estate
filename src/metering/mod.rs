//! Multi-dimensional extension and subsystem metering for Open Estate.
//!
//! OSS core emits dimensions (`UsageDimensions`, `ExtensionUsageEvent`, `MeterScope`) and
//! retains local daily rollups for `/ops`; commercial pricing/billing lives in downstream
//! host hooks (`ServerExtensionHooks`).

pub mod pricing;
pub mod proto_conv;
pub mod scope;

pub use pricing::{normalize_host_key, ExternalCostTable, ModelPriceMicros, ModelPriceTable};
pub use scope::{
    active_extension_id, add_browser_activity, add_browser_ms, add_browser_page_loads,
    add_bytes_downloaded, add_bytes_stored, add_document, add_email_ingested, add_email_sent,
    add_external_call, add_external_call_with_bytes, add_external_call_with_env, add_llm_usage,
    add_llm_usage_detailed, add_llm_usage_with_env, current_dims, current_invocation_id,
    mark_cached, mark_denied, mark_failed, mark_outcome, with_active_llm_spec, MeterHandle,
    MeterScope,
};

use crate::db::{metering_store, DbStore};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use std::pin::Pin;

/// Multi-dimensional resource & outcome counters captured per invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageDimensions {
    #[serde(default)]
    pub invocations: u64,
    #[serde(default)]
    pub invocations_ok: u64,
    #[serde(default)]
    pub invocations_failed: u64,
    #[serde(default)]
    pub invocations_cached: u64,
    #[serde(default)]
    pub invocations_denied: u64,
    #[serde(default)]
    pub wall_time_ms: u64,
    #[serde(default)]
    pub llm_input_tokens: u64,
    #[serde(default)]
    pub llm_cached_input_tokens: u64,
    #[serde(default)]
    pub llm_cache_write_tokens: u64,
    #[serde(default)]
    pub llm_output_tokens: u64,
    #[serde(default)]
    pub llm_reasoning_tokens: u64,
    #[serde(default)]
    pub llm_cost_micros: u64,
    #[serde(default)]
    pub browser_session_ms: u64,
    #[serde(default)]
    pub browser_page_loads: u64,
    #[serde(default)]
    pub emails_sent: u64,
    #[serde(default)]
    pub emails_ingested: u64,
    #[serde(default)]
    pub documents_ingested: u64,
    #[serde(default)]
    pub pages_ocr: u64,
    #[serde(default)]
    pub bytes_downloaded: u64,
    #[serde(default)]
    pub bytes_stored: u64,
    #[serde(default)]
    pub external_api_calls: u64,
    #[serde(default)]
    pub external_api_cost_micros: u64,
}

impl UsageDimensions {
    /// Rolls up a completed child scope into an enclosing parent DAG scope.
    ///
    /// Adds all invocation and resource counters while keeping `wall_time_ms` governed by the
    /// parent scope's own monotonic elapsed clock (so concurrent children do not double-count wall time).
    pub fn rollup_child(&mut self, child: &UsageDimensions) {
        self.invocations = self.invocations.saturating_add(child.invocations);
        self.invocations_ok = self.invocations_ok.saturating_add(child.invocations_ok);
        self.invocations_failed = self
            .invocations_failed
            .saturating_add(child.invocations_failed);
        self.invocations_cached = self
            .invocations_cached
            .saturating_add(child.invocations_cached);
        self.invocations_denied = self
            .invocations_denied
            .saturating_add(child.invocations_denied);
        self.add_resources(child);
    }

    /// Accumulates all dimensions including `wall_time_ms` (for daily rollups across separate events).
    pub fn accumulate_event(&mut self, other: &UsageDimensions) {
        self.rollup_child(other);
        self.wall_time_ms = self.wall_time_ms.saturating_add(other.wall_time_ms);
    }

    fn add_resources(&mut self, other: &UsageDimensions) {
        self.llm_input_tokens = self.llm_input_tokens.saturating_add(other.llm_input_tokens);
        self.llm_cached_input_tokens = self
            .llm_cached_input_tokens
            .saturating_add(other.llm_cached_input_tokens);
        self.llm_cache_write_tokens = self
            .llm_cache_write_tokens
            .saturating_add(other.llm_cache_write_tokens);
        self.llm_output_tokens = self
            .llm_output_tokens
            .saturating_add(other.llm_output_tokens);
        self.llm_reasoning_tokens = self
            .llm_reasoning_tokens
            .saturating_add(other.llm_reasoning_tokens);
        self.llm_cost_micros = self.llm_cost_micros.saturating_add(other.llm_cost_micros);
        self.browser_session_ms = self
            .browser_session_ms
            .saturating_add(other.browser_session_ms);
        self.browser_page_loads = self
            .browser_page_loads
            .saturating_add(other.browser_page_loads);
        self.emails_sent = self.emails_sent.saturating_add(other.emails_sent);
        self.emails_ingested = self.emails_ingested.saturating_add(other.emails_ingested);
        self.documents_ingested = self
            .documents_ingested
            .saturating_add(other.documents_ingested);
        self.pages_ocr = self.pages_ocr.saturating_add(other.pages_ocr);
        self.bytes_downloaded = self.bytes_downloaded.saturating_add(other.bytes_downloaded);
        self.bytes_stored = self.bytes_stored.saturating_add(other.bytes_stored);
        self.external_api_calls = self
            .external_api_calls
            .saturating_add(other.external_api_calls);
        self.external_api_cost_micros = self
            .external_api_cost_micros
            .saturating_add(other.external_api_cost_micros);
    }
}

/// Extension license / monetization tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionTier {
    #[default]
    Basic,
    Premium,
}

impl ExtensionTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Premium => "premium",
        }
    }

    pub fn from_str_lossy(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("premium") {
            Self::Premium
        } else {
            Self::Basic
        }
    }
}

impl fmt::Display for ExtensionTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Origin trigger of an extension invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InvocationTrigger {
    #[default]
    UserRequest,
    PeriodicScan,
    LazyJob,
    AdminTest,
    Dependency,
}

impl InvocationTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserRequest => "user_request",
            Self::PeriodicScan => "periodic_scan",
            Self::LazyJob => "lazy_job",
            Self::AdminTest => "admin_test",
            Self::Dependency => "dependency",
        }
    }

    pub fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "periodic_scan" | "periodicscan" | "scan" => Self::PeriodicScan,
            "lazy_job" | "lazyjob" | "lazy" => Self::LazyJob,
            "admin_test" | "admintest" | "test_fetch" => Self::AdminTest,
            "dependency" | "dep" => Self::Dependency,
            _ => Self::UserRequest,
        }
    }
}

impl fmt::Display for InvocationTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Final execution status of an extension invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOutcome {
    #[default]
    Ok,
    Failed,
    Cached,
    Denied,
}

impl InvocationOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Cached => "cached",
            Self::Denied => "denied",
        }
    }

    pub fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "failed" | "error" => Self::Failed,
            "cached" | "cache_hit" => Self::Cached,
            "denied" => Self::Denied,
            _ => Self::Ok,
        }
    }
}

impl fmt::Display for InvocationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn default_version() -> String {
    "1.0.0".to_string()
}

/// Per-task LLM model/profile/effort override inside an extension manifest (`llm.tasks[task]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveLlmTaskSpec {
    pub task: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

/// Active extension LLM profile/model/effort policy propagated through [`MeterScope`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveLlmSpec {
    #[serde(default)]
    pub extension_id: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub tasks: Vec<ActiveLlmTaskSpec>,
}

/// Individual LLM call usage & resolved model/effort recorded on [`ExtensionUsageEvent`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsageRecord {
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cost_micros: u64,
}

/// Input metadata describing an invocation wrapped by [`MeterScope`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InvocationContext {
    #[serde(default)]
    pub invocation_id: Option<String>,
    #[serde(default)]
    pub parent_invocation_id: Option<String>,
    #[serde(default)]
    pub extension_id: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub tier: ExtensionTier,
    #[serde(default)]
    pub trigger: InvocationTrigger,
    #[serde(default)]
    pub actor_user_id: Option<String>,
    #[serde(default)]
    pub org_id: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub billable: Option<bool>,
    #[serde(default)]
    pub llm_spec: Option<ActiveLlmSpec>,
}

impl InvocationContext {
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self {
            extension_id: extension_id.into(),
            version: default_version(),
            ..Default::default()
        }
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn with_tier(mut self, tier: ExtensionTier) -> Self {
        self.tier = tier;
        self
    }

    pub fn with_trigger(mut self, trigger: InvocationTrigger) -> Self {
        self.trigger = trigger;
        self
    }

    pub fn with_actor_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.actor_user_id = Some(user_id.into());
        self
    }

    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Self {
        self.org_id = Some(org_id.into());
        self
    }

    pub fn with_slug(mut self, slug: impl Into<String>) -> Self {
        self.slug = Some(slug.into());
        self
    }

    pub fn with_parent_invocation_id(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_invocation_id = Some(parent_id.into());
        self
    }

    pub fn with_billable(mut self, billable: bool) -> Self {
        self.billable = Some(billable);
        self
    }

    pub fn with_llm_spec(mut self, llm_spec: ActiveLlmSpec) -> Self {
        self.llm_spec = Some(llm_spec);
        self
    }
}

/// Structured usage event emitted when a [`MeterScope`] finishes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionUsageEvent {
    pub invocation_id: String,
    pub parent_invocation_id: Option<String>,
    pub extension_id: String,
    pub version: String,
    pub tier: ExtensionTier,
    pub trigger: InvocationTrigger,
    pub actor_user_id: Option<String>,
    pub org_id: Option<String>,
    pub slug: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub outcome: InvocationOutcome,
    pub dims: UsageDimensions,
    pub billable: bool,
    #[serde(default)]
    pub llm_model: Option<String>,
    #[serde(default)]
    pub llm_effort: Option<String>,
    #[serde(default)]
    pub llm_calls: Vec<LlmUsageRecord>,
}

/// Authorization decision returned before executing an extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum InvocationDecision {
    #[default]
    Allow,
    Deny {
        reason: String,
        required_tier: String,
    },
    RequireApproval {
        reason: String,
    },
}

impl InvocationDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Default OSS authorization check: allows `basic` open-data / BYOK extensions and denies
/// `premium` or non-open license classes (unless overridden by downstream commercial hooks).
pub fn default_oss_authorize(
    ctx: &InvocationContext,
    manifest_tier: Option<&str>,
    manifest_license_class: Option<&str>,
    min_plan_tier: Option<&str>,
    requires_manual_approval: bool,
) -> InvocationDecision {
    let required_tier = min_plan_tier
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("single_deal")
        .to_string();

    let is_premium_manifest = manifest_tier
        .map(|t| t.trim().eq_ignore_ascii_case("premium"))
        .unwrap_or(false);
    if ctx.tier == ExtensionTier::Premium || is_premium_manifest {
        return InvocationDecision::Deny {
            reason: format!(
                "Extension '{}' requires commercial tier '{}'",
                ctx.extension_id, required_tier
            ),
            required_tier,
        };
    }

    if let Some(lc) = manifest_license_class
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let low = lc.to_ascii_lowercase();
        if matches!(
            low.as_str(),
            "non_open_scrape" | "human_in_loop" | "llm_service" | "proprietary" | "commercial"
        ) {
            return InvocationDecision::Deny {
                reason: format!(
                    "Extension '{}' license class '{}' is not permitted in OSS mode",
                    ctx.extension_id, lc
                ),
                required_tier,
            };
        }
    }

    if requires_manual_approval {
        return InvocationDecision::RequireApproval {
            reason: format!(
                "Extension '{}' requires manual approval before execution",
                ctx.extension_id
            ),
        };
    }

    InvocationDecision::Allow
}

/// Pluggable sink receiving finalized [`ExtensionUsageEvent`] records.
pub trait UsageSink: Send + Sync + 'static {
    fn record_usage<'a>(
        &'a self,
        event: &'a ExtensionUsageEvent,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// No-op [`UsageSink`] for contexts without persistent telemetry storage.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopUsageSink;

impl UsageSink for NoopUsageSink {
    fn record_usage<'a>(
        &'a self,
        _event: &'a ExtensionUsageEvent,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

/// Persistent local daily rollup [`UsageSink`] backed by [`DbStore`] (`metering_store`).
#[derive(Debug, Clone)]
pub struct DbUsageSink(pub DbStore);

impl UsageSink for DbUsageSink {
    fn record_usage<'a>(
        &'a self,
        event: &'a ExtensionUsageEvent,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { metering_store::record_local_usage_rollup(&self.0, event).await })
    }
}
