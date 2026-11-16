//! Database-backed store for admin-tunable application settings (`admin_settings` table).
//!
//! Stores AI prompt overrides and concise word limits (`ai.prompt.*`) so operators can tune
//! tone and length at runtime while preserving immutable housing constitution guardrails.

use crate::db::{now_iso, DbRowExt, DbStore, SqlParam};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const DEFAULT_CHATBOX_MAX_WORDS: usize = 120;
pub const MIN_CHATBOX_MAX_WORDS: usize = 40;
pub const MAX_CHATBOX_MAX_WORDS: usize = 400;
pub const MAX_PROMPT_SETTING_CHARS: usize = 12_000;

pub const SETTING_CHATBOX_SYSTEM_PROMPT: &str = "ai.prompt.chatbox";
pub const SETTING_CHATBOX_MAX_WORDS: &str = "ai.prompt.chatbox_max_words";
pub const SETTING_ANALYST_SYSTEM_PROMPT: &str = "ai.prompt.analyst";
pub const SETTING_SLIDES_SYSTEM_PROMPT: &str = "ai.prompt.slides";
pub const SETTING_ARTIFACT_SYSTEM_PROMPT: &str = "ai.prompt.artifact";
pub const SETTING_INSPECT_SYSTEM_PROMPT: &str = "ai.prompt.inspect";

const CATEGORY_AI_PROMPTS: &str = "ai_prompts";

struct AdminSettingSpec {
    key: &'static str,
    category: &'static str,
    label: &'static str,
    description: &'static str,
    default_value: &'static str,
}

const ADMIN_SETTING_SPECS: &[AdminSettingSpec] = &[
    AdminSettingSpec { key: SETTING_CHATBOX_MAX_WORDS, category: CATEGORY_AI_PROMPTS, label: "Chatbox Max Words", description: "Maximum word cap enforced for buyer chatbox responses (40-400 words).", default_value: "120" },
    AdminSettingSpec { key: SETTING_CHATBOX_SYSTEM_PROMPT, category: CATEGORY_AI_PROMPTS, label: "Chatbox System Prompt", description: "Style, Markdown links, anchor guidance, and plain-language instructions layered over the immutable constitution.", default_value: crate::ai::DEFAULT_CHATBOX_STYLE_PROMPT },
    AdminSettingSpec { key: SETTING_ANALYST_SYSTEM_PROMPT, category: CATEGORY_AI_PROMPTS, label: "AI Analyst System Prompt", description: "System instructions for quantitative whole-report due-diligence analysis.", default_value: crate::ai::ANALYST_INSTRUCTIONS },
    AdminSettingSpec { key: SETTING_SLIDES_SYSTEM_PROMPT, category: CATEGORY_AI_PROMPTS, label: "Walkthrough Slides System Prompt", description: "System instructions for the 8-slide interactive home walkthrough briefing.", default_value: crate::ai::SLIDES_INSTRUCTIONS },
    AdminSettingSpec { key: SETTING_ARTIFACT_SYSTEM_PROMPT, category: CATEGORY_AI_PROMPTS, label: "Interactive Artifact System Prompt", description: "System instructions for Buyer Command Center chart and scenario synthesis.", default_value: crate::ai::DEFAULT_ARTIFACT_INSTRUCTIONS },
    AdminSettingSpec { key: SETTING_INSPECT_SYSTEM_PROMPT, category: CATEGORY_AI_PROMPTS, label: "Inspection Analyst System Prompt", description: "System instructions for building-inspection, repair, and disclosure diligence.", default_value: crate::ai::INSPECTION_INSTRUCTIONS },
];

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdminSettingRow {
    pub setting_key: String,
    pub setting_value: String,
    pub category: String,
    pub description: String,
    pub updated_by: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdminSettingView {
    pub key: String,
    pub category: String,
    pub label: String,
    pub description: String,
    pub default_value: String,
    pub custom_value: Option<String>,
    pub effective_value: String,
    pub is_customized: bool,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AiPromptSettings {
    pub chatbox_system_prompt: Option<String>,
    pub chatbox_max_words: usize,
    pub analyst_system_prompt: Option<String>,
    pub slides_system_prompt: Option<String>,
    pub artifact_system_prompt: Option<String>,
    pub inspect_system_prompt: Option<String>,
}

impl Default for AiPromptSettings {
    fn default() -> Self {
        Self {
            chatbox_system_prompt: None,
            chatbox_max_words: DEFAULT_CHATBOX_MAX_WORDS,
            analyst_system_prompt: None,
            slides_system_prompt: None,
            artifact_system_prompt: None,
            inspect_system_prompt: None,
        }
    }
}

fn spec_for_key(key: &str) -> Option<&'static AdminSettingSpec> {
    ADMIN_SETTING_SPECS.iter().find(|s| s.key == key.trim())
}

pub fn is_allowed_admin_setting_key(key: &str) -> bool {
    spec_for_key(key).is_some()
}

fn contains_disallowed_setting_content(value: &str) -> bool {
    let low = value.to_ascii_lowercase();
    let normalized = low.split_whitespace().collect::<Vec<_>>().join(" ");
    const BLOCKED_RAW: &[&str] = &[
        "<script",
        "</script",
        "javascript:",
        "<iframe",
        "onerror=",
        "onload=",
    ];
    const BLOCKED_BYPASS: &[&str] = &[
        "immutable_housing_agent_constitution",
        "immutable_system_preamble",
        "ignore previous instructions",
        "ignore all previous instructions",
        "ignore prior instructions",
        "disregard the constitution",
        "override constitution",
        "bypass constitution",
        "disable guardrails",
        "bypass guardrails",
        "developer mode",
        "jailbreak",
    ];
    BLOCKED_RAW.iter().any(|p| low.contains(p))
        || BLOCKED_BYPASS.iter().any(|p| normalized.contains(p))
}

pub fn validate_admin_setting_value(key: &str, raw_value: &str) -> Result<String> {
    let spec = spec_for_key(key)
        .ok_or_else(|| anyhow!("Unsupported or disallowed admin setting key: {key}"))?;
    if raw_value.contains('\0') {
        bail!("Setting value contains invalid NUL bytes");
    }
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        bail!("Setting value cannot be empty; use reset to restore default");
    }
    if spec.key == SETTING_CHATBOX_MAX_WORDS {
        let n = trimmed.parse::<usize>().map_err(|_| {
            anyhow!("{SETTING_CHATBOX_MAX_WORDS} must be an integer ({MIN_CHATBOX_MAX_WORDS}..={MAX_CHATBOX_MAX_WORDS})")
        })?;
        if !(MIN_CHATBOX_MAX_WORDS..=MAX_CHATBOX_MAX_WORDS).contains(&n) {
            bail!("{SETTING_CHATBOX_MAX_WORDS} ({n}) must be between {MIN_CHATBOX_MAX_WORDS} and {MAX_CHATBOX_MAX_WORDS}");
        }
        return Ok(n.to_string());
    }
    let chars = trimmed.chars().count();
    if chars > MAX_PROMPT_SETTING_CHARS {
        bail!("Prompt setting exceeds maximum length ({chars} > {MAX_PROMPT_SETTING_CHARS} chars)");
    }
    if contains_disallowed_setting_content(trimmed) {
        bail!("Prompt setting rejected: contains disallowed script markup or guardrail bypass");
    }
    Ok(trimmed.to_string())
}

