//! Periodic-scan lazy extension enqueue hook for [`ScanEngine`] and background report sweeps.
//!
//! When batch or interactive home scans persist outcomes in `ScanEngine::flush_buffer`,
//! an attached [`PeriodicLazyEnqueueHook`] idempotently schedules configured/lazy extensions
//! using singleflight deduplication (`crate::lazy::enqueue`) while respecting
//! `not_enabled` and `requires_manual_approval` safety gates.

use crate::db::DbStore;
use crate::extensions::ExtensionHub;
use crate::lazy::{enqueue, EnqueueLazyJobRequest, LazyExtensionPolicy, LazySectionStatusKind};
use crate::scanner::engine::{BoxFuture, ScanItemOutcome};
use crate::shared::EnvConfig;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// Asynchronous hook invoked by [`crate::scanner::ScanEngine`] after a micro-batch of scan
/// outcomes is flushed to storage.
pub trait PeriodicLazyEnqueueHook: Send + Sync + 'static {
    fn on_batch_flushed<'a>(&'a self, outcomes: &'a [ScanItemOutcome]) -> BoxFuture<'a, ()>;
}

/// Property descriptor passed to periodic lazy enqueue helpers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LazyScanTarget {
    pub slug: String,
    pub address: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub zip: Option<String>,
    pub extra_query: Option<Value>,
}

impl LazyScanTarget {
    pub fn new(slug: impl Into<String>, address: impl Into<String>) -> Self {
        Self {
            slug: slug.into(),
            address: address.into(),
            lat: None,
            lon: None,
            zip: None,
            extra_query: None,
        }
    }

    pub fn with_coords(mut self, lat: f64, lon: f64) -> Self {
        self.lat = Some(lat);
        self.lon = Some(lon);
        self
    }

    pub fn with_zip(mut self, zip: impl Into<String>) -> Self {
        self.zip = Some(zip.into());
        self
    }

    pub fn to_query_json(&self) -> Value {
        let mut obj = match &self.extra_query {
            Some(Value::Object(m)) => m.clone(),
            _ => serde_json::Map::new(),
        };
        if !self.address.trim().is_empty() && !obj.contains_key("address") {
            obj.insert("address".to_string(), json!(self.address.trim()));
        }
        if let Some(lat) = self.lat {
            obj.entry("lat".to_string()).or_insert_with(|| json!(lat));
        }
        if let Some(lon) = self.lon {
            obj.entry("lon".to_string()).or_insert_with(|| json!(lon));
        }
        if let Some(zip) = self.zip.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            obj.entry("zip".to_string()).or_insert_with(|| json!(zip));
        }
        Value::Object(obj)
    }
}

impl From<&ScanItemOutcome> for LazyScanTarget {
    fn from(o: &ScanItemOutcome) -> Self {
        let addr = o
            .matched_address
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&o.address);
        Self {
            slug: o.slug.clone(),
            address: addr.to_string(),
            lat: o.lat,
            lon: o.lon,
            zip: o
                .report_summary
                .get("zip")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            extra_query: None,
        }
    }
}

/// Extension + section pair eligible for periodic/batch lazy background scheduling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeriodicLazyExtensionSpec {
    pub extension_id: String,
    pub section_id: String,
    pub policy: LazyExtensionPolicy,
    pub max_attempts: i64,
}

impl PeriodicLazyExtensionSpec {
    pub fn new(extension_id: impl Into<String>, section_id: impl Into<String>) -> Self {
        Self {
            extension_id: extension_id.into(),
            section_id: section_id.into(),
            policy: LazyExtensionPolicy::default(),
            max_attempts: 5,
        }
    }

