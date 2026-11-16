//! Model token price table (`LLM_MODEL_PRICES_JSON`) and per-host external API cost table
//! (`EXTERNAL_API_COSTS_JSON`) for Open Estate multi-dimensional metering.

use crate::ai::llm::LlmUsage;
use crate::shared::EnvConfig;
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;
use url::Url;

/// Token pricing in micro-USD (`$1e-6`) per 1,000,000 tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelPriceMicros {
    pub input_per_1m_micros: u64,
    pub cached_input_per_1m_micros: u64,
    pub output_per_1m_micros: u64,
}

impl ModelPriceMicros {
    pub const fn new(
        input_per_1m_micros: u64,
        cached_input_per_1m_micros: u64,
        output_per_1m_micros: u64,
    ) -> Self {
        Self {
            input_per_1m_micros,
            cached_input_per_1m_micros,
            output_per_1m_micros,
        }
    }
}

/// Deterministic lookup table mapping LLM model names to micro-USD per 1M tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPriceTable {
    models: BTreeMap<String, ModelPriceMicros>,
}

impl Default for ModelPriceTable {
    fn default() -> Self {
        Self::builtin_defaults()
    }
}

fn normalize_model_key(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_effort = trimmed.split('@').next().unwrap_or(trimmed).trim();
    let without_provider = without_effort
        .rsplit_once('/')
        .map(|(_, rhs)| rhs)
        .unwrap_or(without_effort);
    without_provider.trim().to_ascii_lowercase()
}

fn usd_to_micros(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        0
    } else {
        (usd * 1_000_000.0).round() as u64
    }
}

fn parse_model_price_entry(val: &Value) -> Result<ModelPriceMicros> {
    let obj = val
        .as_object()
        .ok_or_else(|| anyhow!("Model price entry must be a JSON object"))?;

    let get_u64 = |keys: &[&str]| -> Option<u64> {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(|v| v.as_u64()))
    };
    let get_f64 = |keys: &[&str]| -> Option<f64> {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(|v| v.as_f64()))
    };

    let input = get_u64(&[
        "inputPer1mMicros",
        "input_per_1m_micros",
        "inputMicrosPer1m",
        "input_micros_per_1m",
    ])
    .or_else(|| {
        get_f64(&[
            "inputPer1mUsd",
            "input_per_1m_usd",
            "inputUsdPer1m",
            "input_usd_per_1m",
            "input",
        ])
        .map(usd_to_micros)
    })
    .unwrap_or(0);

    let cached_input = get_u64(&[
        "cachedInputPer1mMicros",
        "cached_input_per_1m_micros",
        "cachedPer1mMicros",
        "cached_per_1m_micros",
    ])
    .or_else(|| {
        get_f64(&[
            "cachedInputPer1mUsd",
            "cached_input_per_1m_usd",
            "cachedUsdPer1m",
            "cached_usd_per_1m",
            "cachedInput",
            "cached_input",
        ])
        .map(usd_to_micros)
    })
    .unwrap_or(input / 4);

    let output = get_u64(&[
        "outputPer1mMicros",
        "output_per_1m_micros",
        "outputMicrosPer1m",
        "output_micros_per_1m",
    ])
    .or_else(|| {
        get_f64(&[
            "outputPer1mUsd",
            "output_per_1m_usd",
            "outputUsdPer1m",
            "output_usd_per_1m",
            "output",
        ])
        .map(usd_to_micros)
    })
    .unwrap_or(0);

    Ok(ModelPriceMicros::new(input, cached_input, output))
}

