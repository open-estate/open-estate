// OpenAI Responses API Client (`POST {OPENAI_BASE_URL}/responses`) for Open Estate.
// - Model chain: `LLM_MODEL` (default `gpt-4.1`) @ `LLM_EFFORT` (default `none`) -> `LLM_FALLBACK_MODEL` (default `gpt-4o-mini`)
// - Strict Structured Outputs (`text.format = { type: "json_schema", name, strict: true, schema }`)
// - Deterministic SHA-256 caching over `(schemaName, system, prompt)` via built-in `moka::future::Cache` + pluggable `LlmCacheBackend`
// - Zero leakage of `OPENAI_API_KEY` or `OPENAI_BASE_URL` in logs or errors

use crate::guardrails::{
    inspect_output_guardrail, scrub_sensitive_text, GuardrailError, ALLOWED_AI_PURPOSES,
    IMMUTABLE_SYSTEM_PREAMBLE,
};
use crate::shared::{now_iso, EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_LLM_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_LLM_MODEL: &str = "gpt-4.1";
pub const DEFAULT_LLM_EFFORT: &str = "none";
pub const DEFAULT_MATCH_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_FALLBACK_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_TIMEOUT_MS: u64 = 110_000;

/// Built-in concurrent L1 TTL cache (`moka::future::Cache`, 30-day TTL, up to 2,048 entries).
static GLOBAL_L1_LLM_CACHE: Lazy<Cache<String, Value>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(2_048)
        .time_to_live(Duration::from_secs(86400 * 30))
        .build()
});

pub fn global_llm_cache() -> &'static Cache<String, Value> {
    &GLOBAL_L1_LLM_CACHE
}

/// Pluggable async cache backend for LLM responses (supports `moka::future::Cache`, async closures, and `DbStore` L2 `llm_cache`).
pub trait LlmCacheBackend: Send + Sync {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Value>>;
    fn set<'a>(&'a self, key: &'a str, value: Value) -> BoxFuture<'a, ()>;
}

pub use LlmCacheBackend as LlmCache;

impl LlmCacheBackend for Cache<String, Value> {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move { Cache::get(self, key).await })
    }

    fn set<'a>(&'a self, key: &'a str, value: Value) -> BoxFuture<'a, ()> {
        let owned_key = key.to_string();
        Box::pin(async move {
            Cache::insert(self, owned_key, value).await;
        })
    }
}

/// Standalone `moka`-backed in-memory LLM cache instance.
#[derive(Clone)]
pub struct MemoryLlmCache {
    inner: Cache<String, Value>,
}

impl Default for MemoryLlmCache {
    fn default() -> Self {
        Self::new(1_024, Duration::from_secs(86400 * 7))
    }
}

impl MemoryLlmCache {
    pub fn new(max_capacity: u64, ttl: Duration) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(ttl)
                .build(),
        }
    }

    pub fn raw(&self) -> &Cache<String, Value> {
        &self.inner
    }
}

impl LlmCacheBackend for MemoryLlmCache {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Value>> {
        <Cache<String, Value> as LlmCacheBackend>::get(&self.inner, key)
    }

    fn set<'a>(&'a self, key: &'a str, value: Value) -> BoxFuture<'a, ()> {
        <Cache<String, Value> as LlmCacheBackend>::set(&self.inner, key, value)
    }
}

/// Async closure adapter so `DbStore` L2 `llm_cache` or custom KV stores plug directly into [`LlmCacheBackend`].
pub struct FnLlmCache<G, S> {
    get_fn: G,
    set_fn: S,
}

impl<G, GFut, S, SFut> FnLlmCache<G, S>
where
    G: Fn(String) -> GFut + Send + Sync,
    GFut: Future<Output = Option<Value>> + Send + 'static,
    S: Fn(String, Value) -> SFut + Send + Sync,
    SFut: Future<Output = ()> + Send + 'static,
{
    pub fn new(get_fn: G, set_fn: S) -> Self {
        Self { get_fn, set_fn }
    }
}

impl<G, GFut, S, SFut> LlmCacheBackend for FnLlmCache<G, S>
where
    G: Fn(String) -> GFut + Send + Sync,
    GFut: Future<Output = Option<Value>> + Send + 'static,
    S: Fn(String, Value) -> SFut + Send + Sync,
    SFut: Future<Output = ()> + Send + 'static,
{
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Value>> {
        let fut = (self.get_fn)(key.to_string());
        Box::pin(fut)
    }

    fn set<'a>(&'a self, key: &'a str, value: Value) -> BoxFuture<'a, ()> {
        let fut = (self.set_fn)(key.to_string(), value);
        Box::pin(fut)
    }
}