    pub fn with_policy(mut self, policy: LazyExtensionPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: i64) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Discovers all extensions marked `execution_mode: lazy` (via JSON metadata or
    /// `manifestYaml`) in an [`ExtensionHub`].
    pub fn discover_from_hub(hub: &ExtensionHub, env: Option<&EnvConfig>) -> Vec<Self> {
        let mut specs = Vec::new();
        for manifest in hub.list_manifests() {
            let ext_id = manifest.extension_id;
            let Some(ext_json) = hub.get_extension_json(&ext_id, env) else {
                continue;
            };
            let yaml_doc = ext_json
                .get("manifestYaml")
                .and_then(Value::as_str)
                .and_then(|raw| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(raw).ok());

            let exec_mode = ext_json
                .get("executionMode")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| {
                    yaml_doc
                        .as_ref()
                        .and_then(|d| d.get("execution_mode"))
                        .and_then(|v| v.as_str())
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| "eager".to_string());

            if !exec_mode.trim().eq_ignore_ascii_case("lazy") {
                continue;
            }

            let section_id = ext_json
                .get("sectionId")
                .and_then(Value::as_str)
                .or_else(|| {
                    yaml_doc
                        .as_ref()
                        .and_then(|d| d.get("section_id").or_else(|| d.get("report_section")))
                        .and_then(|v| v.as_str())
                })
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "publicResearch".to_string());

            specs.push(Self::new(ext_id, section_id));
        }
        specs
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeriodicLazyEnqueueSummary {
    pub targets_examined: usize,
    pub jobs_evaluated: usize,
    pub queued: usize,
    pub deduplicated: usize,
    pub awaiting_approval: usize,
    pub not_enabled: usize,
    pub errors: usize,
}

/// [`PeriodicLazyEnqueueHook`] backed by [`DbStore`] and optional [`ExtensionHub`].
#[derive(Clone)]
pub struct DbLazyScanHook {
    pub db: DbStore,
    pub hub: Option<Arc<ExtensionHub>>,
    pub specs: Vec<PeriodicLazyExtensionSpec>,
    pub trigger: String,
}

impl DbLazyScanHook {
    pub fn new(
        db: DbStore,
        hub: Option<Arc<ExtensionHub>>,
        specs: Vec<PeriodicLazyExtensionSpec>,
    ) -> Self {
        Self {
            db,
            hub,
            specs,
            trigger: "periodic_scan".to_string(),
        }
    }

    pub fn from_hub_lazy_extensions(
        db: DbStore,
        hub: Arc<ExtensionHub>,
        env: Option<&EnvConfig>,
    ) -> Self {
        let specs = PeriodicLazyExtensionSpec::discover_from_hub(&hub, env);
        Self::new(db, Some(hub), specs)
    }

    pub fn with_trigger(mut self, trigger: impl Into<String>) -> Self {
        let trimmed = trigger.into();
        if !trimmed.trim().is_empty() {
            self.trigger = trimmed.trim().to_string();
        }
        self
    }

    pub async fn enqueue_for_targets(
        &self,
        targets: &[LazyScanTarget],
    ) -> Result<PeriodicLazyEnqueueSummary> {
        enqueue_periodic_scan_lazy_jobs(
            &self.db,
            self.hub.as_deref(),
            targets,
            &self.specs,
            &self.trigger,
        )
        .await
    }
}

impl PeriodicLazyEnqueueHook for DbLazyScanHook {
    fn on_batch_flushed<'a>(&'a self, outcomes: &'a [ScanItemOutcome]) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.specs.is_empty() || outcomes.is_empty() {
                return;
            }
            let targets: Vec<LazyScanTarget> = outcomes
                .iter()
                .filter(|o| !o.slug.trim().is_empty())
                .map(LazyScanTarget::from)
                .collect();
            if targets.is_empty() {
                return;
            }
            if let Err(err) = self.enqueue_for_targets(&targets).await {
                tracing::warn!("periodic lazy scan enqueue hook error: {}", err);
            }
        })
    }
}

/// Idempotently enqueues `specs` for every target slug using singleflight deduplication.
pub async fn enqueue_periodic_scan_lazy_jobs(
    db: &DbStore,
    hub: Option<&ExtensionHub>,
    targets: &[LazyScanTarget],
    specs: &[PeriodicLazyExtensionSpec],
    trigger: &str,
) -> Result<PeriodicLazyEnqueueSummary> {
    let mut summary = PeriodicLazyEnqueueSummary::default();
    if specs.is_empty() || targets.is_empty() {
        return Ok(summary);
    }

    let clean_trigger = if trigger.trim().is_empty() {
        "periodic_scan"
    } else {
        trigger.trim()
    };

    for target in targets {
        let clean_slug = target.slug.trim();
        if clean_slug.is_empty() {
            continue;
        }
        summary.targets_examined += 1;
        let query = target.to_query_json();

        for spec in specs {
            summary.jobs_evaluated += 1;
            let req = EnqueueLazyJobRequest::new(clean_slug, &spec.extension_id, &spec.section_id)
                .with_trigger(clean_trigger)
                .with_query(query.clone())
                .with_max_attempts(spec.max_attempts)
                .with_policy(spec.policy.clone());

            match enqueue(db, hub, req).await {
                Ok(out) => {
                    if out.queued {
                        summary.queued += 1;
                    } else if out.deduplicated {
                        summary.deduplicated += 1;
                    } else if out.section_status == LazySectionStatusKind::AwaitingApproval {
                        summary.awaiting_approval += 1;
                    } else if out.section_status == LazySectionStatusKind::NotEnabled {
                        summary.not_enabled += 1;
                    }
                }
                Err(err) => {
                    summary.errors += 1;
                    tracing::warn!(
                        "periodic lazy enqueue failed for slug={} ext={}: {}",
                        clean_slug,
                        spec.extension_id,
                        err
                    );
                }
            }
        }
    }

    Ok(summary)
}

/// Sweeps recent `cached_reports` rows (up to `limit`) and idempotently enqueues `specs`
/// for each property using `trigger = "periodic_scan"`.
pub async fn enqueue_periodic_lazy_sweep_from_cached_reports(
    db: &DbStore,
    hub: Option<&ExtensionHub>,
    specs: &[PeriodicLazyExtensionSpec],
    limit: i64,
) -> Result<PeriodicLazyEnqueueSummary> {
    if specs.is_empty() {
        return Ok(PeriodicLazyEnqueueSummary::default());
    }
    let slugs = db.list_cached_report_slugs(limit).await?;
    let mut targets = Vec::with_capacity(slugs.len());
    for slug in slugs {
        let Some(hdr) = db.get_report_header(&slug).await? else {
            continue;
        };
        let address = if hdr.display_address.trim().is_empty() {
            hdr.slug.clone()
        } else {
            hdr.display_address
        };
        let mut target = LazyScanTarget::new(&hdr.slug, address);
        if let (Some(la), Some(lo)) = (hdr.lat, hdr.lon) {
            target = target.with_coords(la, lo);
        }
        if !hdr.zip.trim().is_empty() {
            target = target.with_zip(hdr.zip);
        }
        targets.push(target);
    }

    enqueue_periodic_scan_lazy_jobs(db, hub, &targets, specs, "periodic_scan").await
}
