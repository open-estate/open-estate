//! Shared data contracts for lazy extension execution, status tracking, and notifications.

use crate::db::{LazyExtensionJobRow, PropertyLazyArtifactRow, ReportSectionSubscriptionRow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LazySectionStatusKind {
    Pending,
    AwaitingExternalResponse,
    AwaitingApproval,
    NotEnabled,
    Failed,
    Ready,
}

impl LazySectionStatusKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::AwaitingExternalResponse => "awaiting_external_response",
            Self::AwaitingApproval => "awaiting_approval",
            Self::NotEnabled => "not_enabled",
            Self::Failed => "failed",
            Self::Ready => "ready",
        }
    }

    pub fn can_subscribe(&self) -> bool {
        matches!(
            self,
            Self::Pending | Self::AwaitingExternalResponse | Self::AwaitingApproval
        )
    }

    pub fn from_job_status(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "succeeded" | "ready" => Self::Ready,
            "awaiting_external_response" => Self::AwaitingExternalResponse,
            "awaiting_approval" => Self::AwaitingApproval,
            "not_enabled" | "disabled" => Self::NotEnabled,
            "failed" | "cancelled" => Self::Failed,
            _ => Self::Pending,
        }
    }
}

/// Section-level status entry placed under `report.lazySections[sectionId]` and returned
/// by `GET /api/report/:slug/lazy-status` (never exposes raw paywalled payload bytes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LazySectionEntry {
    pub section_id: String,
    pub status: LazySectionStatusKind,
    pub message: String,
    pub can_subscribe: bool,
    pub required_tier: Option<String>,
    #[serde(default)]
    pub extension_ids: Vec<String>,
    pub updated_at: Option<String>,
    pub fetched_at: Option<String>,
    pub source_fingerprint: Option<String>,
}

/// Redacted job metadata exposed on `/api/report/:slug/lazy-status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LazyJobStatusView {
    pub id: String,
    pub extension_id: String,
    pub section_id: String,
    pub status: String,
    pub attempts: i64,
    pub max_attempts: i64,
    pub next_attempt_at: i64,
    pub trigger: String,
    pub last_error: Option<String>,
    pub updated_at: String,
}

impl From<&LazyExtensionJobRow> for LazyJobStatusView {
    fn from(r: &LazyExtensionJobRow) -> Self {
        Self {
            id: r.id.clone(),
            extension_id: r.extension_id.clone(),
            section_id: r.section_id.clone(),
            status: r.status.clone(),
            attempts: r.attempts,
            max_attempts: r.max_attempts,
            next_attempt_at: r.next_attempt_at,
            trigger: r.trigger_kind.clone(),
            last_error: r.last_error.clone(),
            updated_at: r.updated_at.clone(),
        }
    }
}

/// Redacted artifact metadata exposed on `/api/report/:slug/lazy-status` (excludes `payload_json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LazyArtifactMetaView {
    pub extension_id: String,
    pub section_id: String,
    pub status: String,
    pub required_tier: Option<String>,
    pub source_fingerprint: String,
    pub fetched_at: String,
    pub expires_at: Option<i64>,
    pub updated_at: String,
}

