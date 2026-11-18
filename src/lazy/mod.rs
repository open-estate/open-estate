//! Asynchronous lazy extension execution, persisted artifact patching,
//! cache/ETag invalidation, and section-ready email notifications (`plan §4`).

pub mod executor;
pub mod patch;
pub mod routes;
pub mod runtime;
pub mod types;

pub use executor::{
    spawn_lazy_worker, LazyExecResult, LazyExecutor, LazyWorker, LazyWorkerConfig, LazyWorkerHandle,
};
pub use patch::{
    apply_artifact_payload_to_report, apply_artifacts_and_jobs_to_report, build_lazy_sections_map,
    compute_source_fingerprint, merge_ready_artifacts_into_report, next_monotonic_updated_at,
};
pub use routes::lazy_routes;
pub use runtime::{
    claim_next, complete, complete_section_artifact, enqueue, fail, heartbeat, lazy_section_status,
    mark_awaiting_external_response, notify_section_subscribers, patch_cached_report_for_slug,
};
pub use types::{
    humanize_section_label, CompleteLazyJobOutcome, EnqueueLazyJobOutcome, EnqueueLazyJobRequest,
    LazyArtifactMetaView, LazyExtensionPolicy, LazyJobStatusView, LazySectionEntry,
    LazySectionStatusKind, LazyStatusReport, SectionNotificationOutcome,
    SectionSubscriptionPublicView,
};
