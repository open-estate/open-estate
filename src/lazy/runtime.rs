//! Core lazy extension runtime: singleflight enqueue, lease claim & heartbeat,
//! exponential-backoff retry/failure, atomic report patching + cache/ETag invalidation,
//! and idempotent exactly-once section-ready subscriber notifications.

use crate::db::{DbStore, LazyExtensionJobRow};
use crate::extensions::ExtensionHub;
use crate::lazy::patch::{
    build_lazy_sections_map, compute_source_fingerprint, merge_ready_artifacts_into_report,
    next_monotonic_updated_at,
};
use crate::lazy::types::{
    humanize_section_label, CompleteLazyJobOutcome, EnqueueLazyJobOutcome, EnqueueLazyJobRequest,
    LazyArtifactMetaView, LazyExtensionPolicy, LazyJobStatusView, LazySectionStatusKind,
    LazyStatusReport, SectionNotificationOutcome,
};
use crate::search::mailer::{send_section_ready_email, SectionReadyNotification};
use crate::shared::{sanitize_user_warning, EnvConfig, HttpClient};
use crate::web::hooks::ServerExtensionHooks;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

static SLUG_PATCH_LOCKS: Lazy<DashMap<String, Arc<Mutex<()>>>> = Lazy::new(DashMap::new);

fn slug_lock(slug: &str) -> Arc<Mutex<()>> {
    Arc::clone(
        SLUG_PATCH_LOCKS
            .entry(slug.trim().to_ascii_lowercase())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .value(),
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedExtensionPolicy {
    pub enabled: bool,
    pub requires_manual_approval: bool,
    pub min_plan_tier: Option<String>,
    pub cache_ttl_secs: Option<u64>,
}

pub(crate) fn resolve_extension_policy(
    hub: Option<&ExtensionHub>,
    extension_id: &str,
    overrides: &LazyExtensionPolicy,
) -> ResolvedExtensionPolicy {
    let mut enabled = overrides.enabled;
    let mut requires_manual_approval = overrides.requires_manual_approval;
    let mut min_plan_tier = overrides.min_plan_tier.clone();
    let mut cache_ttl_secs = overrides.cache_ttl_secs;

    if let Some(h) = hub {
        if let Some(ext_json) = h.get_extension_json(extension_id, None) {
            if enabled.is_none() {
                enabled = ext_json.get("enabled").and_then(Value::as_bool);
            }
            if cache_ttl_secs.is_none() {
                cache_ttl_secs = ext_json.get("cacheTtlSecs").and_then(Value::as_u64);
            }
            if requires_manual_approval.is_none() {
                requires_manual_approval = ext_json
                    .get("requiresManualApproval")
                    .and_then(Value::as_bool);
            }
            if min_plan_tier.is_none() {
                min_plan_tier = ext_json
                    .get("minPlanTier")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned);
            }
            if let Some(yaml_str) = ext_json.get("manifestYaml").and_then(Value::as_str) {
                if let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml_str) {
                    if requires_manual_approval.is_none() {
                        requires_manual_approval = doc
                            .get("requires_manual_approval")
                            .and_then(|v| v.as_bool());
                    }
                    if min_plan_tier.is_none() {
                        min_plan_tier = doc
                            .get("min_plan_tier")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToOwned::to_owned);
                    }
                    if enabled.is_none() {
                        enabled = doc.get("enabled_by_default").and_then(|v| v.as_bool());
                    }
                }
            }
        } else if enabled.is_none() {
            enabled = Some(false);
        }
    }

    ResolvedExtensionPolicy {
        enabled: enabled.unwrap_or(true),
        requires_manual_approval: requires_manual_approval.unwrap_or(false),
        min_plan_tier,
        cache_ttl_secs,
    }
}

