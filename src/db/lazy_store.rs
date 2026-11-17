//! Dialect-portable SQLite WAL + MySQL 8.4/9.0 persistence layer for lazy extension jobs,
//! persisted lazy artifacts (`property_lazy_artifacts`), and section-ready subscriptions
//! (`report_section_subscriptions`).

use crate::db::{DbRowExt, DbStore, SqlParam};
use crate::shared::{now_iso, now_unix_secs};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LazyExtensionJobRow {
    pub id: String,
    pub slug: String,
    pub extension_id: String,
    pub section_id: String,
    pub status: String,
    pub attempts: i64,
    pub max_attempts: i64,
    pub next_attempt_at: i64,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub requested_by_user_id: Option<String>,
    #[serde(rename = "trigger")]
    pub trigger_kind: String,
    pub query_json: Option<String>,
    pub last_error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PropertyLazyArtifactRow {
    pub id: String,
    pub slug: String,
    pub extension_id: String,
    pub section_id: String,
    pub status: String,
    pub payload_json: String,
    pub source_fingerprint: String,
    pub required_tier: Option<String>,
    pub fetched_at: String,
    pub expires_at: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportSectionSubscriptionRow {
    pub id: String,
    pub slug: String,
    pub section_id: String,
    pub user_id: Option<String>,
    pub email: String,
    pub unsubscribe_token: String,
    pub status: String,
    pub created_at: String,
    pub notified_at: Option<String>,
    pub notified_event_key: Option<String>,
    pub event_key: String,
    pub last_notes: Option<String>,
    pub updated_at: String,
}

fn decode_lazy_job(r: &dyn DbRowExt) -> LazyExtensionJobRow {
    LazyExtensionJobRow {
        id: r.opt_str("id").unwrap_or_default(),
        slug: r.opt_str("slug").unwrap_or_default(),
        extension_id: r.opt_str("extension_id").unwrap_or_default(),
        section_id: r.opt_str("section_id").unwrap_or_default(),
        status: r.opt_str("status").unwrap_or_else(|| "queued".to_string()),
        attempts: r.opt_i64("attempts").unwrap_or(0),
        max_attempts: r.opt_i64("max_attempts").unwrap_or(5),
        next_attempt_at: r.opt_i64("next_attempt_at").unwrap_or(0),
        lease_owner: r.opt_str("lease_owner"),
        lease_expires_at: r.opt_i64("lease_expires_at"),
        requested_by_user_id: r.opt_str("requested_by_user_id"),
        trigger_kind: r
            .opt_str("trigger_kind")
            .unwrap_or_else(|| "first_visit".to_string()),
        query_json: r.opt_str("query_json"),
        last_error: r.opt_str("last_error"),
        started_at: r.opt_str("started_at"),
        finished_at: r.opt_str("finished_at"),
        created_at: r.opt_str("created_at").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

fn decode_lazy_artifact(r: &dyn DbRowExt) -> PropertyLazyArtifactRow {
    PropertyLazyArtifactRow {
        id: r.opt_str("id").unwrap_or_default(),
        slug: r.opt_str("slug").unwrap_or_default(),
        extension_id: r.opt_str("extension_id").unwrap_or_default(),
        section_id: r.opt_str("section_id").unwrap_or_default(),
        status: r.opt_str("status").unwrap_or_else(|| "ready".to_string()),
        payload_json: r
            .opt_str("payload_json")
            .unwrap_or_else(|| "{}".to_string()),
        source_fingerprint: r.opt_str("source_fingerprint").unwrap_or_default(),
        required_tier: r.opt_str("required_tier"),
        fetched_at: r.opt_str("fetched_at").unwrap_or_default(),
        expires_at: r.opt_i64("expires_at"),
        created_at: r.opt_str("created_at").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

fn decode_section_subscription(r: &dyn DbRowExt) -> ReportSectionSubscriptionRow {
    ReportSectionSubscriptionRow {
        id: r.opt_str("id").unwrap_or_default(),
        slug: r.opt_str("slug").unwrap_or_default(),
        section_id: r.opt_str("section_id").unwrap_or_default(),
        user_id: r.opt_str("user_id"),
        email: r.opt_str("email").unwrap_or_default(),
        unsubscribe_token: r.opt_str("unsubscribe_token").unwrap_or_default(),
        status: r.opt_str("status").unwrap_or_else(|| "active".to_string()),
        created_at: r.opt_str("created_at").unwrap_or_default(),
        notified_at: r.opt_str("notified_at"),
        notified_event_key: r.opt_str("notified_event_key"),
        event_key: r.opt_str("event_key").unwrap_or_default(),
        last_notes: r.opt_str("last_notes"),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

pub fn deterministic_section_sub_event_key(slug: &str, section_id: &str, email: &str) -> String {
    let norm = format!(
        "{}|{}|{}",
        slug.trim().to_ascii_lowercase(),
        section_id.trim(),
        email.trim().to_ascii_lowercase()
    );
    let mut h = Sha256::new();
    h.update(norm.as_bytes());
    format!("secsub_{}", hex::encode(h.finalize()))
}

fn generate_section_unsub_token() -> String {
    let mut h = Sha256::new();
    h.update(Uuid::new_v4().as_bytes());
    h.update(Uuid::new_v4().as_bytes());
    hex::encode(h.finalize())[..48].to_string()
}

pub fn compute_exponential_backoff_secs(
    next_attempt: i64,
    base_backoff_secs: i64,
    max_backoff_secs: i64,
) -> i64 {
    let base = base_backoff_secs.max(1);
    let cap = max_backoff_secs.max(base);
    let shift = next_attempt.saturating_sub(1).clamp(0, 16) as u32;
    base.saturating_mul(1_i64.checked_shl(shift).unwrap_or(cap))
        .min(cap)
}

impl DbStore {
    pub async fn get_lazy_job(
        &self,
        slug: &str,
        extension_id: &str,
    ) -> Result<Option<LazyExtensionJobRow>> {
        self.ensure_schema().await?;
        let clean_slug = slug.trim();
        let clean_ext = extension_id.trim();
        if clean_slug.is_empty() || clean_ext.is_empty() {
            return Ok(None);
        }
        self.fetch_optional_mapped(
            "SELECT * FROM lazy_extension_jobs WHERE slug = ? AND extension_id = ?",
            &[clean_slug.into(), clean_ext.into()],
            decode_lazy_job,
        )
        .await
    }

    pub async fn get_lazy_job_by_id(&self, job_id: &str) -> Result<Option<LazyExtensionJobRow>> {
        self.ensure_schema().await?;
        let clean_id = job_id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.fetch_optional_mapped(
            "SELECT * FROM lazy_extension_jobs WHERE id = ?",
            &[clean_id.into()],
            decode_lazy_job,
        )
        .await
    }

    pub async fn list_lazy_jobs_for_slug(&self, slug: &str) -> Result<Vec<LazyExtensionJobRow>> {
        self.ensure_schema().await?;
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all_mapped(
            "SELECT * FROM lazy_extension_jobs WHERE slug = ? ORDER BY created_at ASC, extension_id ASC",
            &[clean_slug.into()],
            decode_lazy_job,
        )
        .await
    }

    /// Singleflight enqueue in `lazy_extension_jobs` keyed on `UNIQUE(slug, extension_id)`.
    /// Returns `(row, deduplicated)` where `deduplicated == false` only for the single caller
    /// that transitions or creates the active `'queued'` job.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_lazy_job_singleflight(
        &self,
        slug: &str,
        extension_id: &str,
        section_id: &str,
        requested_by_user_id: Option<&str>,
        trigger_kind: &str,
        query_json: Option<&str>,
        max_attempts: i64,
        force_refresh: bool,
    ) -> Result<(LazyExtensionJobRow, bool)> {
        self.ensure_schema().await?;
        let clean_slug = slug.trim();
        let clean_ext = extension_id.trim();
        let clean_sec = section_id.trim();
        if clean_slug.is_empty() || clean_ext.is_empty() || clean_sec.is_empty() {
            return Err(anyhow!("slug, extension_id, and section_id are required"));
        }

        let now_str = now_iso();
        let now_secs = now_unix_secs();
        let id = format!("lj_{}", Uuid::new_v4().simple());
        let max_att = max_attempts.clamp(1, 20);
        let trig = trigger_kind.trim();
        let trig = if trig.is_empty() { "first_visit" } else { trig };

        let affected = self
            .insert_ignore_row(
                "lazy_extension_jobs",
                &[
                    ("id", id.clone().into()),
                    ("slug", clean_slug.into()),
                    ("extension_id", clean_ext.into()),
                    ("section_id", clean_sec.into()),
                    ("status", "queued".into()),
                    ("attempts", 0_i64.into()),
                    ("max_attempts", max_att.into()),
                    ("next_attempt_at", now_secs.into()),
                    ("lease_owner", SqlParam::Null),
                    ("lease_expires_at", SqlParam::Null),
                    ("requested_by_user_id", requested_by_user_id.into()),
                    ("trigger_kind", trig.into()),
                    ("query_json", query_json.into()),
                    ("last_error", SqlParam::Null),
                    ("started_at", SqlParam::Null),
                    ("finished_at", SqlParam::Null),
                    ("created_at", now_str.clone().into()),
                    ("updated_at", now_str.clone().into()),
                ],
            )
            .await?;

        if affected > 0 {
            if let Some(created) = self.get_lazy_job(clean_slug, clean_ext).await? {
                return Ok((created, false));
            }
        }

        let existing = self
            .get_lazy_job(clean_slug, clean_ext)
            .await?
            .ok_or_else(|| anyhow!("Failed to load lazy job after singleflight conflict"))?;

        let can_requeue = match existing.status.as_str() {
            "queued" | "running" => false,
            "succeeded" => force_refresh,
            "failed" | "cancelled" | "awaiting_approval" | "not_enabled" => true,
            _ => force_refresh,
        };

        if !can_requeue {
            return Ok((existing, true));
        }

        let requeued_rows = if force_refresh {
            self.execute_params(
                "UPDATE lazy_extension_jobs
                 SET section_id = ?,
                     status = 'queued',
                     attempts = 0,
                     max_attempts = ?,
                     next_attempt_at = ?,
                     lease_owner = NULL,
                     lease_expires_at = NULL,
                     requested_by_user_id = COALESCE(?, requested_by_user_id),
                     trigger_kind = ?,
                     query_json = COALESCE(?, query_json),
                     last_error = NULL,
                     started_at = NULL,
                     finished_at = NULL,
                     updated_at = ?
                 WHERE id = ? AND status IN ('succeeded', 'failed', 'cancelled', 'awaiting_approval', 'not_enabled')",
                &[
                    clean_sec.into(),
                    max_att.into(),
                    now_secs.into(),
                    requested_by_user_id.into(),
                    trig.into(),
                    query_json.into(),
                    now_str.into(),
                    existing.id.as_str().into(),
                ],
            )
            .await?
        } else {
            self.execute_params(
                "UPDATE lazy_extension_jobs
                 SET section_id = ?,
                     status = 'queued',
                     attempts = 0,
                     max_attempts = ?,
                     next_attempt_at = ?,
                     lease_owner = NULL,
                     lease_expires_at = NULL,
                     requested_by_user_id = COALESCE(?, requested_by_user_id),
                     trigger_kind = ?,
                     query_json = COALESCE(?, query_json),
                     last_error = NULL,
                     started_at = NULL,
                     finished_at = NULL,
                     updated_at = ?
                 WHERE id = ? AND status IN ('failed', 'cancelled', 'awaiting_approval', 'not_enabled')",
                &[
                    clean_sec.into(),
                    max_att.into(),
                    now_secs.into(),
                    requested_by_user_id.into(),
                    trig.into(),
                    query_json.into(),
                    now_str.into(),
                    existing.id.as_str().into(),
                ],
            )
            .await?
        };

        let latest = self
            .get_lazy_job(clean_slug, clean_ext)
            .await?
            .unwrap_or(existing);
        Ok((latest, requeued_rows == 0))
    }

    /// Records a non-runnable policy state (`awaiting_approval` or `not_enabled`) without
    /// ever putting the row into `'queued'`. Never overwrites an active `'running'` or `'succeeded'` job.
    pub async fn record_lazy_job_policy_state(
        &self,
        slug: &str,
        extension_id: &str,
        section_id: &str,
        policy_status: &str,
        requested_by_user_id: Option<&str>,
        trigger_kind: &str,
        reason: Option<&str>,
    ) -> Result<LazyExtensionJobRow> {
        self.ensure_schema().await?;
        let clean_slug = slug.trim();
        let clean_ext = extension_id.trim();
        let clean_sec = section_id.trim();
        if !matches!(policy_status, "awaiting_approval" | "not_enabled") {
            return Err(anyhow!("Invalid policy status '{policy_status}'"));
        }

        let now_str = now_iso();
        let id = format!("lj_{}", Uuid::new_v4().simple());
        let _ = self
            .insert_ignore_row(
                "lazy_extension_jobs",
                &[
                    ("id", id.into()),
                    ("slug", clean_slug.into()),
                    ("extension_id", clean_ext.into()),
                    ("section_id", clean_sec.into()),
                    ("status", policy_status.into()),
                    ("attempts", 0_i64.into()),
                    ("max_attempts", 5_i64.into()),
                    ("next_attempt_at", i64::MAX.into()),
                    ("lease_owner", SqlParam::Null),
                    ("lease_expires_at", SqlParam::Null),
                    ("requested_by_user_id", requested_by_user_id.into()),
                    ("trigger_kind", trigger_kind.into()),
                    ("query_json", SqlParam::Null),
                    ("last_error", reason.into()),
                    ("started_at", SqlParam::Null),
                    ("finished_at", SqlParam::Null),
                    ("created_at", now_str.clone().into()),
                    ("updated_at", now_str.clone().into()),
                ],
            )
            .await?;

        let _ = self
            .execute_params(
                "UPDATE lazy_extension_jobs
                 SET status = ?, last_error = COALESCE(?, last_error), updated_at = ?
                 WHERE slug = ? AND extension_id = ? AND status IN ('awaiting_approval', 'not_enabled')",
                &[
                    policy_status.into(),
                    reason.into(),
                    now_str.into(),
                    clean_slug.into(),
                    clean_ext.into(),
                ],
            )
            .await?;

        self.get_lazy_job(clean_slug, clean_ext)
            .await?
            .ok_or_else(|| anyhow!("Failed to load lazy job policy state"))
    }

    /// Atomically claims the next due `'queued'` job or expired `'running'` lease.
    pub async fn claim_next_lazy_job(
        &self,
        worker_id: &str,
        lease_secs: i64,
    ) -> Result<Option<LazyExtensionJobRow>> {
        self.ensure_schema().await?;
        let clean_worker = worker_id.trim();
        if clean_worker.is_empty() {
            return Err(anyhow!("worker_id is required to claim a lazy job"));
        }
        let lease_dur = lease_secs.clamp(5, 3_600);

        for _attempt in 0..8 {
            let now_secs = now_unix_secs();
            let now_str = now_iso();
            let lease_until = now_secs + lease_dur;

            let candidates = self
                .fetch_all_mapped(
                    "SELECT * FROM lazy_extension_jobs
                     WHERE (status = 'queued' AND next_attempt_at <= ?)
                        OR (status = 'running' AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?)
                     ORDER BY next_attempt_at ASC, created_at ASC
                     LIMIT 8",
                    &[now_secs.into(), now_secs.into()],
                    decode_lazy_job,
                )
                .await?;

            if candidates.is_empty() {
                return Ok(None);
            }

            for cand in candidates {
                let claimed = if cand.status == "queued" {
                    self.execute_params(
                        "UPDATE lazy_extension_jobs
                         SET status = 'running',
                             lease_owner = ?,
                             lease_expires_at = ?,
                             started_at = COALESCE(started_at, ?),
                             updated_at = ?
                         WHERE id = ? AND status = 'queued' AND next_attempt_at <= ?",
                        &[
                            clean_worker.into(),
                            lease_until.into(),
                            now_str.clone().into(),
                            now_str.clone().into(),
                            cand.id.as_str().into(),
                            now_secs.into(),
                        ],
                    )
                    .await?
                } else {
                    self.execute_params(
                        "UPDATE lazy_extension_jobs
                         SET lease_owner = ?,
                             lease_expires_at = ?,
                             updated_at = ?
                         WHERE id = ? AND status = 'running' AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?",
                        &[
                            clean_worker.into(),
                            lease_until.into(),
                            now_str.clone().into(),
                            cand.id.as_str().into(),
                            now_secs.into(),
                        ],
                    )
                    .await?
                };

                if claimed == 1 {
                    return self.get_lazy_job_by_id(&cand.id).await;
                }
            }
        }

        Ok(None)
    }

    pub async fn heartbeat_lazy_job(
        &self,
        job_id: &str,
        worker_id: &str,
        lease_secs: i64,
    ) -> Result<bool> {
        self.ensure_schema().await?;
        let now_secs = now_unix_secs();
        let lease_until = now_secs + lease_secs.clamp(5, 3_600);
        let now_str = now_iso();
        let affected = self
            .execute_params(
                "UPDATE lazy_extension_jobs
                 SET lease_expires_at = ?, updated_at = ?
                 WHERE id = ? AND status = 'running' AND lease_owner = ?",
                &[
                    lease_until.into(),
                    now_str.into(),
                    job_id.trim().into(),
                    worker_id.trim().into(),
                ],
            )
            .await?;
        Ok(affected == 1)
    }

    pub async fn mark_lazy_job_succeeded(
        &self,
        job_id: &str,
    ) -> Result<Option<LazyExtensionJobRow>> {
        self.ensure_schema().await?;
        let now_str = now_iso();
        let clean_id = job_id.trim();
        self.execute_params(
            "UPDATE lazy_extension_jobs
             SET status = 'succeeded',
                 lease_owner = NULL,
                 lease_expires_at = NULL,
                 last_error = NULL,
                 finished_at = ?,
                 updated_at = ?
             WHERE id = ?",
            &[now_str.clone().into(), now_str.into(), clean_id.into()],
        )
        .await?;
        self.get_lazy_job_by_id(clean_id).await
    }

    pub async fn fail_lazy_job(
        &self,
        job_id: &str,
        error_msg: &str,
        base_backoff_secs: i64,
        max_backoff_secs: i64,
    ) -> Result<Option<LazyExtensionJobRow>> {
        self.ensure_schema().await?;
        let clean_id = job_id.trim();
        let Some(existing) = self.get_lazy_job_by_id(clean_id).await? else {
            return Ok(None);
        };
        let next_attempts = existing.attempts + 1;
        let now_secs = now_unix_secs();
        let now_str = now_iso();
        let err_short: String = error_msg.trim().chars().take(1_000).collect();

        if next_attempts >= existing.max_attempts.max(1) {
            self.execute_params(
                "UPDATE lazy_extension_jobs
                 SET status = 'failed',
                     attempts = ?,
                     lease_owner = NULL,
                     lease_expires_at = NULL,
                     last_error = ?,
                     finished_at = ?,
                     updated_at = ?
                 WHERE id = ?",
                &[
                    next_attempts.into(),
                    err_short.into(),
                    now_str.clone().into(),
                    now_str.into(),
                    clean_id.into(),
                ],
            )
            .await?;
        } else {
            let backoff = compute_exponential_backoff_secs(
                next_attempts,
                base_backoff_secs,
                max_backoff_secs,
            );
            let next_at = now_secs + backoff;
            self.execute_params(
                "UPDATE lazy_extension_jobs
                 SET status = 'queued',
                     attempts = ?,
                     next_attempt_at = ?,
                     lease_owner = NULL,
                     lease_expires_at = NULL,
                     last_error = ?,
                     updated_at = ?
                 WHERE id = ?",
                &[
                    next_attempts.into(),
                    next_at.into(),
                    err_short.into(),
                    now_str.into(),
                    clean_id.into(),
                ],
            )
            .await?;
        }

        self.get_lazy_job_by_id(clean_id).await
    }

    pub async fn cancel_lazy_job(&self, slug: &str, extension_id: &str) -> Result<bool> {
        self.ensure_schema().await?;
        let now_str = now_iso();
        let affected = self
            .execute_params(
                "UPDATE lazy_extension_jobs
                 SET status = 'cancelled',
                     lease_owner = NULL,
                     lease_expires_at = NULL,
                     finished_at = ?,
                     updated_at = ?
                 WHERE slug = ? AND extension_id = ? AND status IN ('queued', 'running', 'awaiting_approval')",
                &[
                    now_str.clone().into(),
                    now_str.into(),
                    slug.trim().into(),
                    extension_id.trim().into(),
                ],
            )
            .await?;
        Ok(affected > 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_lazy_artifact(
        &self,
        slug: &str,
        extension_id: &str,
        section_id: &str,
        status: &str,
        payload_json: &str,
        source_fingerprint: &str,
        required_tier: Option<&str>,
        expires_at: Option<i64>,
    ) -> Result<PropertyLazyArtifactRow> {
        self.ensure_schema().await?;
        let clean_slug = slug.trim();
        let clean_ext = extension_id.trim();
        let clean_sec = section_id.trim();
        let now_str = now_iso();
        let id = format!("art_{}", Uuid::new_v4().simple());

        self.upsert_row(
            "property_lazy_artifacts",
            &[
                ("id", id.into()),
                ("slug", clean_slug.into()),
                ("extension_id", clean_ext.into()),
                ("section_id", clean_sec.into()),
                ("status", status.trim().into()),
                ("payload_json", payload_json.into()),
                ("source_fingerprint", source_fingerprint.trim().into()),
                ("required_tier", required_tier.into()),
                ("fetched_at", now_str.clone().into()),
                ("expires_at", expires_at.into()),
                ("created_at", now_str.clone().into()),
                ("updated_at", now_str.into()),
            ],
            &["slug", "extension_id"],
        )
        .await?;

        self.get_lazy_artifact(clean_slug, clean_ext)
            .await?
            .ok_or_else(|| anyhow!("Failed to fetch upserted lazy artifact"))
    }

    pub async fn get_lazy_artifact(
        &self,
        slug: &str,
        extension_id: &str,
    ) -> Result<Option<PropertyLazyArtifactRow>> {
        self.ensure_schema().await?;
        self.fetch_optional_mapped(
            "SELECT * FROM property_lazy_artifacts WHERE slug = ? AND extension_id = ?",
            &[slug.trim().into(), extension_id.trim().into()],
            decode_lazy_artifact,
        )
        .await
    }

    pub async fn list_lazy_artifacts_for_slug(
        &self,
        slug: &str,
    ) -> Result<Vec<PropertyLazyArtifactRow>> {
        self.ensure_schema().await?;
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all_mapped(
            "SELECT * FROM property_lazy_artifacts WHERE slug = ? ORDER BY updated_at ASC, extension_id ASC",
            &[clean_slug.into()],
            decode_lazy_artifact,
        )
        .await
    }

    pub async fn upsert_section_subscription(
        &self,
        slug: &str,
        section_id: &str,
        user_id: Option<&str>,
        email: &str,
    ) -> Result<(ReportSectionSubscriptionRow, bool)> {
        self.ensure_schema().await?;
        let clean_slug = slug.trim();
        let clean_sec = section_id.trim();
        let clean_email = email.trim().to_ascii_lowercase();
        let event_key = deterministic_section_sub_event_key(clean_slug, clean_sec, &clean_email);
        let now_str = now_iso();
        let id = format!("subsec_{}", Uuid::new_v4().simple());
        let unsub_token = generate_section_unsub_token();

        let affected = self
            .insert_ignore_row(
                "report_section_subscriptions",
                &[
                    ("id", id.into()),
                    ("slug", clean_slug.into()),
                    ("section_id", clean_sec.into()),
                    ("user_id", user_id.into()),
                    ("email", clean_email.as_str().into()),
                    ("unsubscribe_token", unsub_token.into()),
                    ("status", "active".into()),
                    ("created_at", now_str.clone().into()),
                    ("notified_at", SqlParam::Null),
                    ("notified_event_key", SqlParam::Null),
                    ("event_key", event_key.as_str().into()),
                    ("last_notes", SqlParam::Null),
                    ("updated_at", now_str.clone().into()),
                ],
            )
            .await?;

        let existing = self
            .fetch_optional_mapped(
                "SELECT * FROM report_section_subscriptions WHERE event_key = ?",
                &[event_key.as_str().into()],
                decode_section_subscription,
            )
            .await?
            .ok_or_else(|| anyhow!("Failed to load section subscription after insert"))?;

        if affected > 0 {
            return Ok((existing, true));
        }

        if existing.status == "unsubscribed" {
            let reactivated = self
                .execute_params(
                    "UPDATE report_section_subscriptions
                     SET status = 'active',
                         notified_at = NULL,
                         notified_event_key = NULL,
                         user_id = COALESCE(?, user_id),
                         updated_at = ?
                     WHERE id = ? AND status = 'unsubscribed'",
                    &[user_id.into(), now_str.into(), existing.id.as_str().into()],
                )
                .await?;
            let row = self
                .fetch_optional_mapped(
                    "SELECT * FROM report_section_subscriptions WHERE id = ?",
                    &[existing.id.as_str().into()],
                    decode_section_subscription,
                )
                .await?
                .unwrap_or(existing);
            return Ok((row, reactivated == 1));
        }

        Ok((existing, false))
    }

    pub async fn get_section_subscription_by_token(
        &self,
        token: &str,
    ) -> Result<Option<ReportSectionSubscriptionRow>> {
        self.ensure_schema().await?;
        let clean = token.trim();
        if clean.is_empty() {
            return Ok(None);
        }
        self.fetch_optional_mapped(
            "SELECT * FROM report_section_subscriptions WHERE unsubscribe_token = ?",
            &[clean.into()],
            decode_section_subscription,
        )
        .await
    }

    pub async fn unsubscribe_section_by_token(
        &self,
        token: &str,
        slug_filter: Option<&str>,
    ) -> Result<Option<ReportSectionSubscriptionRow>> {
        self.ensure_schema().await?;
        let clean = token.trim();
        if clean.is_empty() {
            return Ok(None);
        }
        let existing = self.get_section_subscription_by_token(clean).await?;
        let Some(row) = existing else {
            return Ok(None);
        };
        if let Some(slug_f) = slug_filter.map(str::trim).filter(|s| !s.is_empty()) {
            if !row.slug.eq_ignore_ascii_case(slug_f) {
                return Ok(None);
            }
        }
        let now_str = now_iso();
        self.execute_params(
            "UPDATE report_section_subscriptions SET status = 'unsubscribed', updated_at = ? WHERE id = ?",
            &[now_str.into(), row.id.as_str().into()],
        )
        .await?;
        self.fetch_optional_mapped(
            "SELECT * FROM report_section_subscriptions WHERE id = ?",
            &[row.id.as_str().into()],
            decode_section_subscription,
        )
        .await
    }

    pub async fn list_active_section_subscriptions(
        &self,
        slug: &str,
        section_id: &str,
    ) -> Result<Vec<ReportSectionSubscriptionRow>> {
        self.ensure_schema().await?;
        self.fetch_all_mapped(
            "SELECT * FROM report_section_subscriptions
             WHERE slug = ? AND section_id = ? AND status = 'active'
             ORDER BY created_at ASC",
            &[slug.trim().into(), section_id.trim().into()],
            decode_section_subscription,
        )
        .await
    }

    pub async fn list_section_subscriptions_for_slug(
        &self,
        slug: &str,
    ) -> Result<Vec<ReportSectionSubscriptionRow>> {
        self.ensure_schema().await?;
        self.fetch_all_mapped(
            "SELECT * FROM report_section_subscriptions WHERE slug = ? ORDER BY created_at ASC",
            &[slug.trim().into()],
            decode_section_subscription,
        )
        .await
    }

    /// Atomically transitions a subscription from `'active'` to `'notified'` if and only if
    /// it has not already been notified (`notified_at IS NULL`) and has not seen `notify_event_key`.
    pub async fn try_claim_section_subscription_notification(
        &self,
        sub_id: &str,
        notify_event_key: &str,
    ) -> Result<bool> {
        self.ensure_schema().await?;
        let now_str = now_iso();
        let affected = self
            .execute_params(
                "UPDATE report_section_subscriptions
                 SET status = 'notified',
                     notified_at = ?,
                     notified_event_key = ?,
                     updated_at = ?
                 WHERE id = ?
                   AND status = 'active'
                   AND notified_at IS NULL
                   AND (notified_event_key IS NULL OR notified_event_key != ?)",
                &[
                    now_str.clone().into(),
                    notify_event_key.trim().into(),
                    now_str.into(),
                    sub_id.trim().into(),
                    notify_event_key.trim().into(),
                ],
            )
            .await?;
        Ok(affected == 1)
    }

    pub async fn update_section_subscription_notes(&self, sub_id: &str, notes: &str) -> Result<()> {
        self.ensure_schema().await?;
        let now_str = now_iso();
        let trimmed: String = notes.trim().chars().take(500).collect();
        self.execute_params(
            "UPDATE report_section_subscriptions SET last_notes = ?, updated_at = ? WHERE id = ?",
            &[trimmed.into(), now_str.into(), sub_id.trim().into()],
        )
        .await?;
        Ok(())
    }

    /// Fetches the `cached_reports` record directly from the database (bypassing `l1_reports`).
    pub async fn get_cached_report_from_db_bypass_l1(
        &self,
        slug: &str,
    ) -> Result<Option<crate::db::CachedReportRecord>> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        self.l1_reports.invalidate(clean_slug).await;
        self.get_cached_report(clean_slug).await
    }

    /// Patches `cached_reports` in DB with a strictly advanced `updated_at` timestamp (invalidating
    /// any prior CDN ETag) and invalidates `self.l1_reports` without re-inserting stale L1 state.
    pub async fn patch_cached_report_and_invalidate_l1(
        &self,
        slug: &str,
        report: &Value,
        _rendered_html: Option<&str>,
        new_updated_at: &str,
    ) -> Result<()> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;

        let mut clean_report = report.clone();
        if let Some(obj) = clean_report.as_object_mut() {
            obj.remove("inspection");
            obj.remove("titleReview");
            obj.remove("creReview");
            obj.remove("agent_profile");
            obj.remove("coBranding");
        }

        self.l1_reports.invalidate(&clean_slug).await;
        self.put_cached_report(&clean_slug, &clean_report, None).await?;
        if let Some(upd_ts) = crate::db::proto::iso_to_unix_ts(new_updated_at.trim()) {
            let _ = self
                .execute_params_raw(
                    "UPDATE cached_reports SET updated_ts = ? WHERE slug = ?",
                    &[SqlParam::I64(upd_ts), SqlParam::from(clean_slug.as_str())],
                )
                .await;
        }
        self.l1_reports.invalidate(&clean_slug).await;
        Ok(())
    }
}