impl ModelPriceTable {
    /// Built-in default token prices (micro-USD per 1M tokens) for Open Estate's supported models.
    pub fn builtin_defaults() -> Self {
        let mut models = BTreeMap::new();
        // Public list prices in micro-USD per 1M tokens ($1.00 = 1_000_000 micros).
        models.insert(
            "gpt-4.1".to_string(),
            ModelPriceMicros::new(2_000_000, 500_000, 8_000_000),
        );
        models.insert(
            "gpt-4.1-mini".to_string(),
            ModelPriceMicros::new(400_000, 100_000, 1_600_000),
        );
        models.insert(
            "gpt-4.1-nano".to_string(),
            ModelPriceMicros::new(100_000, 25_000, 400_000),
        );
        models.insert(
            "gpt-4o".to_string(),
            ModelPriceMicros::new(2_500_000, 1_250_000, 10_000_000),
        );
        models.insert(
            "gpt-4o-mini".to_string(),
            ModelPriceMicros::new(150_000, 75_000, 600_000),
        );
        models.insert(
            "o3".to_string(),
            ModelPriceMicros::new(2_000_000, 500_000, 8_000_000),
        );
        models.insert(
            "o3-mini".to_string(),
            ModelPriceMicros::new(1_100_000, 550_000, 4_400_000),
        );
        models.insert(
            "o4-mini".to_string(),
            ModelPriceMicros::new(1_100_000, 275_000, 4_400_000),
        );
        models.insert(
            "gpt-5".to_string(),
            ModelPriceMicros::new(1_250_000, 125_000, 10_000_000),
        );
        models.insert(
            "gpt-5-mini".to_string(),
            ModelPriceMicros::new(250_000, 25_000, 2_000_000),
        );
        models.insert(
            "gpt-5-nano".to_string(),
            ModelPriceMicros::new(50_000, 5_000, 400_000),
        );
        models.insert(
            "default".to_string(),
            ModelPriceMicros::new(2_000_000, 500_000, 8_000_000),
        );
        Self { models }
    }

    /// Creates an empty price table with zero built-in rates (useful for custom override-only tables).
    pub fn empty() -> Self {
        Self {
            models: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, model: impl AsRef<str>, price: ModelPriceMicros) {
        let key = normalize_model_key(model.as_ref());
        if !key.is_empty() {
            self.models.insert(key, price);
        }
    }

    /// Parses `LLM_MODEL_PRICES_JSON` overlaid onto [`Self::builtin_defaults`].
    pub fn from_json_str(raw: &str) -> Result<Self> {
        let mut table = Self::builtin_defaults();
        table.merge_json_str(raw)?;
        Ok(table)
    }

    /// Merges JSON entries into an existing price table.
    pub fn merge_json_str(&mut self, raw: &str) -> Result<()> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let val: Value = serde_json::from_str(trimmed)?;
        let root_obj = val
            .as_object()
            .ok_or_else(|| anyhow!("LLM_MODEL_PRICES_JSON must be a JSON object"))?;
        let map_obj = root_obj
            .get("models")
            .and_then(|m| m.as_object())
            .unwrap_or(root_obj);
        for (k, v) in map_obj {
            let norm = normalize_model_key(k);
            if !norm.is_empty() {
                let price = parse_model_price_entry(v)?;
                self.models.insert(norm, price);
            }
        }
        Ok(())
    }

    /// Builds a price table from `LLM_MODEL_PRICES_JSON` in `env` (or process environment).
    pub fn from_env(env: Option<&EnvConfig>) -> Self {
        let mut table = Self::builtin_defaults();
        let raw_opt = env
            .and_then(|e| e.get("LLM_MODEL_PRICES_JSON"))
            .map(str::to_string)
            .or_else(|| std::env::var("LLM_MODEL_PRICES_JSON").ok());
        if let Some(raw) = raw_opt {
            if let Err(err) = table.merge_json_str(&raw) {
                tracing::warn!(error = %err, "Ignoring invalid LLM_MODEL_PRICES_JSON");
            }
        }
        table
    }

    /// Resolves token pricing for `model` using exact match, longest dated-snapshot prefix match,
    /// or `"default"` fallback if configured.
    pub fn price_for(&self, model: &str) -> Option<ModelPriceMicros> {
        let key = normalize_model_key(model);
        if key.is_empty() {
            return self.models.get("default").copied();
        }
        if let Some(p) = self.models.get(&key) {
            return Some(*p);
        }
        let mut best: Option<(usize, ModelPriceMicros)> = None;
        for (candidate, price) in &self.models {
            if candidate == "default" || candidate.is_empty() {
                continue;
            }
            if key.starts_with(candidate.as_str()) {
                let boundary_ok = key
                    .as_bytes()
                    .get(candidate.len())
                    .is_some_and(|&b| matches!(b, b'-' | b':' | b'@' | b'.' | b'_'));
                if boundary_ok && best.map(|(len, _)| candidate.len() > len).unwrap_or(true) {
                    best = Some((candidate.len(), *price));
                }
            }
        }
        best.map(|(_, p)| p)
            .or_else(|| self.models.get("default").copied())
    }