/// Enqueues a lazy extension job with singleflight deduplication (`UNIQUE(slug, extension_id)`).
///
/// Never auto-enqueues disabled extensions (`not_enabled`) or `requires_manual_approval`
/// extensions without `explicit_approval == true` (`awaiting_approval`).
pub async fn enqueue(
    db: &DbStore,
    hub: Option<&ExtensionHub>,
    req: EnqueueLazyJobRequest,
) -> Result<EnqueueLazyJobOutcome> {
    let clean_slug = req.slug.trim().to_string();
    let clean_ext = req.extension_id.trim().to_string();
    let clean_sec = req.section_id.trim().to_string();
    if clean_slug.is_empty() || clean_ext.is_empty() || clean_sec.is_empty() {
        return Err(anyhow!("slug, extension_id, and section_id are required"));
    }

    let policy = resolve_extension_policy(hub, &clean_ext, &req.policy);
    if !policy.enabled {
        let job = db
            .record_lazy_job_policy_state(
                &clean_slug,
                &clean_ext,
                &clean_sec,
                "not_enabled",
                req.requested_by_user_id.as_deref(),
                &req.trigger,
                Some("Extension is disabled by default"),
            )
            .await?;
        return Ok(EnqueueLazyJobOutcome {
            job,
            queued: false,
            deduplicated: false,
            section_status: LazySectionStatusKind::NotEnabled,
        });
    }

    if policy.requires_manual_approval && !req.explicit_approval {
        let job = db
            .record_lazy_job_policy_state(
                &clean_slug,
                &clean_ext,
                &clean_sec,
                "awaiting_approval",
                req.requested_by_user_id.as_deref(),
                &req.trigger,
                Some("Extension requires manual approval before execution"),
            )
            .await?;
        return Ok(EnqueueLazyJobOutcome {
            job,
            queued: false,
            deduplicated: false,
            section_status: LazySectionStatusKind::AwaitingApproval,
        });
    }

    let query_json = req
        .query
        .as_ref()
        .and_then(|v| serde_json::to_string(v).ok());

    let effective_trigger = if policy.requires_manual_approval && req.explicit_approval {
        "approved"
    } else {
        req.trigger.as_str()
    };

    let (job, deduplicated) = db
        .enqueue_lazy_job_singleflight(
            &clean_slug,
            &clean_ext,
            &clean_sec,
            req.requested_by_user_id.as_deref(),
            effective_trigger,
            query_json.as_deref(),
            req.max_attempts,
            req.force_refresh,
        )
        .await?;

    let section_status = LazySectionStatusKind::from_job_status(&job.status);
    let queued = !deduplicated && job.status == "queued";
    Ok(EnqueueLazyJobOutcome {
        job,
        queued,
        deduplicated,
        section_status,
    })
}

pub async fn claim_next(
    db: &DbStore,
    worker_id: &str,
    lease_secs: i64,
) -> Result<Option<LazyExtensionJobRow>> {
    db.claim_next_lazy_job(worker_id, lease_secs).await
}

pub async fn heartbeat(
    db: &DbStore,
    job_id: &str,
    worker_id: &str,
    lease_secs: i64,
) -> Result<bool> {
    db.heartbeat_lazy_job(job_id, worker_id, lease_secs).await
}

/// Re-renders and updates `cached_reports` with merged ready lazy artifacts and section statuses,
/// advancing `updated_at` monotonically (to invalidate CDN ETags) and invalidating `db.l1_reports`.
pub async fn patch_cached_report_for_slug(
    db: &DbStore,
    _env: &EnvConfig,
    slug: &str,
) -> Result<(bool, Option<String>)> {
    let clean_slug = slug.trim();
    if clean_slug.is_empty() {
        return Ok((false, None));
    }

    let lock = slug_lock(clean_slug);
    let _guard = lock.lock().await;

    let Some(mut rec) = db.get_cached_report_from_db_bypass_l1(clean_slug).await? else {
        return Ok((false, None));
    };

    let _ = merge_ready_artifacts_into_report(db, &mut rec.report).await?;
    let new_updated_at = next_monotonic_updated_at(Some(&rec.updated_at));
    if let Some(meta) = rec.report.get_mut("meta").and_then(Value::as_object_mut) {
        meta.insert("lazyUpdatedAt".to_string(), json!(new_updated_at));
    }

    db.patch_cached_report_and_invalidate_l1(&rec.slug, &rec.report, None, &new_updated_at)
        .await?;

    Ok((true, Some(new_updated_at)))
}