impl From<&PropertyLazyArtifactRow> for LazyArtifactMetaView {
    fn from(a: &PropertyLazyArtifactRow) -> Self {
        Self {
            extension_id: a.extension_id.clone(),
            section_id: a.section_id.clone(),
            status: a.status.clone(),
            required_tier: a.required_tier.clone(),
            source_fingerprint: a.source_fingerprint.clone(),
            fetched_at: a.fetched_at.clone(),
            expires_at: a.expires_at,
            updated_at: a.updated_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LazyStatusReport {
    pub ok: bool,
    pub slug: String,
    pub sections: std::collections::BTreeMap<String, LazySectionEntry>,
    pub jobs: Vec<LazyJobStatusView>,
    pub artifacts: Vec<LazyArtifactMetaView>,
}

/// Optional explicit policy overrides for an extension during `enqueue` (used when calling
/// before manifest proto fields are synced or for programmatic cloud policies).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LazyExtensionPolicy {
    pub enabled: Option<bool>,
    pub requires_manual_approval: Option<bool>,
    pub min_plan_tier: Option<String>,
    pub cache_ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueLazyJobRequest {
    pub slug: String,
    pub extension_id: String,
    pub section_id: String,
    pub requested_by_user_id: Option<String>,
    pub trigger: String,
    pub query: Option<Value>,
    pub max_attempts: i64,
    pub force_refresh: bool,
    /// Must be `true` only when a human/admin has explicitly approved a `requires_manual_approval` extension.
    pub explicit_approval: bool,
    pub policy: LazyExtensionPolicy,
}

impl EnqueueLazyJobRequest {
    pub fn new(
        slug: impl Into<String>,
        extension_id: impl Into<String>,
        section_id: impl Into<String>,
    ) -> Self {
        Self {
            slug: slug.into(),
            extension_id: extension_id.into(),
            section_id: section_id.into(),
            requested_by_user_id: None,
            trigger: "first_visit".to_string(),
            query: None,
            max_attempts: 5,
            force_refresh: false,
            explicit_approval: false,
            policy: LazyExtensionPolicy::default(),
        }
    }

    pub fn with_trigger(mut self, trigger: impl Into<String>) -> Self {
        self.trigger = trigger.into();
        self
    }

    pub fn with_user_id(mut self, user_id: Option<String>) -> Self {
        self.requested_by_user_id = user_id;
        self
    }

    pub fn with_query(mut self, query: Value) -> Self {
        self.query = Some(query);
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: i64) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn with_explicit_approval(mut self, approved: bool) -> Self {
        self.explicit_approval = approved;
        self
    }

    pub fn with_policy(mut self, policy: LazyExtensionPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueLazyJobOutcome {
    pub job: LazyExtensionJobRow,
    pub queued: bool,
    pub deduplicated: bool,
    pub section_status: LazySectionStatusKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionNotificationOutcome {
    pub checked: usize,
    pub claimed: usize,
    pub delivered_via_hook: usize,
    pub delivered_via_smtp: usize,
    pub skipped_no_smtp: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteLazyJobOutcome {
    pub job: Option<LazyExtensionJobRow>,
    pub artifact: PropertyLazyArtifactRow,
    pub report_patched: bool,
    pub new_report_updated_at: Option<String>,
    pub notifications: SectionNotificationOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionSubscriptionPublicView {
    pub id: String,
    pub slug: String,
    pub section_id: String,
    pub email: String,
    pub status: String,
    pub unsubscribe_token: String,
    pub created_at: String,
    pub notified_at: Option<String>,
}

impl From<&ReportSectionSubscriptionRow> for SectionSubscriptionPublicView {
    fn from(r: &ReportSectionSubscriptionRow) -> Self {
        Self {
            id: r.id.clone(),
            slug: r.slug.clone(),
            section_id: r.section_id.clone(),
            email: r.email.clone(),
            status: r.status.clone(),
            unsubscribe_token: r.unsubscribe_token.clone(),
            created_at: r.created_at.clone(),
            notified_at: r.notified_at.clone(),
        }
    }
}

pub fn humanize_section_label(section_id: &str) -> String {
    match section_id.trim() {
        "disclosuresAndInspections" | "disclosures" => {
            "Disclosures & Inspection Packet".to_string()
        }
        "packetReview" => "Packet Forensics & Review".to_string(),
        "hazards" | "naturalHazards" => "Climate & Natural Hazards".to_string(),
        "environment" | "environmental" => "Environmental & Water Screening".to_string(),
        "permits" | "publicResearch" => "Permits & Property Systems".to_string(),
        "utilities" | "buyerDiligence" => "Utilities, Broadband & Buyer Diligence".to_string(),
        "broadband" | "connectivity" => "Broadband & Fiber Availability".to_string(),
        "titleLegalRadar" => "Title & Legal Radar".to_string(),
        other => {
            let mut out = String::with_capacity(other.len() + 8);
            for (idx, ch) in other.chars().enumerate() {
                if ch == '_' || ch == '-' {
                    out.push(' ');
                } else if idx == 0 {
                    out.extend(ch.to_uppercase());
                } else if ch.is_uppercase() {
                    out.push(' ');
                    out.push(ch);
                } else {
                    out.push(ch);
                }
            }
            let trimmed = out.trim().to_string();
            if trimmed.is_empty() {
                "Property Report Section".to_string()
            } else {
                trimmed
            }
        }
    }
}