fn decode_admin_setting_row(r: &dyn DbRowExt) -> AdminSettingRow {
    AdminSettingRow {
        setting_key: r.opt_str("setting_key").unwrap_or_default(),
        setting_value: r.opt_str("setting_value").unwrap_or_default(),
        category: r
            .opt_str("category")
            .unwrap_or_else(|| CATEGORY_AI_PROMPTS.to_string()),
        description: r.opt_str("description").unwrap_or_default(),
        updated_by: r.opt_str("updated_by").filter(|s| !s.trim().is_empty()),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

fn build_setting_view(spec: &AdminSettingSpec, row: Option<&AdminSettingRow>) -> AdminSettingView {
    AdminSettingView {
        key: spec.key.to_string(),
        category: spec.category.to_string(),
        label: spec.label.to_string(),
        description: spec.description.to_string(),
        default_value: spec.default_value.to_string(),
        custom_value: row.map(|r| r.setting_value.clone()),
        effective_value: row
            .map(|r| r.setting_value.clone())
            .unwrap_or_else(|| spec.default_value.to_string()),
        is_customized: row.is_some(),
        updated_by: row.and_then(|r| r.updated_by.clone()),
        updated_at: row
            .map(|r| r.updated_at.clone())
            .filter(|s| !s.trim().is_empty()),
    }
}

impl DbStore {
    async fn fetch_admin_setting_rows(
        &self,
        category: Option<&str>,
    ) -> Result<HashMap<String, AdminSettingRow>> {
        let rows = if let Some(cat) = category.map(str::trim).filter(|s| !s.is_empty()) {
            self.fetch_all_mapped(
                "SELECT setting_key, setting_value, category, description, updated_by, updated_at FROM admin_settings WHERE category = ? ORDER BY setting_key ASC",
                &[SqlParam::from(cat)],
                decode_admin_setting_row,
            )
            .await?
        } else {
            self.fetch_all_mapped(
                "SELECT setting_key, setting_value, category, description, updated_by, updated_at FROM admin_settings ORDER BY setting_key ASC",
                &[],
                decode_admin_setting_row,
            )
            .await?
        };
        Ok(rows
            .into_iter()
            .map(|r| (r.setting_key.clone(), r))
            .collect())
    }

    pub async fn list_admin_settings(
        &self,
        category: Option<&str>,
    ) -> Result<Vec<AdminSettingView>> {
        let cat = category.map(str::trim).filter(|s| !s.is_empty());
        let saved = self.fetch_admin_setting_rows(cat).await?;
        Ok(ADMIN_SETTING_SPECS
            .iter()
            .filter(|s| cat.map(|c| s.category == c).unwrap_or(true))
            .map(|s| build_setting_view(s, saved.get(s.key)))
            .collect())
    }

    pub async fn get_admin_setting(&self, key: &str) -> Result<Option<AdminSettingView>> {
        let Some(spec) = spec_for_key(key) else {
            return Ok(None);
        };
        let row = self
            .fetch_optional_mapped(
                "SELECT setting_key, setting_value, category, description, updated_by, updated_at FROM admin_settings WHERE setting_key = ?",
                &[SqlParam::from(spec.key)],
                decode_admin_setting_row,
            )
            .await?;
        Ok(Some(build_setting_view(spec, row.as_ref())))
    }

    pub async fn upsert_admin_setting(
        &self,
        key: &str,
        value: &str,
        updated_by: Option<&str>,
    ) -> Result<AdminSettingView> {
        let spec = spec_for_key(key)
            .ok_or_else(|| anyhow!("Unsupported or disallowed admin setting key: {key}"))?;
        let clean_value = validate_admin_setting_value(spec.key, value)?;
        let clean_actor: Option<String> = updated_by
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(160).collect());
        let now = now_iso();
        self.upsert_row(
            "admin_settings",
            &[
                ("setting_key", SqlParam::from(spec.key)),
                ("setting_value", SqlParam::from(clean_value.as_str())),
                ("category", SqlParam::from(spec.category)),
                (
                    "description",
                    SqlParam::from(spec.description.chars().take(512).collect::<String>()),
                ),
                ("updated_by", SqlParam::from(clean_actor.clone())),
                ("updated_at", SqlParam::from(now.as_str())),
            ],
            &["setting_key"],
        )
        .await?;
        let saved = AdminSettingRow {
            setting_key: spec.key.to_string(),
            setting_value: clean_value,
            category: spec.category.to_string(),
            description: spec.description.to_string(),
            updated_by: clean_actor,
            updated_at: now,
        };
        Ok(build_setting_view(spec, Some(&saved)))
    }

    pub async fn reset_admin_setting(&self, key: &str) -> Result<AdminSettingView> {
        let spec = spec_for_key(key)
            .ok_or_else(|| anyhow!("Unsupported or disallowed admin setting key: {key}"))?;
        self.execute_params(
            "DELETE FROM admin_settings WHERE setting_key = ?",
            &[SqlParam::from(spec.key)],
        )
        .await?;
        Ok(build_setting_view(spec, None))
    }

    pub async fn try_load_ai_prompt_settings(&self) -> Result<AiPromptSettings> {
        let rows = self
            .fetch_admin_setting_rows(Some(CATEGORY_AI_PROMPTS))
            .await?;
        let opt_prompt = |key: &str| -> Option<String> {
            rows.get(key)
                .map(|r| r.setting_value.trim().to_string())
                .filter(|s| !s.is_empty() && !contains_disallowed_setting_content(s))
        };
        let chatbox_max_words = rows
            .get(SETTING_CHATBOX_MAX_WORDS)
            .and_then(|r| r.setting_value.trim().parse::<usize>().ok())
            .map(|n| n.clamp(MIN_CHATBOX_MAX_WORDS, MAX_CHATBOX_MAX_WORDS))
            .unwrap_or(DEFAULT_CHATBOX_MAX_WORDS);
        Ok(AiPromptSettings {
            chatbox_system_prompt: opt_prompt(SETTING_CHATBOX_SYSTEM_PROMPT),
            chatbox_max_words,
            analyst_system_prompt: opt_prompt(SETTING_ANALYST_SYSTEM_PROMPT),
            slides_system_prompt: opt_prompt(SETTING_SLIDES_SYSTEM_PROMPT),
            artifact_system_prompt: opt_prompt(SETTING_ARTIFACT_SYSTEM_PROMPT),
            inspect_system_prompt: opt_prompt(SETTING_INSPECT_SYSTEM_PROMPT),
        })
    }

    pub async fn load_ai_prompt_settings(&self) -> AiPromptSettings {
        match self.try_load_ai_prompt_settings().await {
            Ok(settings) => settings,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to load admin AI prompt settings; using defaults");
                AiPromptSettings::default()
            }
        }
    }
}