/// Notifies active section subscribers at most once per `(subscription, event_key)` using
/// `ServerExtensionHooks::notify_section_ready` when implemented, falling back to SMTP `mailer`.
#[allow(clippy::too_many_arguments)]
pub async fn notify_section_subscribers(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    hooks: Option<&dyn ServerExtensionHooks>,
    slug: &str,
    section_id: &str,
    source_fingerprint: &str,
    base_origin: &str,
    display_address_hint: Option<&str>,
) -> Result<SectionNotificationOutcome> {
    let clean_slug = slug.trim();
    let clean_sec = section_id.trim();
    let subs = db
        .list_active_section_subscriptions(clean_slug, clean_sec)
        .await?;

    let mut outcome = SectionNotificationOutcome {
        checked: subs.len(),
        claimed: 0,
        delivered_via_hook: 0,
        delivered_via_smtp: 0,
        skipped_no_smtp: 0,
        failed: 0,
    };

    if subs.is_empty() {
        return Ok(outcome);
    }

    let resolved_addr = match display_address_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
    {
        Some(a) => a,
        None => db
            .get_cached_report_from_db_bypass_l1(clean_slug)
            .await
            .ok()
            .flatten()
            .map(|r| r.display_address)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| clean_slug.to_string()),
    };

    let section_label = humanize_section_label(clean_sec);
    let eff_origin = if base_origin.trim().is_empty() {
        env.get("PUBLIC_BASE_URL").unwrap_or("").trim().to_string()
    } else {
        base_origin.trim().to_string()
    };
    let clean_origin = eff_origin.trim_end_matches('/');

    for sub in subs {
        let notify_event_key = format!(
            "sec_ready:{}:{}:{}:{}",
            clean_slug,
            clean_sec,
            sub.id,
            source_fingerprint.trim()
        );
        let claimed = db
            .try_claim_section_subscription_notification(&sub.id, &notify_event_key)
            .await?;
        if !claimed {
            continue;
        }
        outcome.claimed += 1;

        let deep_link_url = if clean_origin.is_empty() {
            format!("/r/{clean_slug}/#{clean_sec}")
        } else {
            format!("{clean_origin}/r/{clean_slug}/#{clean_sec}")
        };
        let encoded_tok = urlencoding::encode(sub.unsubscribe_token.trim());
        let unsubscribe_url = if clean_origin.is_empty() {
            format!("/u/section/{encoded_tok}")
        } else {
            format!("{clean_origin}/u/section/{encoded_tok}")
        };

        let notif = SectionReadyNotification {
            subscription_id: sub.id.clone(),
            slug: clean_slug.to_string(),
            section_id: clean_sec.to_string(),
            section_label: section_label.clone(),
            display_address: resolved_addr.clone(),
            email: sub.email.clone(),
            user_id: sub.user_id.clone(),
            unsubscribe_token: sub.unsubscribe_token.clone(),
            unsubscribe_url,
            deep_link_url,
            event_key: notify_event_key,
            base_origin: clean_origin.to_string(),
        };

        let handled_by_hook = if let Some(h) = hooks {
            match h.notify_section_ready(db, http, env, &notif).await {
                Ok(handled) => handled,
                Err(err) => {
                    let _ = db
                        .update_section_subscription_notes(&sub.id, &format!("hook_error: {err}"))
                        .await;
                    outcome.failed += 1;
                    continue;
                }
            }
        } else {
            false
        };

        if handled_by_hook {
            outcome.delivered_via_hook += 1;
            let _ = db
                .update_section_subscription_notes(&sub.id, "delivered_via_hook")
                .await;
        } else if env.smtp_configured() {
            match send_section_ready_email(env, &notif).await {
                Ok(_) => {
                    outcome.delivered_via_smtp += 1;
                    let _ = db
                        .update_section_subscription_notes(&sub.id, "delivered_via_smtp")
                        .await;
                }
                Err(err) => {
                    outcome.failed += 1;
                    let _ = db
                        .update_section_subscription_notes(&sub.id, &format!("smtp_error: {err}"))
                        .await;
                }
            }
        } else {
            outcome.skipped_no_smtp += 1;
            let _ = db
                .update_section_subscription_notes(&sub.id, "queued_no_smtp")
                .await;
        }
    }

    Ok(outcome)
}

/// Marks a claimed/running lazy job as succeeded, persists `property_lazy_artifacts`,
/// patches `cached_reports` + invalidates L1 & CDN ETag, and notifies subscribers exactly once.
#[allow(clippy::too_many_arguments)]
pub async fn complete(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    hooks: Option<&dyn ServerExtensionHooks>,
    job_id: &str,
    payload: Value,
    source_fingerprint: Option<&str>,
    required_tier: Option<&str>,
    expires_at: Option<i64>,
    base_origin: &str,
) -> Result<CompleteLazyJobOutcome> {
    let job = db
        .get_lazy_job_by_id(job_id)
        .await?
        .ok_or_else(|| anyhow!("Lazy job '{job_id}' not found"))?;

    let succeeded_job = db.mark_lazy_job_succeeded(&job.id).await?;
    let fp = source_fingerprint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| compute_source_fingerprint(&payload));
    let payload_json = serde_json::to_string(&payload)?;

    let artifact = db
        .upsert_lazy_artifact(
            &job.slug,
            &job.extension_id,
            &job.section_id,
            "ready",
            &payload_json,
            &fp,
            required_tier,
            expires_at,
        )
        .await?;

    let (report_patched, new_report_updated_at) =
        patch_cached_report_for_slug(db, env, &job.slug).await?;

    let notifications = notify_section_subscribers(
        db,
        http,
        env,
        hooks,
        &job.slug,
        &job.section_id,
        &artifact.source_fingerprint,
        base_origin,
        None,
    )
    .await?;

    Ok(CompleteLazyJobOutcome {
        job: succeeded_job,
        artifact,
        report_patched,
        new_report_updated_at,
        notifications,
    })
}