    /// Computes micro-USD cost (`$1e-6`) for an [`LlmUsage`] report on `model`.
    pub fn cost_micros(&self, usage: &LlmUsage, model: &str) -> u64 {
        let Some(price) = self.price_for(model) else {
            return 0;
        };
        let input_total = usage.input_tokens.unwrap_or(0);
        let cached = usage.cached_input_tokens.min(input_total);
        let cache_write = usage
            .cache_write_tokens
            .min(input_total.saturating_sub(cached));
        let uncached = input_total
            .saturating_sub(cached)
            .saturating_sub(cache_write);
        let output = usage.output_tokens.unwrap_or(0);
        let scaled: u128 = (uncached as u128) * (price.input_per_1m_micros as u128)
            + (cached as u128) * (price.cached_input_per_1m_micros as u128)
            + (cache_write as u128) * (price.input_per_1m_micros as u128)
            + (output as u128) * (price.output_per_1m_micros as u128);
        if scaled == 0 {
            return 0;
        }
        ((scaled + 500_000) / 1_000_000).max(1) as u64
    }

    pub fn entries(&self) -> &BTreeMap<String, ModelPriceMicros> {
        &self.models
    }
}

/// Normalizes a hostname or URL string to a lowercase host without port.
pub fn normalize_host_key(host_or_url: &str) -> String {
    let trimmed = host_or_url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains("://") {
        if let Ok(u) = Url::parse(trimmed) {
            if let Some(h) = u.host_str() {
                return h.trim_matches(['[', ']']).to_ascii_lowercase();
            }
        }
    }
    let no_path = trimmed.split('/').next().unwrap_or(trimmed);
    if no_path.starts_with('[') {
        no_path
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    } else {
        no_path
            .split(':')
            .next()
            .unwrap_or(no_path)
            .trim()
            .to_ascii_lowercase()
    }
}

/// Per-host external API call cost table (`EXTERNAL_API_COSTS_JSON`) in micro-USD per call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalCostTable {
    hosts: BTreeMap<String, u64>,
}

fn parse_external_cost_entry(val: &Value) -> Result<u64> {
    if let Some(micros) = val.as_u64() {
        return Ok(micros);
    }
    if let Some(usd) = val.as_f64() {
        return Ok(usd_to_micros(usd));
    }
    let obj = val
        .as_object()
        .ok_or_else(|| anyhow!("External cost entry must be an integer, float, or object"))?;
    if let Some(micros) = obj
        .get("perCallMicros")
        .or_else(|| obj.get("per_call_micros"))
        .or_else(|| obj.get("micros"))
        .and_then(|v| v.as_u64())
    {
        return Ok(micros);
    }
    if let Some(usd) = obj
        .get("perCallUsd")
        .or_else(|| obj.get("per_call_usd"))
        .or_else(|| obj.get("usd"))
        .and_then(|v| v.as_f64())
    {
        return Ok(usd_to_micros(usd));
    }
    Ok(0)
}

impl ExternalCostTable {
    pub fn builtin_defaults() -> Self {
        Self {
            hosts: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, host: impl AsRef<str>, per_call_micros: u64) {
        let raw = host.as_ref().trim();
        let norm = if raw.starts_with('.') {
            format!(".{}", normalize_host_key(raw.trim_start_matches('.')))
        } else {
            normalize_host_key(raw)
        };
        if !norm.is_empty() && norm != "." {
            self.hosts.insert(norm, per_call_micros);
        }
    }

    pub fn from_json_str(raw: &str) -> Result<Self> {
        let mut table = Self::builtin_defaults();
        table.merge_json_str(raw)?;
        Ok(table)
    }

    pub fn merge_json_str(&mut self, raw: &str) -> Result<()> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let val: Value = serde_json::from_str(trimmed)?;
        let root_obj = val
            .as_object()
            .ok_or_else(|| anyhow!("EXTERNAL_API_COSTS_JSON must be a JSON object"))?;
        let map_obj = root_obj
            .get("hosts")
            .and_then(|m| m.as_object())
            .unwrap_or(root_obj);
        for (k, v) in map_obj {
            let cost = parse_external_cost_entry(v)?;
            self.insert(k, cost);
        }
        Ok(())
    }