/// Two-tier L1 (`moka`) + L2 (`DbStore` / persistent store) LLM cache.
#[derive(Clone)]
pub struct LayeredLlmCache {
    l1: Cache<String, Value>,
    l2: Arc<dyn LlmCacheBackend>,
}

impl LayeredLlmCache {
    pub fn new(l2: Arc<dyn LlmCacheBackend>) -> Self {
        Self {
            l1: GLOBAL_L1_LLM_CACHE.clone(),
            l2,
        }
    }
}

impl LlmCacheBackend for LayeredLlmCache {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            if let Some(v) = Cache::get(&self.l1, key).await {
                return Some(v);
            }
            if let Some(v) = self.l2.get(key).await {
                Cache::insert(&self.l1, key.to_string(), v.clone()).await;
                return Some(v);
            }
            None
        })
    }

    fn set<'a>(&'a self, key: &'a str, value: Value) -> BoxFuture<'a, ()> {
        let k = key.to_string();
        Box::pin(async move {
            Cache::insert(&self.l1, k.clone(), value.clone()).await;
            self.l2.set(&k, value).await;
        })
    }
}

/// Persistent [`DbStore`](crate::db::DbStore) adapter implementing [`LlmCacheBackend`].
#[derive(Clone)]
pub struct DbLlmCache(pub crate::db::DbStore);

impl LlmCacheBackend for DbLlmCache {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move { self.0.get_llm_cache(key).await.ok().flatten() })
    }

    fn set<'a>(&'a self, key: &'a str, value: Value) -> BoxFuture<'a, ()> {
        let k = key.to_string();
        Box::pin(async move {
            let _ = self.0.put_llm_cache(&k, &value, 86400 * 30).await;
        })
    }
}

// ------------------------- Errors, Models & Usage -------------------------

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct LlmError {
    pub code: String,
    pub message: String,
    pub status: Option<u16>,
    pub retryable: bool,
    pub unsupported_effort: bool,
}

impl LlmError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: scrub_sensitive_text(&message.into()),
            status: None,
            retryable: false,
            unsupported_effort: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub model: String,
    pub effort: Option<String>,
}

fn norm_effort(raw: Option<&str>) -> Option<String> {
    let s = raw?.trim().to_lowercase();
    if s.is_empty() || s == "none" || s == "off" || s == "default" {
        None
    } else {
        Some(s)
    }
}

/// Parses `"o3@high"` -> `ModelSpec { model: "o3", effort: Some("high") }`.
pub fn parse_model_spec(spec: &str, default_effort: Option<&str>) -> ModelSpec {
    let s = spec.trim();
    if let Some(at) = s.rfind('@') {
        if at > 0 {
            return ModelSpec {
                model: s[..at].trim().to_string(),
                effort: norm_effort(Some(&s[at + 1..])),
            };
        }
    }
    ModelSpec {
        model: s.to_string(),
        effort: norm_effort(default_effort),
    }
}

/// Resolves ordered primary -> fallback [`ModelSpec`] chain from `env`, optionally overriding
/// the primary model's reasoning effort (stripping any `@high` / `@max` suffix embedded in
/// `LLM_MODEL`), splitting `LLM_FALLBACK_MODEL` on `','`, ignoring `"none"`, and deduplicating.
pub fn resolve_models_with_effort(env: &EnvConfig, effort_override: Option<&str>) -> Vec<ModelSpec> {
    let primary_raw = env.get("LLM_MODEL").unwrap_or(DEFAULT_LLM_MODEL);
    let primary = match effort_override {
        Some(eff) => {
            let base = parse_model_spec(primary_raw, None).model;
            ModelSpec {
                model: base,
                effort: norm_effort(Some(eff)),
            }
        }
        None => {
            let primary_effort = env.get("LLM_EFFORT").or(Some(DEFAULT_LLM_EFFORT));
            parse_model_spec(primary_raw, primary_effort)
        }
    };

    let fb_raw = env
        .get("LLM_FALLBACK_MODEL")
        .unwrap_or(DEFAULT_FALLBACK_MODEL);
    let mut raw_chain = vec![primary];
    for part in fb_raw.split(',') {
        let trimmed = part.trim();
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("none") {
            raw_chain.push(parse_model_spec(trimmed, None));
        }
    }

    dedup_model_specs(raw_chain)
}

