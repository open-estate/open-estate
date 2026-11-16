//! Shared document-review pipeline utilities used across `inspect`, `title_review`,
//! `hoa_review`, `packet_review`, `repair_addendum`, and `cre::documents`.

use crate::ai::brief::build_brief;
use crate::ai::llm::LlmCallResult;
use serde_json::{json, Value};

/// Builds a compact grounded property brief when `report` contains `meta`, otherwise returns a
/// minimal fallback object carrying `default_address`.
pub fn build_report_brief_or_default(report: &Value, default_address: &str) -> Value {
    if report.is_object() && report.get("meta").is_some() {
        build_brief(report)
    } else {
        json!({ "address": default_address })
    }
}

/// Resolves the canonical display address from `report` (`/meta/displayAddress` or
/// `/geo/displayAddress`), falling back to `fallback`.
pub fn resolve_report_display_address<'a>(report: &'a Value, fallback: &'a str) -> &'a str {
    report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback)
}

/// Truncates already-scrubbed text to at most `max_chars` Unicode scalar values.
pub fn clip_sanitized_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Appends a standardized prompt-injection line-scrubbing warning when `scrubbed_lines > 0`.
pub fn append_scrubbed_lines_warning(
    warnings: &mut Vec<String>,
    scrubbed_lines: usize,
    doc_label: &str,
) {
    if scrubbed_lines > 0 {
        warnings.push(format!(
            "Scrubbed {scrubbed_lines} untrusted directive line(s) from uploaded {doc_label}."
        ));
    }
}

/// Redacts every string element in an optional JSON array using `scrub_fn`.
pub fn redact_pii_string_array(
    val: Option<&Value>,
    scrub_fn: impl Fn(&str) -> String,
) -> Vec<Value> {
    val.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .map(|s| Value::String(scrub_fn(s)))
                .collect()
        })
        .unwrap_or_default()
}

/// Initializes deterministic baseline metadata (`files`, `aiEnhanced = false`, `model = null`).
pub fn init_deterministic_ai_meta(parsed: &mut Value, files: Option<Vec<Value>>) {
    if let Some(obj) = parsed.as_object_mut() {
        if let Some(f) = files {
            obj.insert("files".to_string(), Value::Array(f));
        }
        obj.insert("aiEnhanced".to_string(), Value::Bool(false));
        obj.insert("model".to_string(), Value::Null);
    }
}

/// Stamps LLM enhancement metadata (`aiEnhanced`, `model`, `cached`) onto a review result object.
pub fn stamp_ai_enhancement_meta(parsed: &mut Value, res: &LlmCallResult, set_ai_enhanced: bool) {
    if let Some(obj) = parsed.as_object_mut() {
        if set_ai_enhanced {
            obj.insert("aiEnhanced".to_string(), Value::Bool(true));
        }
        obj.insert("model".to_string(), Value::String(res.model.clone()));
        obj.insert("cached".to_string(), Value::Bool(res.cached));
    }
}