    pub fn from_env(env: Option<&EnvConfig>) -> Self {
        let mut table = Self::builtin_defaults();
        let raw_opt = env
            .and_then(|e| e.get("EXTERNAL_API_COSTS_JSON"))
            .map(str::to_string)
            .or_else(|| std::env::var("EXTERNAL_API_COSTS_JSON").ok());
        if let Some(raw) = raw_opt {
            if let Err(err) = table.merge_json_str(&raw) {
                tracing::warn!(error = %err, "Ignoring invalid EXTERNAL_API_COSTS_JSON");
            }
        }
        table
    }

    /// Returns configured per-call cost in micro-USD (`0` for free/unconfigured public hosts).
    pub fn cost_micros_for_host(&self, host_or_url: &str) -> u64 {
        let host = normalize_host_key(host_or_url);
        if host.is_empty() {
            return self.hosts.get("default").copied().unwrap_or(0);
        }
        if let Some(&c) = self.hosts.get(&host) {
            return c;
        }
        // Check wildcard subdomain suffixes like `.paid-geo.example`
        let mut best: Option<(usize, u64)> = None;
        for (pattern, &cost) in &self.hosts {
            if let Some(suffix) = pattern.strip_prefix('.') {
                if (host == suffix || host.ends_with(pattern.as_str()))
                    && best.map(|(len, _)| suffix.len() > len).unwrap_or(true)
                {
                    best = Some((suffix.len(), cost));
                }
            }
        }
        best.map(|(_, c)| c)
            .or_else(|| self.hosts.get("default").copied())
            .unwrap_or(0)
    }

    pub fn entries(&self) -> &BTreeMap<String, u64> {
        &self.hosts
    }
}

static MODEL_PRICE_CACHE: Lazy<Mutex<(Option<String>, ModelPriceTable)>> =
    Lazy::new(|| Mutex::new((None, ModelPriceTable::builtin_defaults())));

static EXTERNAL_COST_CACHE: Lazy<Mutex<(Option<String>, ExternalCostTable)>> =
    Lazy::new(|| Mutex::new((None, ExternalCostTable::builtin_defaults())));

/// Memoized `ModelPriceTable` lookup keyed on raw `LLM_MODEL_PRICES_JSON` string.
pub(crate) fn resolve_model_price_table(env: Option<&EnvConfig>) -> ModelPriceTable {
    let raw_opt = env
        .and_then(|e| e.get("LLM_MODEL_PRICES_JSON"))
        .map(str::to_string)
        .or_else(|| std::env::var("LLM_MODEL_PRICES_JSON").ok());
    let mut guard = MODEL_PRICE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.0 == raw_opt {
        return guard.1.clone();
    }
    let table = ModelPriceTable::from_env(env);
    *guard = (raw_opt, table.clone());
    table
}

/// Memoized `ExternalCostTable` lookup keyed on raw `EXTERNAL_API_COSTS_JSON` string.
pub(crate) fn resolve_external_cost_table(env: Option<&EnvConfig>) -> ExternalCostTable {
    let raw_opt = env
        .and_then(|e| e.get("EXTERNAL_API_COSTS_JSON"))
        .map(str::to_string)
        .or_else(|| std::env::var("EXTERNAL_API_COSTS_JSON").ok());
    let mut guard = EXTERNAL_COST_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if guard.0 == raw_opt {
        return guard.1.clone();
    }
    let table = ExternalCostTable::from_env(env);
    *guard = (raw_opt, table.clone());
    table
}