/// Formats [`resolve_models_with_effort`] into `"model@effort"` / `"model"` strings suitable for [`LlmCallOptions::models`].
pub fn resolve_model_specs_with_effort(
    env: &EnvConfig,
    effort_override: Option<&str>,
) -> Vec<String> {
    resolve_models_with_effort(env, effort_override)
        .into_iter()
        .map(|m| match m.effort {
            Some(eff) => format!("{}@{eff}", m.model),
            None => m.model,
        })
        .collect()
}

fn dedup_model_specs(raw_chain: Vec<ModelSpec>) -> Vec<ModelSpec> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for item in raw_chain {
        if item.model.is_empty() {
            continue;
        }
        let key = format!("{}@{}", item.model, item.effort.as_deref().unwrap_or(""));
        if seen.insert(key) {
            deduped.push(item);
        }
    }
    deduped
}

/// Resolves ordered primary -> fallback model chain from `env` or explicit override list.
pub fn resolve_models(env: &EnvConfig, explicit: &[String]) -> Vec<ModelSpec> {
    if !explicit.is_empty() {
        dedup_model_specs(
            explicit
                .iter()
                .map(|m| parse_model_spec(m, None))
                .collect(),
        )
    } else {
        resolve_models_with_effort(env, None)
    }
}

/// Resolves an interactive low-latency model chain (`SLIDES_LLM_MODEL` or `<LLM_MODEL base>@low` -> `LLM_FALLBACK_MODEL`),
/// stripping any global `@max` / `@high` suffix embedded in `LLM_MODEL` so interactive endpoints respond in seconds.
pub fn resolve_interactive_model_specs(env: &EnvConfig) -> Vec<String> {
    let explicit_slides_eff = env
        .get("SLIDES_LLM_EFFORT")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut specs = Vec::new();
    if let Some(explicit_slides_model) = env
        .get("SLIDES_LLM_MODEL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let parsed = parse_model_spec(explicit_slides_model, explicit_slides_eff.or(Some("low")));
        if !parsed.model.is_empty() {
            match parsed.effort {
                Some(eff) => specs.push(format!("{}@{eff}", parsed.model)),
                None => specs.push(parsed.model),
            }
        }
    } else {
        let raw_primary = env.get("LLM_MODEL").unwrap_or(DEFAULT_LLM_MODEL);
        let parsed_primary = parse_model_spec(raw_primary, env.get("LLM_EFFORT"));
        if !parsed_primary.model.is_empty() {
            let eff = match explicit_slides_eff {
                Some(e) => norm_effort(Some(e)),
                None => parsed_primary.effort.as_ref().map(|_| "low".to_string()),
            };
            match eff {
                Some(e) => specs.push(format!("{}@{e}", parsed_primary.model)),
                None => specs.push(parsed_primary.model),
            }
        }
    }
    let fb_raw = env
        .get("LLM_FALLBACK_MODEL")
        .unwrap_or(DEFAULT_FALLBACK_MODEL);
    for part in fb_raw.split(',') {
        let trimmed = part.trim();
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("none") {
            let parsed_fb = parse_model_spec(trimmed, None);
            if !parsed_fb.model.is_empty() {
                match parsed_fb.effort.as_deref() {
                    Some("max") | Some("xhigh") | Some("high") => {
                        specs.push(format!("{}@low", parsed_fb.model));
                    }
                    Some(eff) => specs.push(format!("{}@{eff}", parsed_fb.model)),
                    None => specs.push(parsed_fb.model),
                }
            }
        }
    }
    specs
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

impl LlmMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmInput {
    Text(String),
    Messages(Vec<LlmMessage>),
    RawItems(Vec<Value>),
}

impl Default for LlmInput {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl From<String> for LlmInput {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for LlmInput {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

impl From<Vec<LlmMessage>> for LlmInput {
    fn from(v: Vec<LlmMessage>) -> Self {
        Self::Messages(v)
    }
}

/// Normalize [`LlmInput`] into OpenAI Responses API `input` array items.
pub fn normalize_input(input: &LlmInput) -> Result<Value> {
    match input {
        LlmInput::Text(s) => {
            if s.trim().is_empty() {
                return Err(LlmError::new("bad_input", "LLM input prompt cannot be empty").into());
            }
            Ok(json!([
                {
                    "role": "user",
                    "content": [{ "type": "input_text", "text": s }]
                }
            ]))
        }
        LlmInput::Messages(msgs) => {
            if msgs.is_empty() {
                return Err(LlmError::new("bad_input", "LLM messages list cannot be empty").into());
            }
            let items: Vec<Value> = msgs
                .iter()
                .map(|m| {
                    let role = match m.role.as_str() {
                        "assistant" => "assistant",
                        "system" | "developer" => "developer",
                        _ => "user",
                    };
                    let part_type = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    json!({
                        "role": role,
                        "content": [{ "type": part_type, "text": m.content }]
                    })
                })
                .collect();
            Ok(Value::Array(items))
        }
        LlmInput::RawItems(items) => {
            if items.is_empty() {
                return Err(LlmError::new("bad_input", "LLM raw items cannot be empty").into());
            }
            Ok(Value::Array(items.clone()))
        }
    }
}

/// Deterministic SHA-256 cache key over `(schemaName, system, prompt)`.
pub fn compute_llm_cache_key(schema_name: &str, system: &str, prompt: &str) -> String {
    let canonical = serde_json::to_string(&(schema_name, system, prompt)).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let hex_digest = hex::encode(hasher.finalize());
    format!("llm:v1:{schema_name}:{hex_digest}")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: u64,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: u64,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmCallResult {
    pub data: Value,
    pub model: String,
    pub effort: Option<String>,
    pub fallback_used: bool,
    pub cached: bool,
    pub ms: u64,
    pub usage: Option<LlmUsage>,
    pub created_at: String,
}

#[derive(Clone)]
pub struct LlmCallOptions {
    pub purpose: String,
    pub instructions: String,
    pub input: LlmInput,
    pub schema: Value,
    pub schema_name: String,
    pub models: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub cache: Option<Arc<dyn LlmCacheBackend>>,
    pub refresh: bool,
    pub use_cache: bool,
    pub validator: Option<Arc<dyn Fn(&Value) -> Vec<String> + Send + Sync>>,
}

impl LlmCallOptions {
    pub fn new(
        purpose: impl Into<String>,
        schema_name: impl Into<String>,
        instructions: impl Into<String>,
        input: impl Into<LlmInput>,
        schema: Value,
    ) -> Self {
        Self {
            purpose: purpose.into(),
            instructions: instructions.into(),
            input: input.into(),
            schema,
            schema_name: schema_name.into(),
            models: Vec::new(),
            timeout_ms: None,
            max_output_tokens: None,
            cache: None,
            refresh: false,
            use_cache: true,
            validator: None,
        }
    }
}

// ------------------------- Parsing & JSON Schema Validation -------------------------

pub(crate) fn extract_response_text(j: &Value) -> Result<String, LlmError> {
    if let Some(ot) = j.get("output_text").and_then(|v| v.as_str()) {
        if !ot.trim().is_empty() {
            return Ok(ot.to_string());
        }
    }
    let Some(output_arr) = j.get("output").and_then(|v| v.as_array()) else {
        return Err(LlmError::new("empty", "model returned no output array"));
    };

    let msgs: Vec<&Value> = output_arr
        .iter()
        .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("message"))
        .collect();
    let finals: Vec<&Value> = msgs
        .iter()
        .copied()
        .filter(|m| m.get("phase").and_then(|p| p.as_str()) == Some("final_answer"))
        .collect();

    let candidates: Vec<&Value> = if !finals.is_empty() {
        finals
    } else if !msgs.is_empty() {
        msgs
    } else {
        output_arr.iter().collect()
    };

    let mut parts = Vec::new();
    for item in candidates {
        if let Some(content_arr) = item.get("content").and_then(|c| c.as_array()) {
            for c in content_arr {
                let ctype = c.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ctype == "refusal" {
                    let msg = c
                        .get("refusal")
                        .and_then(|r| r.as_str())
                        .unwrap_or("refused");
                    return Err(LlmError::new("refusal", format!("model refused: {msg}")));
                }
                if (ctype == "output_text" || ctype == "text" || ctype.is_empty())
                    && c.get("text").and_then(|t| t.as_str()).is_some()
                {
                    if let Some(txt) = c.get("text").and_then(|t| t.as_str()) {
                        parts.push(txt);
                    }
                }
            }
        }
    }

    let joined = parts.join("");
    if joined.trim().is_empty() {
        Err(LlmError::new("empty", "model returned empty output text"))
    } else {
        Ok(joined)
    }
}

fn extract_usage(u: Option<&Value>) -> Option<LlmUsage> {
    let obj = u?.as_object()?;
    Some(LlmUsage {
        input_tokens: obj.get("input_tokens").and_then(|v| v.as_u64()),
        cached_input_tokens: obj
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: obj.get("output_tokens").and_then(|v| v.as_u64()),
        reasoning_tokens: obj
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        total_tokens: obj.get("total_tokens").and_then(|v| v.as_u64()),
    })
}

/// Parse model output as JSON, tolerating markdown code fences or stray leading/trailing prose.
pub fn parse_json_loose(text: &str) -> Result<Value, LlmError> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    if let Some(fence_start) = t.find("```") {
        let rest = &t[fence_start + 3..];
        let body_start = rest.find('\n').map(|i| i + 1).unwrap_or(0);
        let after_lang = &rest[body_start..];
        if let Some(fence_end) = after_lang.find("```") {
            let candidate = after_lang[..fence_end].trim();
            if let Ok(v) = serde_json::from_str::<Value>(candidate) {
                return Ok(v);
            }
        }
    }
    if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) {
        if b > a {
            if let Ok(v) = serde_json::from_str::<Value>(&t[a..=b]) {
                return Ok(v);
            }
        }
    }
    let preview: String = t.chars().take(120).collect();
    Err(LlmError::new(
        "invalid_json",
        format!(
            "model output is not valid JSON ({} chars: {})",
            t.len(),
            preview.replace('\n', " ")
        ),
    ))
}

fn json_value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 {
                    "integer"
                } else {
                    "number"
                }
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Recursive validator for the strict JSON Schema subset used with OpenAI Structured Outputs.
pub fn validate_json_schema(schema: &Value, value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    validate_schema_inner(schema, value, "$", &mut errors);
    errors
}

fn validate_schema_inner(
    schema: &Value,
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if errors.len() > 25 {
        return;
    }
    let Some(s_obj) = schema.as_object() else {
        return;
    };

    if let Some(any_of) = s_obj.get("anyOf").and_then(|v| v.as_array()) {
        let ok = any_of.iter().any(|sub| {
            let mut sub_errs = Vec::new();
            validate_schema_inner(sub, value, path, &mut sub_errs);
            sub_errs.is_empty()
        });
        if !ok {
            errors.push(format!("{path}: does not match anyOf"));
        }
        return;
    }

    if let Some(type_val) = s_obj.get("type") {
        let expected_types: Vec<&str> = match type_val {
            Value::String(s) => vec![s.as_str()],
            Value::Array(arr) => arr.iter().filter_map(|x| x.as_str()).collect(),
            _ => Vec::new(),
        };
        if !expected_types.is_empty() {
            let actual = json_value_type(value);
            let matches = expected_types
                .iter()
                .any(|&exp| exp == actual || (exp == "number" && actual == "integer"));
            if !matches {
                errors.push(format!(
                    "{path}: expected {}, got {actual}",
                    expected_types.join("|")
                ));
                return;
            }
        }
    }

    if let Some(enum_arr) = s_obj.get("enum").and_then(|v| v.as_array()) {
        if !enum_arr.iter().any(|item| item == value) {
            errors.push(format!("{path}: {value} not in enum"));
        }
    }

    if let Some(val_obj) = value.as_object() {
        let props = s_obj.get("properties").and_then(|p| p.as_object());
        if let Some(req_arr) = s_obj.get("required").and_then(|r| r.as_array()) {
            for req in req_arr.iter().filter_map(|x| x.as_str()) {
                if !val_obj.contains_key(req) {
                    errors.push(format!("{path}.{req}: missing required property"));
                }
            }
        }
        let forbid_extra = s_obj
            .get("additionalProperties")
            .and_then(|a| a.as_bool())
            == Some(false);
        for (k, v) in val_obj {
            if let Some(prop_schema) = props.and_then(|p| p.get(k)) {
                validate_schema_inner(prop_schema, v, &format!("{path}.{k}"), errors);
            } else if forbid_extra {
                errors.push(format!("{path}.{k}: unexpected additional property"));
            }
        }
    }

    if let Some(val_arr) = value.as_array() {
        if let Some(min_items) = s_obj.get("minItems").and_then(|m| m.as_u64()) {
            if (val_arr.len() as u64) < min_items {
                errors.push(format!(
                    "{path}: {} items < minItems {min_items}",
                    val_arr.len()
                ));
            }
        }
        if let Some(max_items) = s_obj.get("maxItems").and_then(|m| m.as_u64()) {
            if (val_arr.len() as u64) > max_items {
                errors.push(format!(
                    "{path}: {} items > maxItems {max_items}",
                    val_arr.len()
                ));
            }
        }
        if let Some(items_schema) = s_obj.get("items") {
            for (idx, item) in val_arr.iter().enumerate() {
                validate_schema_inner(items_schema, item, &format!("{path}[{idx}]"), errors);
            }
        }
    }
}

fn check_json_strings_guardrail(v: &Value) -> Option<&'static str> {
    match v {
        Value::String(s) => inspect_output_guardrail(s),
        Value::Array(arr) => arr.iter().find_map(check_json_strings_guardrail),
        Value::Object(obj) => obj.values().find_map(check_json_strings_guardrail),
        _ => None,
    }
}

// ------------------------- Transport (`POST {OPENAI_BASE_URL}/responses`) -------------------------

struct SingleCallOutput {
    text: String,
    usage: Option<LlmUsage>,
    response_model: String,
}

async fn call_responses_once(
    http: &HttpClient,
    base_url: &str,
    api_key: &str,
    model: &str,
    effort: Option<&str>,
    guarded_instructions: &str,
    normalized_input: &Value,
    schema: &Value,
    schema_name: &str,
    timeout_ms: u64,
    max_output_tokens: Option<u32>,
) -> Result<SingleCallOutput, LlmError> {
    let endpoint = format!("{}/responses", base_url.trim_end_matches('/'));
    let mut body = json!({
        "model": model,
        "instructions": guarded_instructions,
        "input": normalized_input,
        "stream": false,
        "store": false,
        "text": {
            "format": {
                "type": "json_schema",
                "name": if schema_name.is_empty() { "output" } else { schema_name },
                "strict": true,
                "schema": schema,
            }
        }
    });
    if let Some(eff) = effort {
        body["reasoning"] = json!({ "effort": eff });
    }
    if let Some(max_tok) = max_output_tokens {
        body["max_output_tokens"] = json!(max_tok);
    }

    let resp = http
        .raw()
        .post(&endpoint)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(timeout_ms))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            let code = if e.is_timeout() { "timeout" } else { "network" };
            LlmError {
                code: code.to_string(),
                message: scrub_sensitive_text(&format!("upstream {code} error: {e}")),
                status: None,
                retryable: true,
                unsupported_effort: false,
            }
        })?;

    let status = resp.status();
    let raw_text = resp.text().await.map_err(|e| LlmError {
        code: "network".to_string(),
        message: scrub_sensitive_text(&format!("response read error: {e}")),
        status: Some(status.as_u16()),
        retryable: true,
        unsupported_effort: false,
    })?;

    if !status.is_success() {
        let parsed_msg = serde_json::from_str::<Value>(&raw_text)
            .ok()
            .and_then(|j| {
                j.get("error")
                    .and_then(|er| {
                        er.get("message")
                            .or_else(|| er.get("type"))
                            .and_then(|m| m.as_str())
                    })
                    .or_else(|| j.get("detail").and_then(|d| d.as_str()))
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| raw_text.chars().take(240).collect());

        let low = parsed_msg.to_lowercase();
        let unsupported_effort = (status.as_u16() == 400 || status.as_u16() == 422)
            && effort.is_some()
            && (low.contains("reasoning")
                || low.contains("effort")
                || low.contains("unsupported")
                || low.contains("unknown parameter"));
        let retryable = status.as_u16() == 429 || status.is_server_error();
        return Err(LlmError {
            code: "http".to_string(),
            message: scrub_sensitive_text(&format!("HTTP {}: {}", status.as_u16(), parsed_msg)),
            status: Some(status.as_u16()),
            retryable,
            unsupported_effort,
        });
    }

    let j: Value = serde_json::from_str(&raw_text).map_err(|_| LlmError {
        code: "bad_response".to_string(),
        message: format!("non-JSON upstream response ({} bytes)", raw_text.len()),
        status: Some(status.as_u16()),
        retryable: true,
        unsupported_effort: false,
    })?;

    if let Some(err_obj) = j.get("error").filter(|v| !v.is_null()) {
        let msg = err_obj
            .get("message")
            .or_else(|| err_obj.get("type"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown upstream API error");
        return Err(LlmError::new("api", format!("API error: {msg}")));
    }

    if let Some(st) = j.get("status").and_then(|s| s.as_str()) {
        if st != "completed" {
            let reason = j
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str())
                .unwrap_or("unknown");
            return Err(LlmError {
                code: "incomplete".to_string(),
                message: format!("response status \"{st}\" ({reason})"),
                status: None,
                retryable: true,
                unsupported_effort: false,
            });
        }
    }

    let text = extract_response_text(&j)?;
    let response_model = j
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(model)
        .to_string();

    Ok(SingleCallOutput {
        text,
        usage: extract_usage(j.get("usage")),
        response_model,
    })
}

/// Calls OpenAI Responses API with Structured Outputs, model fallback chain, and optional borrowed L2 cache.
pub async fn responses_json_with_cache(
    http: &HttpClient,
    env: &EnvConfig,
    opts: LlmCallOptions,
    external_cache: Option<&dyn LlmCacheBackend>,
) -> Result<LlmCallResult> {
    if !ALLOWED_AI_PURPOSES.contains(&opts.purpose.as_str()) {
        return Err(GuardrailError::new(
            "DIRECT_LLM_ACCESS_DENIED",
            format!(
                "Direct or unguarded calls to the LLM backend are prohibited. AI access is strictly limited to verified {} residential property skills.",
                env.brand_name()
            ),
            403,
        )
        .into());
    }

    let guarded_instructions =
        if opts.instructions.contains("IMMUTABLE SECURITY & PURPOSE-LOCK CONSTITUTION") {
            opts.instructions.clone()
        } else if opts.instructions.trim().is_empty() {
            IMMUTABLE_SYSTEM_PREAMBLE.to_string()
        } else {
            format!("{IMMUTABLE_SYSTEM_PREAMBLE}\n\n{}", opts.instructions)
        };

    let normalized_input = normalize_input(&opts.input)?;
    let prompt_canonical = serde_json::to_string(&normalized_input).unwrap_or_default();
    let cache_key = compute_llm_cache_key(&opts.schema_name, &guarded_instructions, &prompt_canonical);

    let t0 = Instant::now();

    // 1. Check L1 `moka` cache & optional L2 cache BEFORE requiring network calls
    if opts.use_cache && !opts.refresh {
        if let Some(hit) = Cache::get(&GLOBAL_L1_LLM_CACHE, &cache_key).await {
            if let Ok(mut parsed) = serde_json::from_value::<LlmCallResult>(hit) {
                parsed.cached = true;
                parsed.ms = t0.elapsed().as_millis() as u64;
                return Ok(parsed);
            }
        }
        if let Some(ext) = external_cache.or(opts.cache.as_deref()) {
            if let Some(hit) = ext.get(&cache_key).await {
                if let Ok(mut parsed) = serde_json::from_value::<LlmCallResult>(hit.clone()) {
                    Cache::insert(&GLOBAL_L1_LLM_CACHE, cache_key.clone(), hit).await;
                    parsed.cached = true;
                    parsed.ms = t0.elapsed().as_millis() as u64;
                    return Ok(parsed);
                }
            }
        }
    }

    let api_key = env
        .llm_api_key()
        .ok_or_else(|| LlmError::new("no_key", "AI backend credentials are not configured"))?;
    let base_url = env
        .llm_base_url()
        .ok_or_else(|| LlmError::new("no_base_url", "AI backend endpoint is not configured"))?;

    let chain = resolve_models(env, &opts.models);
    if chain.is_empty() {
        return Err(LlmError::new("no_model", "No LLM model configured").into());
    }

    let timeout_ms = opts
        .timeout_ms
        .or_else(|| env.get("LLM_TIMEOUT_MS").and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(DEFAULT_TIMEOUT_MS);

    let retries_per_model = if chain.len() > 1 { 0usize } else { 1usize };
    let mut attempt_errors: Vec<String> = Vec::new();

    for (idx, spec) in chain.iter().enumerate() {
        let mut current_effort = spec.effort.clone();
        for attempt in 0..=retries_per_model {
            let call_res = call_responses_once(
                http,
                base_url,
                api_key,
                &spec.model,
                current_effort.as_deref(),
                &guarded_instructions,
                &normalized_input,
                &opts.schema,
                &opts.schema_name,
                timeout_ms,
                opts.max_output_tokens,
            )
            .await;

            // If a model spec failed solely due to unsupported `reasoning.effort`, retry immediately without `reasoning`
            let single_out = match call_res {
                Ok(o) => o,
                Err(e) if e.unsupported_effort && current_effort.is_some() => {
                    current_effort = None;
                    match call_responses_once(
                        http,
                        base_url,
                        api_key,
                        &spec.model,
                        None,
                        &guarded_instructions,
                        &normalized_input,
                        &opts.schema,
                        &opts.schema_name,
                        timeout_ms,
                        opts.max_output_tokens,
                    )
                    .await
                    {
                        Ok(o2) => o2,
                        Err(e2) => {
                            tracing::warn!(
                                schema = %opts.schema_name,
                                model = %spec.model,
                                elapsed_ms = t0.elapsed().as_millis() as u64,
                                error = %e2.message,
                                "LLM attempt failed after dropping unsupported reasoning parameter"
                            );
                            attempt_errors.push(format!("{}: {}", spec.model, e2.message));
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        schema = %opts.schema_name,
                        model = %spec.model,
                        elapsed_ms = t0.elapsed().as_millis() as u64,
                        error = %e.message,
                        "LLM attempt failed"
                    );
                    attempt_errors.push(format!(
                        "{}{}: {}",
                        spec.model,
                        current_effort
                            .as_ref()
                            .map(|ef| format!("@{ef}"))
                            .unwrap_or_default(),
                        e.message
                    ));
                    if !e.retryable || attempt == retries_per_model {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(900 * (attempt as u64 + 1))).await;
                    continue;
                }
            };

            let data = match parse_json_loose(&single_out.text) {
                Ok(d) => d,
                Err(pe) => {
                    attempt_errors.push(format!("{}: {}", spec.model, pe.message));
                    break;
                }
            };

            let schema_errs = validate_json_schema(&opts.schema, &data);
            if !schema_errs.is_empty() {
                let summary = schema_errs
                    .into_iter()
                    .take(4)
                    .collect::<Vec<_>>()
                    .join("; ");
                attempt_errors.push(format!("{}: schema mismatch ({summary})", spec.model));
                break;
            }

            if check_json_strings_guardrail(&data).is_some() {
                return Err(GuardrailError::new(
                    "OUTPUT_POLICY_VIOLATION",
                    "AI output blocked by residential property purpose guardrail.",
                    400,
                )
                .into());
            }

            if let Some(ref validator) = opts.validator {
                let extra_errs = validator(&data);
                if !extra_errs.is_empty() {
                    let summary = extra_errs
                        .into_iter()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join("; ");
                    attempt_errors.push(format!("{}: validation rejected ({summary})", spec.model));
                    break;
                }
            }

            let _ = single_out.response_model;
            let result = LlmCallResult {
                data,
                model: spec.model.clone(),
                effort: current_effort.clone(),
                fallback_used: idx > 0,
                cached: false,
                ms: t0.elapsed().as_millis() as u64,
                usage: single_out.usage,
                created_at: now_iso(),
            };

            if opts.use_cache {
                if let Ok(serialized) = serde_json::to_value(&result) {
                    Cache::insert(&GLOBAL_L1_LLM_CACHE, cache_key.clone(), serialized.clone())
                        .await;
                    if let Some(ext) = external_cache.or(opts.cache.as_deref()) {
                        ext.set(&cache_key, serialized).await;
                    }
                }
            }

            return Ok(result);
        }
    }

    Err(anyhow!(LlmError::new(
        "all_failed",
        format!(
            "All LLM models failed — {}",
            attempt_errors.join(" | ")
        ),
    )))
}

/// Primary structured JSON LLM call (`LLM_MODEL` -> `LLM_FALLBACK_MODEL`) over the OpenAI Responses API.
pub async fn responses_json(
    http: &HttpClient,
    env: &EnvConfig,
    opts: LlmCallOptions,
) -> Result<LlmCallResult> {
    responses_json_with_cache(http, env, opts, None).await
}