/// Convenience completion API for domain pipelines (e.g. disclosures) publishing an artifact
/// directly by `(slug, extension_id, section_id)`.
#[allow(clippy::too_many_arguments)]
pub async fn complete_section_artifact(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    hooks: Option<&dyn ServerExtensionHooks>,
    slug: &str,
    extension_id: &str,
    section_id: &str,
    payload: Value,
    source_fingerprint: Option<&str>,
    required_tier: Option<&str>,
    expires_at: Option<i64>,
    base_origin: &str,
) -> Result<CompleteLazyJobOutcome> {
    let clean_slug = slug.trim();
    let clean_ext = extension_id.trim();
    let clean_sec = section_id.trim();

    let job = if let Some(existing) = db.get_lazy_job(clean_slug, clean_ext).await? {
        db.mark_lazy_job_succeeded(&existing.id).await?
    } else {
        None
    };

    let fp = source_fingerprint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| compute_source_fingerprint(&payload));
    let payload_json = serde_json::to_string(&payload)?;

    let artifact = db
        .upsert_lazy_artifact(
            clean_slug,
            clean_ext,
            clean_sec,
            "ready",
            &payload_json,
            &fp,
            required_tier,
            expires_at,
        )
        .await?;

    let (report_patched, new_report_updated_at) =
        patch_cached_report_for_slug(db, env, clean_slug).await?;

    let notifications = notify_section_subscribers(
        db,
        http,
        env,
        hooks,
        clean_slug,
        clean_sec,
        &artifact.source_fingerprint,
        base_origin,
        None,
    )
    .await?;

    Ok(CompleteLazyJobOutcome {
        job,
        artifact,
        report_patched,
        new_report_updated_at,
        notifications,
    })
}

/// Transitions a section artifact to `awaiting_external_response` (e.g., pending email reply)
/// and updates `cached_reports` + invalidates L1 cache without notifying ready subscribers.
#[allow(clippy::too_many_arguments)]
pub async fn mark_awaiting_external_response(
    db: &DbStore,
    env: &EnvConfig,
    slug: &str,
    extension_id: &str,
    section_id: &str,
    metadata: Value,
    required_tier: Option<&str>,
    expires_at: Option<i64>,
) -> Result<()> {
    let clean_slug = slug.trim();
    let clean_ext = extension_id.trim();
    let clean_sec = section_id.trim();

    if let Some(existing) = db.get_lazy_job(clean_slug, clean_ext).await? {
        db.mark_lazy_job_succeeded(&existing.id).await?;
    }

    let fp = compute_source_fingerprint(&metadata);
    let payload_json = serde_json::to_string(&metadata)?;
    db.upsert_lazy_artifact(
        clean_slug,
        clean_ext,
        clean_sec,
        "awaiting_external_response",
        &payload_json,
        &fp,
        required_tier,
        expires_at,
    )
    .await?;

    let _ = patch_cached_report_for_slug(db, env, clean_slug).await?;
    Ok(())
}

/// Records an execution failure with exponential backoff retry or terminal `failed` status.
pub async fn fail(
    db: &DbStore,
    env: &EnvConfig,
    job_id: &str,
    error_msg: &str,
    base_backoff_secs: i64,
    max_backoff_secs: i64,
) -> Result<Option<LazyExtensionJobRow>> {
    let clean_err = sanitize_user_warning(error_msg);
    let updated = db
        .fail_lazy_job(job_id, &clean_err, base_backoff_secs, max_backoff_secs)
        .await?;
    if let Some(ref row) = updated {
        if row.status == "failed" {
            let _ = patch_cached_report_for_slug(db, env, &row.slug).await;
        }
    }
    Ok(updated)
}

/// Returns redacted section, job, and artifact metadata statuses for a property report slug.
pub async fn lazy_section_status(db: &DbStore, slug: &str) -> Result<LazyStatusReport> {
    let clean_slug = slug.trim();
    let artifacts = db.list_lazy_artifacts_for_slug(clean_slug).await?;
    let jobs = db.list_lazy_jobs_for_slug(clean_slug).await?;
    let sections = build_lazy_sections_map(&artifacts, &jobs);

    Ok(LazyStatusReport {
        ok: true,
        slug: clean_slug.to_string(),
        sections,
        jobs: jobs.iter().map(LazyJobStatusView::from).collect(),
        artifacts: artifacts.iter().map(LazyArtifactMetaView::from).collect(),
    })
}
