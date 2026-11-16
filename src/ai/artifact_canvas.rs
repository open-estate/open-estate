// Dynamic Buyer Command Center & Interactive AI Re-Charting Canvas (`POST /api/report/:slug/artifact`).
// Combines an instant deterministic property visualization router (<1ms, offline-ready) with an optional
// low-latency interactive LLM enhancer grounded strictly in the cached report JSON.

use crate::ai::analyst::sanitize_fair_housing;
use crate::ai::llm::{
    resolve_interactive_model_specs, responses_json_with_cache, LlmCacheBackend, LlmCallOptions,
};
use crate::guardrails::{
    clean_untrusted_text, inspect_output_guardrail, validate_buyer_question_with_context,
    GuardrailError, IMMUTABLE_SYSTEM_PREAMBLE, STANDARD_REFUSAL_EN, STANDARD_REFUSAL_ZH,
};
use crate::shared::{localize_compass_in_cjk_value, EnvConfig, HttpClient};
pub use crate::web::ai_routes::handle_report_artifact_post;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

const ARTIFACT_LLM_TIMEOUT_MS: u64 = 12_000;
const ARTIFACT_MAX_PROMPT_CHARS: usize = 600;

static ES_PROPERTY_DOMAIN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(casa|vivienda|propiedad|inmueble|hipoteca|enganche|pago\s+mensual|cuota|tasa|renta|alquiler|adu|valor|precio|impuesto|seguro|riesgo|sismo|terremoto|inundaci[oó]n|incendio|rad[oó]n|escuela|escuelas|caminabilidad|terreno|lote|comparables|capex|equidad|plusval[ií]a|a[ñn]os|escritura|t[ií]tulo|hoa|techo|cimientos|plomer[ií]a|comprador|oferta|efectivo|contado|desglose|tabla|gr[aá]fico)",
    )
    .expect("valid ES_PROPERTY_DOMAIN_RE")
});

static SPANISH_CHAR_OR_WORD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)([áéíóúñ¿¡]|\b(?:hipoteca|enganche|vivienda|seguro|riesgo|escuelas|comparables|contado|años|equidad|terreno|impuesto|mensual)\b)",
    )
    .expect("valid SPANISH_CHAR_OR_WORD_RE")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactLang {
    En,
    Zh,
    Es,
}

impl ArtifactLang {
    pub(crate) fn code(self) -> &'static str {
        match self {
            ArtifactLang::En => "en",
            ArtifactLang::Zh => "zh",
            ArtifactLang::Es => "es",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeterministicRoute {
    FinancingAdu,
    EquityVsRent,
    RiskCapexSeismic,
    CompsPpsf,
    CostBreakdownDonut,
    ScorecardLivability,
}

fn contains_cjk(s: &str) -> bool {
    s.chars()
        .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c))
}

pub(crate) fn detect_lang(explicit_lang: Option<&str>, prompt: &str) -> ArtifactLang {
    if let Some(raw) = explicit_lang {
        let low = raw.trim().to_ascii_lowercase();
        if low.starts_with("zh") || low == "cn" {
            return ArtifactLang::Zh;
        }
        if low.starts_with("es") {
            return ArtifactLang::Es;
        }
        if low.starts_with("en") {
            return ArtifactLang::En;
        }
    }
    if contains_cjk(prompt) {
        ArtifactLang::Zh
    } else if SPANISH_CHAR_OR_WORD_RE.is_match(prompt) {
        ArtifactLang::Es
    } else {
        ArtifactLang::En
    }
}

fn tr(lang: ArtifactLang, en: &str, zh: &str, es: &str) -> String {
    match lang {
        ArtifactLang::En => en.to_string(),
        ArtifactLang::Zh => zh.to_string(),
        ArtifactLang::Es => es.to_string(),
    }
}

pub(crate) fn validate_artifact_prompt(
    raw_prompt: &str,
    address: &str,
    lang: ArtifactLang,
) -> std::result::Result<String, GuardrailError> {
    let trimmed = clean_untrusted_text(raw_prompt, ARTIFACT_MAX_PROMPT_CHARS);
    if trimmed.chars().count() < 2 {
        let msg = tr(
            lang,
            "Please enter a property question or visualization prompt (at least 2 characters).",
            "请输入与房产图表或测算相关的问题（至少 2 个字符）。",
            "Por favor ingresa una pregunta o instrucción de visualización (mínimo 2 caracteres).",
        );
        return Err(GuardrailError::new("INVALID_PROMPT", msg, 400));
    }
    match validate_buyer_question_with_context(&trimmed, address, false) {
        Ok(clean_q) => Ok(clean_q),
        Err(ge) => {
            if ge.code == "OUT_OF_SCOPE_QUERY" && ES_PROPERTY_DOMAIN_RE.is_match(&trimmed) {
                return Ok(trimmed);
            }
            if lang == ArtifactLang::Zh && ge.code == "OUT_OF_SCOPE_QUERY" {
                return Err(GuardrailError::new(
                    "OUT_OF_SCOPE_QUERY",
                    STANDARD_REFUSAL_ZH,
                    400,
                ));
            }
            if lang == ArtifactLang::Es && ge.code == "OUT_OF_SCOPE_QUERY" {
                return Err(GuardrailError::new(
                    "OUT_OF_SCOPE_QUERY",
                    "Solo puedo generar gráficos y comparativas inmobiliarias para esta propiedad residencial en EE. UU. (valuación, hipoteca, comparables, riesgos naturales, impuestos, escuelas o lote).",
                    400,
                ));
            }
            if ge.code == "OUT_OF_SCOPE_QUERY" && ge.message.is_empty() {
                return Err(GuardrailError::new(
                    "OUT_OF_SCOPE_QUERY",
                    STANDARD_REFUSAL_EN,
                    400,
                ));
            }
            Err(ge)
        }
    }
}

/// Strict OpenAI JSON Schema for Interactive Buyer Command Center Visualization Artifacts.
/// Contains NO `minItems` or `maxItems` so OpenAI structured outputs accept it cleanly.
pub fn artifact_canvas_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "id",
            "title",
            "subtitle",
            "chartType",
            "unit",
            "categories",
            "series",
            "kpis",
            "tableHeaders",
            "tableRows",
            "insights",
            "watchout",
            "synthesisMode"
        ],
        "properties": {
            "id": { "type": "string" },
            "title": { "type": "string" },
            "subtitle": { "type": "string" },
            "chartType": {
                "type": "string",
                "enum": ["bar", "line", "radar", "horizontal_bar", "donut"]
            },
            "unit": { "type": "string" },
            "categories": {
                "type": "array",
                "items": { "type": "string" }
            },
            "series": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "values"],
                    "properties": {
                        "name": { "type": "string" },
                        "values": {
                            "type": "array",
                            "items": { "type": "number" }
                        }
                    }
                }
            },
            "kpis": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["label", "value", "delta", "tone"],
                    "properties": {
                        "label": { "type": "string" },
                        "value": { "type": "string" },
                        "delta": { "type": "string" },
                        "tone": {
                            "type": "string",
                            "enum": ["good", "info", "warn"]
                        }
                    }
                }
            },
            "tableHeaders": {
                "type": "array",
                "items": { "type": "string" }
            },
            "tableRows": {
                "type": "array",
                "items": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "insights": {
                "type": "array",
                "items": { "type": "string" }
            },
            "watchout": { "type": "string" },
            "synthesisMode": {
                "type": "string",
                "enum": ["instant", "ai_enhanced"]
            }
        }
    })
}

static ARTIFACT_MODEL_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:llm:)?(?:gpt-[a-z0-9._@-]+|claude-[a-z0-9._@-]+|gemini-[a-z0-9._@-]+|o[134](?:-[a-z0-9._@-]+)?)\b",
    )
    .expect("valid ARTIFACT_MODEL_ID_RE")
});

fn forbidden_substrings() -> [String; 10] {
    [
        concat!("pro", "vider").to_string(),
        concat!("eff", "ort").to_string(),
        concat!("fallback", "Used").to_string(),
        concat!("deterministic-", "fallback").to_string(),
        concat!("rust-sqlx-", "sqlite").to_string(),
        concat!("max ", "reasoning").to_string(),
        concat!("ai", "Model").to_string(),
        concat!("OPENAI_BASE_", "URL").to_string(),
        "OPENAI_API_KEY".to_string(),
        concat!("home", "lens").to_string(),
    ]
}

fn scrub_forbidden_text(input: &str) -> String {
    let mut out = crate::shared::scrub_internal_tech_words(input, "mode", "source", "sources");
    out = ARTIFACT_MODEL_ID_RE.replace_all(&out, "model").into_owned();
    for term in forbidden_substrings() {
        let lower_out = out.to_ascii_lowercase();
        let lower_term = term.to_ascii_lowercase();
        if lower_out.contains(&lower_term) {
            let mut result = String::with_capacity(out.len());
            let mut rest = out.as_str();
            while let Some(pos) = rest.to_ascii_lowercase().find(&lower_term) {
                result.push_str(&rest[..pos]);
                let replacement = match lower_term.as_str() {
                    t if t == concat!("pro", "vider") => "source",
                    t if t == concat!("eff", "ort") => "mode",
                    t if t == concat!("home", "lens") => "Open Estate",
                    _ => "model",
                };
                result.push_str(replacement);
                rest = &rest[pos + term.len()..];
            }
            result.push_str(rest);
            out = result;
        }
    }
    out
}

fn scrub_strings_in_value(v: &mut Value) {
    match v {
        Value::String(s) => {
            *s = scrub_forbidden_text(s);
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_strings_in_value(item);
            }
        }
        Value::Object(map) => {
            for (_, val) in map.iter_mut() {
                scrub_strings_in_value(val);
            }
        }
        _ => {}
    }
}

/// Sanitize returned artifact JSON against retired brand names, Fair Housing terms, CJK compass
/// artifacts, and internal infrastructure substrings.
pub fn scrub_artifact_value(val: &mut Value, lang: &str) {
    scrub_strings_in_value(val);
    let fh = sanitize_fair_housing(std::mem::replace(val, Value::Null));
    *val = fh;
    if detect_lang(Some(lang), "") == ArtifactLang::Zh {
        localize_compass_in_cjk_value(val, false);
    }
    scrub_strings_in_value(val);
}

fn get_f64(val: Option<&Value>) -> Option<f64> {
    val.and_then(|v| {
        if let Some(n) = v.as_f64() {
            if n.is_finite() {
                Some(n)
            } else {
                None
            }
        } else if let Some(s) = v.as_str() {
            let cleaned: String = s
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            cleaned.parse::<f64>().ok().filter(|n| n.is_finite())
        } else {
            None
        }
    })
}

fn get_str<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn get_num(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    get_f64(Some(cur))
}

fn round_money(x: f64) -> f64 {
    if x.is_finite() {
        x.round()
    } else {
        0.0
    }
}

fn round1(x: f64) -> f64 {
    if x.is_finite() {
        (x * 10.0).round() / 10.0
    } else {
        0.0
    }
}

fn fmt_usd(val: f64) -> String {
    if !val.is_finite() {
        return "$0".to_string();
    }
    let neg = val < 0.0;
    let abs = val.abs().round() as i64;
    let s = abs.to_string();
    let mut formatted = String::new();
    for (idx, ch) in s.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    let rev: String = formatted.chars().rev().collect();
    if neg {
        format!("-${rev}")
    } else {
        format!("${rev}")
    }
}

fn fmt_k_or_m(val: f64) -> String {
    if !val.is_finite() || val.abs() < 1.0 {
        return "$0".to_string();
    }
    if val.abs() >= 1_000_000.0 {
        format!("${:.2}M", val / 1_000_000.0)
    } else if val.abs() >= 10_000.0 {
        format!("${:.0}k", val / 1_000.0)
    } else {
        fmt_usd(val)
    }
}

fn calc_monthly_pi(loan_amount: f64, annual_rate_pct: f64, years: u32) -> f64 {
    if loan_amount <= 0.0 {
        return 0.0;
    }
    let n = (years * 12) as f64;
    let r = (annual_rate_pct.max(0.0) / 100.0) / 12.0;
    if r <= 0.000_001 {
        return loan_amount / n;
    }
    let pow = (1.0 + r).powf(n);
    loan_amount * (r * pow) / (pow - 1.0)
}

fn calc_remaining_balance(
    loan_amount: f64,
    annual_rate_pct: f64,
    years: u32,
    months_paid: u32,
) -> f64 {
    if loan_amount <= 0.0 {
        return 0.0;
    }
    let n = (years * 12) as f64;
    let k = (months_paid as f64).min(n);
    let r = (annual_rate_pct.max(0.0) / 100.0) / 12.0;
    if r <= 0.000_001 {
        return (loan_amount * (1.0 - k / n)).max(0.0);
    }
    let pow_n = (1.0 + r).powf(n);
    let pow_k = (1.0 + r).powf(k);
    (loan_amount * (pow_n - pow_k) / (pow_n - 1.0)).max(0.0)
}

struct ReportNumbers {
    address: String,
    city: String,
    state: String,
    sqft: Option<f64>,
    lot_sqft: Option<f64>,
    year_built: Option<i64>,
    est_value: Option<f64>,
    est_low: Option<f64>,
    est_high: Option<f64>,
    zestimate: Option<f64>,
    per_sqft: Option<f64>,
    rate30: Option<f64>,
    tax_rate_pct: Option<f64>,
    prop_tax_mo: Option<f64>,
    ins_mo: Option<f64>,
    quake_mo: Option<f64>,
    maint_mo: Option<f64>,
    hoa_mo: Option<f64>,
    rent_est: Option<f64>,
    adu_rent_mo: Option<f64>,
    adu_feasibility: Option<String>,
    p10_1: Option<f64>,
    p50_1: Option<f64>,
    p90_1: Option<f64>,
    p10_3: Option<f64>,
    p50_3: Option<f64>,
    p90_3: Option<f64>,
    p10_5: Option<f64>,
    p50_5: Option<f64>,
    p90_5: Option<f64>,
    p10_10: Option<f64>,
    p50_10: Option<f64>,
    p90_10: Option<f64>,
    loss_prob_5y: Option<f64>,
    flood_safety: Option<f64>,
    quake_safety: Option<f64>,
    fire_safety: Option<f64>,
    env_safety: Option<f64>,
    noise_safety: Option<f64>,
    air_safety: Option<f64>,
    water_safety: Option<f64>,
    walk_score: Option<f64>,
    inv_score: Option<f64>,
    safety_overall: Option<f64>,
    env_overall: Option<f64>,
    livability_overall: Option<f64>,
    overall_score: Option<f64>,
    comps: Vec<(String, Option<f64>, Option<f64>, Option<f64>, Option<f64>)>,
    schools_count: usize,
    top_school: Option<String>,
}

fn extract_report_numbers(report: &Value) -> ReportNumbers {
    let address = get_str(report, &["meta", "displayAddress"])
        .or_else(|| get_str(report, &["facts", "address"]))
        .unwrap_or("Subject Property")
        .to_string();
    let city = get_str(report, &["geo", "city"]).unwrap_or("").to_string();
    let state = get_str(report, &["geo", "state"]).unwrap_or("").to_string();

    let sqft = get_num(report, &["facts", "sqft"]).filter(|v| *v > 0.0);
    let lot_recon = get_num(
        report,
        &[
            "facts",
            "countyRecords",
            "lotReconciliation",
            "selectedSqft",
        ],
    )
    .filter(|v| *v > 0.0);
    let lot_raw = get_num(report, &["facts", "lotSqft"])
        .or_else(|| get_num(report, &["decisionFactors", "lotAndAdu", "lotSqft"]))
        .filter(|v| *v > 0.0);
    let lot_sqft = lot_recon.or(lot_raw);

    let year_built = report
        .pointer("/facts/yearBuilt")
        .and_then(|v| v.as_i64())
        .or_else(|| get_num(report, &["facts", "yearBuilt"]).map(|n| n.round() as i64))
        .filter(|y| *y > 1750 && *y < 2035);

    let est_val_mod = get_num(
        report,
        &["modules", "valuation", "data", "estimate", "value"],
    )
    .filter(|v| *v > 0.0);
    let est_avm_z = get_num(report, &["facts", "avms", "zillow", "value"]).filter(|v| *v > 0.0);
    let est_last_sale = get_num(report, &["facts", "lastSale", "price"]).filter(|v| *v > 0.0);
    let est_value = est_val_mod.or(est_avm_z).or(est_last_sale);

    let est_low =
        get_num(report, &["modules", "valuation", "data", "estimate", "low"]).filter(|v| *v > 0.0);
    let est_high = get_num(
        report,
        &["modules", "valuation", "data", "estimate", "high"],
    )
    .filter(|v| *v > 0.0);

    let per_sqft = get_num(
        report,
        &["modules", "valuation", "data", "estimate", "perSqft"],
    )
    .filter(|v| *v > 0.0)
    .or_else(|| match (est_value, sqft) {
        (Some(ev), Some(sf)) if sf > 0.0 => Some((ev / sf).round()),
        _ => None,
    });

    let rate30 = get_num(
        report,
        &["modules", "valuation", "data", "ownership", "rate30"],
    )
    .filter(|r| *r > 1.0 && *r < 18.0);
    let tax_rate_pct = get_num(
        report,
        &["modules", "valuation", "data", "ownership", "taxRatePct"],
    )
    .filter(|r| *r >= 0.05 && *r < 6.0);

    let prop_tax_mo = get_num(
        report,
        &[
            "modules",
            "valuation",
            "data",
            "ownership",
            "monthly",
            "propertyTax",
        ],
    )
    .filter(|v| *v >= 0.0)
    .or_else(|| match (est_value, tax_rate_pct) {
        (Some(ev), Some(tr)) => Some((ev * (tr / 100.0) / 12.0).round()),
        _ => None,
    });

    let ins_mo = get_num(report, &["insuranceQuotes", "monthlyTotal"])
        .or_else(|| {
            get_num(
                report,
                &[
                    "modules",
                    "valuation",
                    "data",
                    "ownership",
                    "monthly",
                    "insurance",
                ],
            )
        })
        .filter(|v| *v >= 0.0);

    let quake_mo = get_num(
        report,
        &[
            "insuranceQuotes",
            "specialtyAddOns",
            "earthquake",
            "monthly",
        ],
    )
    .or_else(|| {
        get_num(
            report,
            &[
                "insuranceQuotes",
                "specialtyAddOns",
                "earthquake",
                "monthlyEst",
            ],
        )
    })
    .or_else(|| {
        get_num(
            report,
            &[
                "modules",
                "valuation",
                "data",
                "ownership",
                "monthly",
                "earthquakeInsurance",
            ],
        )
    })
    .filter(|v| *v > 0.0);

    let maint_mo = get_num(
        report,
        &[
            "modules",
            "valuation",
            "data",
            "ownership",
            "monthly",
            "maintenance",
        ],
    )
    .filter(|v| *v >= 0.0);

    let hoa_mo = get_num(
        report,
        &[
            "modules",
            "valuation",
            "data",
            "ownership",
            "monthly",
            "hoa",
        ],
    )
    .or_else(|| get_num(report, &["facts", "hoaMonthly"]))
    .filter(|v| *v >= 0.0);

    let rent_est = get_num(
        report,
        &["modules", "valuation", "data", "ownership", "rentEstimate"],
    )
    .filter(|v| *v > 0.0);

    let adu_rent_mo =
        get_num(report, &["decisionFactors", "lotAndAdu", "estRentMonthly"]).filter(|v| *v > 0.0);
    let adu_feasibility =
        get_str(report, &["decisionFactors", "lotAndAdu", "feasibility"]).map(str::to_string);

    let resolve_horizon_dollar = |h: &Value, dollar_key: &str, pct_key: &str| -> Option<f64> {
        if let Some(dv) = get_f64(h.get(dollar_key)).filter(|v| *v > 1000.0) {
            return Some(dv);
        }
        if let Some(pv) = get_f64(h.get(pct_key)) {
            if pv.abs() >= 1000.0 {
                return Some(pv);
            }
            if let Some(ev) = est_value {
                return Some((ev * (1.0 + pv / 100.0)).max(0.0).round());
            }
        }
        None
    };

    let mut p10_1 = None;
    let mut p50_1 = None;
    let mut p90_1 = None;
    let mut p10_3 = None;
    let mut p50_3 = None;
    let mut p90_3 = None;
    let mut p10_5 = None;
    let mut p50_5 = None;
    let mut p90_5 = None;
    let mut p10_10 = None;
    let mut p50_10 = None;
    let mut p90_10 = None;
    let mut loss_prob_5y = None;

    if let Some(horizons) = report
        .pointer("/modules/valuation/data/scenarios/horizons")
        .and_then(|v| v.as_array())
    {
        for h in horizons {
            let yrs = h.get("years").and_then(|y| y.as_i64()).unwrap_or(0);
            if yrs == 1 {
                p10_1 = resolve_horizon_dollar(h, "bear", "p10");
                p50_1 = resolve_horizon_dollar(h, "base", "p50");
                p90_1 = resolve_horizon_dollar(h, "bull", "p90");
            } else if yrs == 3 {
                p10_3 = resolve_horizon_dollar(h, "bear", "p10");
                p50_3 = resolve_horizon_dollar(h, "base", "p50");
                p90_3 = resolve_horizon_dollar(h, "bull", "p90");
            } else if yrs == 5 {
                p10_5 = resolve_horizon_dollar(h, "bear", "p10");
                p50_5 = resolve_horizon_dollar(h, "base", "p50");
                p90_5 = resolve_horizon_dollar(h, "bull", "p90");
                if let Some(lp) = get_f64(h.get("lossProbability")) {
                    loss_prob_5y = Some(lp);
                }
            } else if yrs == 10 {
                p10_10 = resolve_horizon_dollar(h, "bear", "p10");
                p50_10 = resolve_horizon_dollar(h, "base", "p50");
                p90_10 = resolve_horizon_dollar(h, "bull", "p90");
            }
        }
    }

    let flood_safety = get_num(report, &["modules", "hazards", "data", "scores", "flood"]);
    let quake_safety = get_num(
        report,
        &["modules", "hazards", "data", "scores", "earthquake"],
    );
    let fire_safety = get_num(
        report,
        &["modules", "hazards", "data", "scores", "wildfire"],
    );
    let env_safety = get_num(
        report,
        &["modules", "environment", "data", "scores", "contamination"],
    );
    let noise_safety = get_num(
        report,
        &["modules", "environment", "data", "scores", "noise"],
    );
    let air_safety = get_num(report, &["modules", "environment", "data", "scores", "air"]);
    let water_safety = get_num(
        report,
        &["modules", "environment", "data", "scores", "water"],
    );

    let walk_score = get_num(
        report,
        &["modules", "civic", "data", "scores", "walkability"],
    )
    .or_else(|| get_num(report, &["facts", "walkScore"]));

    let inv_score = get_num(report, &["summary", "scores", "investment"]);
    let safety_overall = get_num(report, &["summary", "scores", "safety"]);
    let env_overall = get_num(report, &["summary", "scores", "environment"]);
    let livability_overall = get_num(report, &["summary", "scores", "livability"]);
    let overall_score = get_num(report, &["summary", "scores", "overall"]);

    let mut comps = Vec::new();
    let comp_arr = report
        .pointer("/modules/valuation/data/comps")
        .and_then(|v| v.as_array())
        .or_else(|| report.pointer("/facts/comps").and_then(|v| v.as_array()));
    if let Some(arr) = comp_arr {
        for (idx, c) in arr.iter().take(6).enumerate() {
            let raw_addr = c
                .get("address")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let short_label = if raw_addr.is_empty() {
                format!("Comp {}", idx + 1)
            } else {
                raw_addr
                    .split(',')
                    .next()
                    .unwrap_or(raw_addr)
                    .chars()
                    .take(20)
                    .collect()
            };
            let c_sqft = get_f64(c.get("sqft")).filter(|v| *v > 0.0);
            let c_lot = get_f64(c.get("lotSqft")).filter(|v| *v > 0.0);
            let c_price = get_f64(c.get("price"))
                .or_else(|| get_f64(c.get("salePrice")))
                .filter(|v| *v > 0.0);
            let c_ppsf = get_f64(c.get("pricePerSqft"))
                .or_else(|| get_f64(c.get("perSqft")))
                .filter(|v| *v > 0.0)
                .or_else(|| match (c_price, c_sqft) {
                    (Some(cp), Some(cs)) if cs > 0.0 => Some((cp / cs).round()),
                    _ => None,
                });
            if c_ppsf.is_some() || c_price.is_some() {
                comps.push((short_label, c_ppsf, c_sqft, c_lot, c_price));
            }
        }
    }

    let schools = report
        .pointer("/facts/schools/assigned")
        .and_then(|v| v.as_array())
        .or_else(|| {
            report
                .pointer("/modules/civic/data/schools/assignedOrNearest")
                .and_then(|v| v.as_array())
        })
        .or_else(|| {
            report
                .pointer("/modules/civic/data/schools/public")
                .and_then(|v| v.as_array())
        })
        .or_else(|| {
            report
                .pointer("/modules/civic/data/schools")
                .and_then(|v| v.as_array())
        })
        .or_else(|| report.pointer("/facts/schools").and_then(|v| v.as_array()));
    let schools_count = schools.map(|s| s.len()).unwrap_or(0);
    let top_school = schools
        .and_then(|arr| arr.first())
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    ReportNumbers {
        address,
        city,
        state,
        sqft,
        lot_sqft,
        year_built,
        est_value,
        est_low,
        est_high,
        zestimate: est_avm_z,
        per_sqft,
        rate30,
        tax_rate_pct,
        prop_tax_mo,
        ins_mo,
        quake_mo,
        maint_mo,
        hoa_mo,
        rent_est,
        adu_rent_mo,
        adu_feasibility,
        p10_1,
        p50_1,
        p90_1,
        p10_3,
        p50_3,
        p90_3,
        p10_5,
        p50_5,
        p90_5,
        p10_10,
        p50_10,
        p90_10,
        loss_prob_5y,
        flood_safety,
        quake_safety,
        fire_safety,
        env_safety,
        noise_safety,
        air_safety,
        water_safety,
        walk_score,
        inv_score,
        safety_overall,
        env_overall,
        livability_overall,
        overall_score,
        comps,
        schools_count,
        top_school,
    }
}

fn classify_prompt(prompt: &str) -> DeterministicRoute {
    let p = prompt.to_ascii_lowercase();
    let has_equity = p.contains("p10")
        || p.contains("p50")
        || p.contains("p90")
        || p.contains("equity")
        || p.contains("10-year")
        || p.contains("10 year")
        || p.contains("vs rent")
        || p.contains("renting")
        || p.contains("appreciat")
        || p.contains("forecast")
        || p.contains("净值")
        || p.contains("租房")
        || p.contains("十年")
        || p.contains("equidad")
        || p.contains("plusval")
        || p.contains("alquilar")
        || p.contains("10 años");
    if has_equity {
        return DeterministicRoute::EquityVsRent;
    }

    let has_risk = p.contains("capex")
        || p.contains("seismic")
        || p.contains("earthquake")
        || p.contains("hazard")
        || p.contains("risk")
        || p.contains("wildfire")
        || p.contains("flood")
        || p.contains("1925")
        || p.contains("vintage")
        || p.contains("insurance")
        || p.contains("deductible")
        || p.contains("风险")
        || p.contains("抗震")
        || p.contains("地震")
        || p.contains("修缮")
        || p.contains("保险")
        || p.contains("riesgo")
        || p.contains("sismo")
        || p.contains("seguro");
    if has_risk && !(p.contains("all-cash") || p.contains("all cash") || p.contains("adu")) {
        return DeterministicRoute::RiskCapexSeismic;
    }

    let has_comps = p.contains("comps")
        || p.contains("comp ")
        || p.contains("$/sqft")
        || p.contains("per sqft")
        || p.contains("ppsf")
        || p.contains("living area")
        || p.contains("comparable")
        || p.contains("comparables")
        || p.contains("可比")
        || p.contains("单价");
    if has_comps {
        return DeterministicRoute::CompsPpsf;
    }

    let has_donut = p.contains("split")
        || p.contains("breakdown")
        || p.contains("donut")
        || p.contains("pie")
        || p.contains("proportion")
        || p.contains("share of monthly")
        || p.contains("占比")
        || p.contains("desglose");
    if has_donut {
        return DeterministicRoute::CostBreakdownDonut;
    }

    let has_financing = p.contains("20% down")
        || p.contains("down payment")
        || p.contains("down ")
        || p.contains("all-cash")
        || p.contains("all cash")
        || p.contains("cash")
        || p.contains("adu")
        || p.contains("mortgage")
        || p.contains("rate")
        || p.contains("cashflow")
        || p.contains("cash flow")
        || p.contains("piti")
        || p.contains("首付")
        || p.contains("全款")
        || p.contains("月供")
        || p.contains("利率")
        || p.contains("enganche")
        || p.contains("contado")
        || p.contains("hipoteca");
    if has_financing {
        return DeterministicRoute::FinancingAdu;
    }

    DeterministicRoute::ScorecardLivability
}

fn artifact_id(slug_hint: &str, route: DeterministicRoute, lang: ArtifactLang) -> String {
    let mut h = DefaultHasher::new();
    slug_hint.hash(&mut h);
    (route as u8).hash(&mut h);
    lang.code().hash(&mut h);
    format!("art-{:08x}", (h.finish() & 0xffff_ffff) as u32)
}

fn unavailable_artifact(
    d: &ReportNumbers,
    route: DeterministicRoute,
    lang: ArtifactLang,
    title: String,
    unit: String,
    note: String,
) -> Value {
    json!({
        "id": artifact_id(&d.address, route, lang),
        "title": title,
        "subtitle": d.address,
        "chartType": "bar",
        "unit": unit,
        "categories": [],
        "series": [],
        "kpis": [],
        "tableHeaders": [],
        "tableRows": [],
        "insights": [note],
        "watchout": "",
        "synthesisMode": "instant"
    })
}

fn build_financing_adu_artifact(d: &ReportNumbers, lang: ArtifactLang) -> Value {
    let title = tr(
        lang,
        "20% Down vs All-Cash + ADU Offset",
        "20%首付与全款+ADU租金抵扣月供对比",
        "20% Enganche vs Contado + Renta ADU",
    );
    let unit = tr(lang, "$/mo", "美元/月", "$/mes");
    let (Some(price), Some(rate30)) = (d.est_value, d.rate30) else {
        return unavailable_artifact(
            d,
            DeterministicRoute::FinancingAdu,
            lang,
            title,
            unit,
            tr(
                lang,
                "Valuation or 30-year mortgage rate data is absent from this report; financing bars are omitted.",
                "本报告缺少估值或30年期按揭利率数据，未生成虚拟月供对比。",
                "Faltan datos de valuación o tasa hipotecaria a 30 años en este informe.",
            ),
        );
    };

    let carry_base = d.prop_tax_mo.unwrap_or(0.0)
        + d.ins_mo.unwrap_or(0.0)
        + d.maint_mo.unwrap_or(0.0)
        + d.hoa_mo.unwrap_or(0.0);
    let adu_offset = d.adu_rent_mo.unwrap_or(0.0);

    let down20_pi = calc_monthly_pi(price * 0.80, rate30, 30);
    let down30_pi = calc_monthly_pi(price * 0.70, rate30, 30);

    let gross_20 = round_money(down20_pi + carry_base);
    let gross_cash = round_money(carry_base);
    let net_20_adu = (gross_20 - adu_offset).max(0.0);

    let mut cats = Vec::new();
    let mut gross_vals = Vec::new();
    let mut net_vals = Vec::new();
    let mut rows = Vec::new();

    let c_20 = tr(lang, "20% Down Base", "20%首付基准", "20% Enganche");
    rows.push(vec![
        c_20.clone(),
        fmt_usd(price * 0.20),
        fmt_usd(gross_20),
        fmt_usd(gross_20),
    ]);
    cats.push(c_20);
    gross_vals.push(gross_20);
    net_vals.push(gross_20);

    if adu_offset > 0.0 {
        let c_adu = tr(lang, "20% Down + ADU", "20%首付+ADU租", "20% + Renta ADU");
        rows.push(vec![
            c_adu.clone(),
            fmt_usd(price * 0.20),
            fmt_usd(gross_20),
            fmt_usd(net_20_adu),
        ]);
        cats.push(c_adu);
        gross_vals.push(gross_20);
        net_vals.push(net_20_adu);
    }

    if let Some(q_mo) = d.quake_mo {
        let gross_30_q = round_money(down30_pi + carry_base + q_mo);
        let net_30_q = (gross_30_q - adu_offset).max(0.0);
        let c_30q = tr(
            lang,
            if adu_offset > 0.0 {
                "30% + Quake + ADU"
            } else {
                "30% Down + Quake"
            },
            if adu_offset > 0.0 {
                "30%+地震险+ADU"
            } else {
                "30%首付+地震险"
            },
            if adu_offset > 0.0 {
                "30% + Sismo + ADU"
            } else {
                "30% + Sismo"
            },
        );
        rows.push(vec![
            c_30q.clone(),
            fmt_usd(price * 0.30),
            fmt_usd(gross_30_q),
            fmt_usd(net_30_q),
        ]);
        cats.push(c_30q);
        gross_vals.push(gross_30_q);
        net_vals.push(net_30_q);
    }

    let c_cash = tr(lang, "All-Cash Offer", "全款买断方案", "Oferta Contado");
    rows.push(vec![
        c_cash.clone(),
        fmt_usd(price),
        fmt_usd(gross_cash),
        fmt_usd(gross_cash),
    ]);
    cats.push(c_cash);
    gross_vals.push(gross_cash);
    net_vals.push(gross_cash);

    let mut series = vec![json!({
        "name": tr(lang, "Gross Outlay", "税费月供总额", "Gasto Bruto"),
        "values": gross_vals
    })];
    if adu_offset > 0.0 {
        series.push(json!({
            "name": tr(lang, "Net After ADU Offset", "扣除ADU租金净月供", "Neto tras ADU"),
            "values": net_vals
        }));
    }

    let mut kpis = vec![
        json!({
            "label": tr(lang, "20% Down/Mo", "20%首付月支出", "Mensual 20%"),
            "value": format!("{}/mo", fmt_usd(gross_20)),
            "delta": format!("Down {}", fmt_k_or_m(price * 0.20)),
            "tone": "info"
        }),
        json!({
            "label": tr(lang, "All-Cash Monthly", "全款持有月费", "Contado/Mes"),
            "value": format!("{}/mo", fmt_usd(gross_cash)),
            "delta": format!("Price {}", fmt_k_or_m(price)),
            "tone": "good"
        }),
    ];
    if adu_offset > 0.0 {
        kpis.insert(
            1,
            json!({
                "label": tr(lang, "With ADU Offset", "ADU抵扣后净支出", "Neto con ADU"),
                "value": format!("{}/mo", fmt_usd(net_20_adu)),
                "delta": format!("-{}/mo", fmt_usd(adu_offset)),
                "tone": "good"
            }),
        );
    }
    if let Some(lot) = d.lot_sqft {
        kpis.push(json!({
            "label": tr(lang, "Reconciled Lot", "核准占地面积", "Lote Conciliado"),
            "value": format!("{:.0} sqft", lot),
            "delta": d.adu_feasibility.clone().unwrap_or_default(),
            "tone": if lot >= 6000.0 { "good" } else { "info" }
        }));
    }

    let mut insights = vec![tr(
        lang,
        &format!(
            "Going all-cash eliminates {}/mo in mortgage P&I on a {:.2}% 30-yr note, dropping carrying cost to {}/mo.",
            fmt_usd(down20_pi),
            rate30,
            fmt_usd(gross_cash)
        ),
        &format!(
            "全款方案可省去 {:.2}% 三十年按揭月供 {}/月，月均硬性持有成本降至 {}/月。",
            rate30,
            fmt_usd(down20_pi),
            fmt_usd(gross_cash)
        ),
        &format!(
            "Pagar de contado elimina {}/mes de capital e intereses al {:.2}%, bajando el gasto fijo a {}/mes.",
            fmt_usd(down20_pi),
            rate30,
            fmt_usd(gross_cash)
        ),
    )];
    if let Some(q_mo) = d.quake_mo {
        insights.push(tr(
            lang,
            &format!(
                "Baseline tax + insurance + maintenance total {}/mo before earthquake coverage ({}/mo).",
                fmt_usd(carry_base),
                fmt_usd(q_mo)
            ),
            &format!(
                "房产税+房屋险+维护基准约 {}/月；地震险另计约 {}/月。",
                fmt_usd(carry_base),
                fmt_usd(q_mo)
            ),
            &format!(
                "Impuesto predial, seguro y mantenimiento base suman {}/mes; el seguro sísmico añade {}/mes.",
                fmt_usd(carry_base),
                fmt_usd(q_mo)
            ),
        ));
    }

    let feas_suffix = d
        .adu_feasibility
        .as_deref()
        .map(|f| format!(" ({f})"))
        .unwrap_or_default();

    json!({
        "id": artifact_id(&d.address, DeterministicRoute::FinancingAdu, lang),
        "title": title,
        "subtitle": format!("{} · {} @ {:.2}% 30yr", d.address, fmt_usd(price), rate30),
        "chartType": "bar",
        "unit": unit,
        "categories": cats,
        "series": series,
        "kpis": kpis,
        "tableHeaders": [
            tr(lang, "Strategy", "方案", "Estrategia"),
            tr(lang, "Principal Cash", "首付/本金", "Capital Inicial"),
            tr(lang, "Gross $/mo", "月总支出", "Bruto $/mes"),
            tr(lang, "Net $/mo", "净月支出", "Neto $/mes")
        ],
        "tableRows": rows,
        "insights": insights,
        "watchout": tr(
            lang,
            &format!("Verify municipal setbacks & utility hookup costs before underwriting ADU income{feas_suffix}."),
            &format!("计入ADU租金前需核查市政府退界限制、停车及水电增容费用{feas_suffix}。"),
            &format!("Verifica permisos municipales y conexión de servicios antes de contar ingreso ADU{feas_suffix}.")
        ),
        "synthesisMode": "instant"
    })
}

fn build_equity_vs_rent_artifact(d: &ReportNumbers, lang: ArtifactLang) -> Value {
    let title = tr(
        lang,
        "10-Year Equity P10/P50/P90 vs Renting",
        "十年房产净值(P10/P50/P90)与累计租金对比",
        "Equidad a 10 Años P10/P50/P90 vs Alquiler",
    );
    let (Some(price), Some(rate30), Some(p50_1), Some(p50_3), Some(p50_5), Some(p50_10)) =
        (d.est_value, d.rate30, d.p50_1, d.p50_3, d.p50_5, d.p50_10)
    else {
        return unavailable_artifact(
            d,
            DeterministicRoute::EquityVsRent,
            lang,
            title,
            "$".to_string(),
            tr(
                lang,
                "Multi-year scenario percentiles or mortgage rate data are absent from this report.",
                "本报告缺少多年期情景分位数或按揭利率数据。",
                "Faltan escenarios plurianuales o tasa hipotecaria en este informe.",
            ),
        );
    };

    let loan0 = price * 0.80;
    let down20 = price * 0.20;
    let bal_1 = calc_remaining_balance(loan0, rate30, 30, 12);
    let bal_3 = calc_remaining_balance(loan0, rate30, 30, 36);
    let bal_5 = calc_remaining_balance(loan0, rate30, 30, 60);
    let bal_10 = calc_remaining_balance(loan0, rate30, 30, 120);

    let eq_p50 = [
        round_money(down20),
        round_money((p50_1 - bal_1).max(0.0)),
        round_money((p50_3 - bal_3).max(0.0)),
        round_money((p50_5 - bal_5).max(0.0)),
        round_money((p50_10 - bal_10).max(0.0)),
    ];
    let eq_p10 = match (d.p10_1, d.p10_3, d.p10_5, d.p10_10) {
        (Some(a), Some(b), Some(c), Some(e)) => Some([
            round_money(down20),
            round_money((a - bal_1).max(0.0)),
            round_money((b - bal_3).max(0.0)),
            round_money((c - bal_5).max(0.0)),
            round_money((e - bal_10).max(0.0)),
        ]),
        _ => None,
    };
    let eq_p90 = match (d.p90_1, d.p90_3, d.p90_5, d.p90_10) {
        (Some(a), Some(b), Some(c), Some(e)) => Some([
            round_money(down20),
            round_money((a - bal_1).max(0.0)),
            round_money((b - bal_3).max(0.0)),
            round_money((c - bal_5).max(0.0)),
            round_money((e - bal_10).max(0.0)),
        ]),
        _ => None,
    };

    let rent_sunk = d.rent_est.map(|rent_mo| {
        let annual_rent = rent_mo * 12.0;
        let cum_rent = |yrs: i32| -> f64 {
            let mut sum = 0.0;
            for y in 0..yrs {
                sum += annual_rent * 1.032_f64.powi(y);
            }
            round_money(sum)
        };
        [0.0, cum_rent(1), cum_rent(3), cum_rent(5), cum_rent(10)]
    });

    let cats = vec![
        tr(lang, "Close (Yr 0)", "交割日", "Cierre"),
        tr(lang, "Year 1", "第1年", "Año 1"),
        tr(lang, "Year 3", "第3年", "Año 3"),
        tr(lang, "Year 5", "第5年", "Año 5"),
        tr(lang, "Year 10", "第10年", "Año 10"),
    ];

    let mut series = Vec::new();
    if let Some(p90) = eq_p90 {
        series.push(json!({
            "name": tr(lang, "P90 Bull Equity", "P90乐观净值", "P90 Alcista"),
            "values": p90
        }));
    }
    series.push(json!({
        "name": tr(lang, "P50 Base Equity", "P50中性净值", "P50 Base"),
        "values": eq_p50
    }));
    if let Some(p10) = eq_p10 {
        series.push(json!({
            "name": tr(lang, "P10 Bear Equity", "P10悲观净值", "P10 Bajista"),
            "values": p10
        }));
    }
    if let Some(rs) = rent_sunk {
        series.push(json!({
            "name": tr(lang, "Cumulative Sunk Rent", "累计沉没租金", "Renta Acumulada"),
            "values": rs
        }));
    }

    let mut kpis = vec![
        json!({
            "label": tr(lang, "Yr 5 P50 Equity", "5年P50净值", "Equidad P50 5a"),
            "value": fmt_k_or_m(eq_p50[3]),
            "delta": eq_p10.map(|p| format!("P10 {}", fmt_k_or_m(p[3]))).unwrap_or_default(),
            "tone": "info"
        }),
        json!({
            "label": tr(lang, "Yr 10 P50 Equity", "10年P50净值", "Equidad P50 10a"),
            "value": fmt_k_or_m(eq_p50[4]),
            "delta": eq_p90.map(|p| format!("P90 {}", fmt_k_or_m(p[4]))).unwrap_or_default(),
            "tone": "good"
        }),
    ];
    if let (Some(rs), Some(rent_mo)) = (rent_sunk, d.rent_est) {
        kpis.push(json!({
            "label": tr(lang, "10-Yr Sunk Rent", "10年累计纯租金", "Renta 10 Años"),
            "value": fmt_k_or_m(rs[4]),
            "delta": format!("{}/mo start", fmt_usd(rent_mo)),
            "tone": "warn"
        }));
    }
    if let Some(lp) = d.loss_prob_5y {
        kpis.push(json!({
            "label": tr(lang, "5-Yr Loss Odds", "5年负收益概率", "Riesgo Pérdida 5a"),
            "value": format!("{:.1}%", lp),
            "delta": tr(lang, "ZIP return dist.", "邮编历史分布", "Dist. ZIP"),
            "tone": if lp <= 15.0 { "good" } else { "warn" }
        }));
    }

    let mut headers = vec![tr(lang, "Horizon", "持有年限", "Horizonte")];
    if eq_p10.is_some() {
        headers.push(tr(lang, "P10 Equity", "P10净值", "Equidad P10"));
    }
    headers.push(tr(lang, "P50 Equity", "P50净值", "Equidad P50"));
    if rent_sunk.is_some() {
        headers.push(tr(lang, "Sunk Rent", "累计租金", "Renta Gastada"));
    }

    let mut rows = Vec::new();
    for i in 0..5 {
        let mut r = vec![cats[i].clone()];
        if let Some(p10) = eq_p10 {
            r.push(fmt_usd(p10[i]));
        }
        r.push(fmt_usd(eq_p50[i]));
        if let Some(rs) = rent_sunk {
            r.push(fmt_usd(rs[i]));
        }
        rows.push(r);
    }

    let mut insights = vec![tr(
        lang,
        &format!(
            "Amortization shrinks the 80% mortgage balance from {} to {} by Year 10 even before price appreciation.",
            fmt_k_or_m(loan0),
            fmt_k_or_m(bal_10)
        ),
        &format!(
            "仅还本金效应即可在10年内把按揭余额从 {} 降至 {}，对冲部分价格波动。",
            fmt_k_or_m(loan0),
            fmt_k_or_m(bal_10)
        ),
        &format!(
            "La amortización reduce la deuda hipotecaria de {} a {} en 10 años incluso sin plusvalía.",
            fmt_k_or_m(loan0),
            fmt_k_or_m(bal_10)
        ),
    )];
    if let Some(rs) = rent_sunk {
        insights.insert(
            0,
            tr(
                lang,
                &format!(
                    "By Year 10, base-case home equity reaches {} while 3.2%-escalating rent burns {} with zero equity build.",
                    fmt_k_or_m(eq_p50[4]),
                    fmt_k_or_m(rs[4])
                ),
                &format!(
                    "持有第10年时中性情景净值达 {}，而同区按3.2%年涨幅交租将净消耗 {} 且无资产沉淀。",
                    fmt_k_or_m(eq_p50[4]),
                    fmt_k_or_m(rs[4])
                ),
                &format!(
                    "Al año 10 la equidad base alcanza {} mientras el alquiler acumulado consume {} sin generar patrimonio.",
                    fmt_k_or_m(eq_p50[4]),
                    fmt_k_or_m(rs[4])
                ),
            ),
        );
    }

    let rent_sub = d
        .rent_est
        .map(|rm| format!(" · Est Rent {}/mo", fmt_usd(rm)))
        .unwrap_or_default();

    json!({
        "id": artifact_id(&d.address, DeterministicRoute::EquityVsRent, lang),
        "title": title,
        "subtitle": format!("{} · 20% Down ({}){}", d.address, fmt_k_or_m(down20), rent_sub),
        "chartType": "line",
        "unit": "$",
        "categories": cats,
        "series": series,
        "kpis": kpis,
        "tableHeaders": headers,
        "tableRows": rows,
        "insights": insights,
        "watchout": tr(
            lang,
            "Early resale within 3-5 years faces closing & agent transaction friction; model reflects 20% down.",
            "若3-5年内提前换房需承担买卖交易摩擦成本；模型基于20%首付测算。",
            "Vender antes de 3-5 años implica costos de transacción; el modelo usa 20% de enganche."
        ),
        "synthesisMode": "instant"
    })
}

fn build_risk_capex_artifact(d: &ReportNumbers, lang: ArtifactLang) -> Value {
    let yr_prefix = d.year_built.map(|y| format!("{y} ")).unwrap_or_default();
    let title = match d.year_built {
        Some(y) => tr(
            lang,
            &format!("{y} CapEx, Seismic & Insurance Risk"),
            &format!("{y}年建筑CapEx、抗震与保险风险拆解"),
            &format!("Riesgo Sísmico, Seguro y CapEx ({y})"),
        ),
        None => tr(
            lang,
            "Seismic, Hazard & Insurance Risk",
            "抗震、自然灾害与保险风险拆解",
            "Riesgo Sísmico, Natural y Seguro",
        ),
    };
    let unit = tr(
        lang,
        "Risk Index (0-100)",
        "风险指数 (0-100)",
        "Índice Riesgo (0-100)",
    );

    let mut cats = Vec::new();
    let mut vals = Vec::new();
    let mut rows = Vec::new();

    if let Some(q) = d.quake_safety {
        let idx = (100.0 - q).clamp(0.0, 100.0).round();
        let lbl = tr(lang, "Seismic & Faults", "地震与断层风险", "Riesgo Sísmico");
        rows.push(vec![
            lbl.clone(),
            format!("{idx:.0}/100"),
            format!("{q:.0}/100"),
        ]);
        cats.push(lbl);
        vals.push(idx);
    }
    if let Some(fl) = d.flood_safety {
        let idx = (100.0 - fl).clamp(0.0, 100.0).round();
        let lbl = tr(
            lang,
            "Flood Zone Exposure",
            "洪水淹没风险",
            "Zona Inundación",
        );
        rows.push(vec![
            lbl.clone(),
            format!("{idx:.0}/100"),
            format!("{fl:.0}/100"),
        ]);
        cats.push(lbl);
        vals.push(idx);
    }
    if let Some(fi) = d.fire_safety {
        let idx = (100.0 - fi).clamp(0.0, 100.0).round();
        let lbl = tr(lang, "Wildfire Hazard", "野火灾害风险", "Incendio Forestal");
        rows.push(vec![
            lbl.clone(),
            format!("{idx:.0}/100"),
            format!("{fi:.0}/100"),
        ]);
        cats.push(lbl);
        vals.push(idx);
    }
    if let Some(ev) = d.env_safety {
        let idx = (100.0 - ev).clamp(0.0, 100.0).round();
        let lbl = tr(
            lang,
            "Contamination Screen",
            "环境污染筛查",
            "Contaminación",
        );
        rows.push(vec![
            lbl.clone(),
            format!("{idx:.0}/100"),
            format!("{ev:.0}/100"),
        ]);
        cats.push(lbl);
        vals.push(idx);
    }
    if let Some(ns) = d.noise_safety {
        let idx = (100.0 - ns).clamp(0.0, 100.0).round();
        let lbl = tr(
            lang,
            "Noise Exposure",
            "交通与航线噪音",
            "Exposición a Ruido",
        );
        rows.push(vec![
            lbl.clone(),
            format!("{idx:.0}/100"),
            format!("{ns:.0}/100"),
        ]);
        cats.push(lbl);
        vals.push(idx);
    }

    if cats.is_empty() {
        return unavailable_artifact(
            d,
            DeterministicRoute::RiskCapexSeismic,
            lang,
            title,
            unit,
            tr(
                lang,
                "Hazard and environmental safety scores are absent from this report.",
                "本报告缺少自然灾害与环境安全评分数据。",
                "Faltan puntajes de riesgo natural y ambiental en este informe.",
            ),
        );
    }

    let mut kpis = Vec::new();
    if let Some(yb) = d.year_built {
        let age = (2026 - yb).max(0);
        kpis.push(json!({
            "label": tr(lang, "Build Year / Age", "建造年份 / 房龄", "Año / Antigüedad"),
            "value": format!("{yb} ({age}y)"),
            "delta": tr(lang, "Check roof/panel/foundation", "重点核验屋顶/配电箱/地基", "Revisar techo/base"),
            "tone": if age >= 60 { "warn" } else { "info" }
        }));
    }
    if let Some(ins_m) = d.ins_mo {
        let q_m = d.quake_mo.unwrap_or(0.0);
        let annual_ins = (ins_m + q_m) * 12.0;
        kpis.push(json!({
            "label": tr(lang, "Annual Ins. Est.", "年保险估算", "Seguro Anual Est."),
            "value": format!("{}/yr", fmt_usd(annual_ins)),
            "delta": d.quake_mo.map(|qm| format!("Quake {}/mo", fmt_usd(qm))).unwrap_or_default(),
            "tone": if annual_ins > 5500.0 { "warn" } else { "info" }
        }));
    }
    if let Some(qs) = d.quake_safety {
        kpis.push(json!({
            "label": tr(lang, "Seismic Safety Score", "抗震安全评分", "Seguridad Sísmica"),
            "value": format!("{qs:.0}/100"),
            "delta": tr(lang, "100 - Risk Index", "100 - 风险指数", "100 - Riesgo"),
            "tone": if qs < 60.0 { "warn" } else { "good" }
        }));
    }
    if let Some(maint_m) = d.maint_mo {
        let annual_capex = maint_m * 12.0;
        kpis.push(json!({
            "label": tr(lang, "Annual Maint Reserve", "年度维护储备", "Reserva Anual Mant."),
            "value": format!("{}/yr", fmt_usd(annual_capex)),
            "delta": format!("{}/mo", fmt_usd(maint_m)),
            "tone": "info"
        }));
    }

    let mut insights = Vec::new();
    if let Some(yb) = d.year_built {
        let age = (2026 - yb).max(0);
        insights.push(tr(
            lang,
            &format!("Built in {yb} ({age} yrs old): verify foundation bolting, plumbing material, electrical panel capacity, and roof life in inspections."),
            &format!("该房建于 {yb} 年（房龄约{age}年）：验房应重点核实抗震地脚螺栓、供排水管、配电箱容量与屋顶剩余寿命。"),
            &format!("Construida en {yb} ({age} años): verificar anclaje sísmico, tuberías, panel eléctrico y vida útil del techo."),
        ));
    }
    if let (Some(fl), Some(fi)) = (d.flood_safety, d.fire_safety) {
        insights.push(tr(
            lang,
            &format!("Flood ({fl:.0}/100) and wildfire ({fi:.0}/100) safety scores reflect parcel hazard screening ({yr_prefix}profile)."),
            &format!("洪水安全（{fl:.0}/100）与野火安全（{fi:.0}/100）来自地块灾害实测筛查。"),
            &format!("La seguridad ante inundación ({fl:.0}/100) e incendio ({fi:.0}/100) proviene del análisis del lote."),
        ));
    }

    let sub_parts = [
        Some(d.address.clone()),
        d.ins_mo.map(|im| {
            format!(
                "{}/yr Ins",
                fmt_usd((im + d.quake_mo.unwrap_or(0.0)) * 12.0)
            )
        }),
        d.maint_mo
            .map(|mm| format!("{}/yr Maint", fmt_usd(mm * 12.0))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");

    json!({
        "id": artifact_id(&d.address, DeterministicRoute::RiskCapexSeismic, lang),
        "title": title,
        "subtitle": sub_parts,
        "chartType": "horizontal_bar",
        "unit": unit,
        "categories": cats,
        "series": [
            {
                "name": tr(lang, "This Property (100 - Safety)", "本房源风险指数", "Esta Propiedad"),
                "values": vals
            }
        ],
        "kpis": kpis,
        "tableHeaders": [
            tr(lang, "Risk Factor", "风险维度", "Factor"),
            tr(lang, "Risk Index", "风险指数", "Índice"),
            tr(lang, "Safety Score", "安全分", "Puntaje")
        ],
        "tableRows": rows,
        "insights": insights,
        "watchout": tr(
            lang,
            "Older electrical panels or unbolted foundations can affect insurability; obtain bindable quotes during contingency.",
            "老旧配电箱或未加固地基可能影响承保，务必在解除期限前取得正式保单报价。",
            "Paneles antiguos o cimientos sin anclaje afectan el seguro; obtén cotización vinculante antes de cerrar."
        ),
        "synthesisMode": "instant"
    })
}

fn build_comps_artifact(d: &ReportNumbers, lang: ArtifactLang) -> Value {
    let title = tr(
        lang,
        "Comps $/sqft vs Living & Lot Area",
        "可比成交单价($/sqft)与面积对比",
        "Comparables $/sqft vs Superficie y Lote",
    );
    let mut cats = Vec::new();
    let mut ppsf_vals = Vec::new();
    let mut rows = Vec::new();

    if d.per_sqft.is_some() || d.est_value.is_some() {
        let subj_lbl = tr(lang, "Subject Prop", "目标房源", "Propiedad");
        if let Some(ps) = d.per_sqft {
            cats.push(subj_lbl);
            ppsf_vals.push(round_money(ps));
        }
        rows.push(vec![
            tr(lang, "Subject Property", "目标房源", "Propiedad Objetivo"),
            d.est_value.map(fmt_usd).unwrap_or_else(|| "—".to_string()),
            d.sqft
                .map(|s| format!("{s:.0}"))
                .unwrap_or_else(|| "—".to_string()),
            d.lot_sqft
                .map(|l| format!("{l:.0}"))
                .unwrap_or_else(|| "—".to_string()),
            d.per_sqft
                .map(|p| format!("${p:.0}"))
                .unwrap_or_else(|| "—".to_string()),
        ]);
    }

    let mut comp_ppsfs = Vec::new();
    for (addr, ppsf, sqft, lot, price) in &d.comps {
        if let Some(ps) = ppsf {
            cats.push(addr.clone());
            ppsf_vals.push(round_money(*ps));
            comp_ppsfs.push(*ps);
        }
        rows.push(vec![
            addr.clone(),
            price.map(fmt_usd).unwrap_or_else(|| "—".to_string()),
            sqft.map(|s| format!("{s:.0}"))
                .unwrap_or_else(|| "—".to_string()),
            lot.map(|l| format!("{l:.0}"))
                .unwrap_or_else(|| "—".to_string()),
            ppsf.map(|p| format!("${p:.0}"))
                .unwrap_or_else(|| "—".to_string()),
        ]);
    }

    if comp_ppsfs.is_empty() {
        if let (Some(sf), Some(lo), Some(hi)) = (d.sqft, d.est_low, d.est_high) {
            cats.push(tr(lang, "Est. Low", "估值下沿", "Mínimo Est."));
            cats.push(tr(lang, "Est. High", "估值上沿", "Máximo Est."));
            ppsf_vals.push((lo / sf).round());
            ppsf_vals.push((hi / sf).round());
        }
    }

    if cats.is_empty() {
        return unavailable_artifact(
            d,
            DeterministicRoute::CompsPpsf,
            lang,
            title,
            "$/sqft".to_string(),
            tr(
                lang,
                "Verified comparable sales and $/sqft data are absent from this report.",
                "本报告缺少已核实可比成交及单价数据。",
                "Faltan ventas comparables verificadas y datos de $/sqft en este informe.",
            ),
        );
    }

    let comp_avg = if !comp_ppsfs.is_empty() {
        Some(comp_ppsfs.iter().sum::<f64>() / comp_ppsfs.len() as f64)
    } else {
        None
    };

    let mut series = vec![json!({
        "name": tr(lang, "Price per Sqft", "每平方英尺单价", "Precio por Pie²"),
        "values": ppsf_vals
    })];
    if let Some(avg) = comp_avg {
        series.push(json!({
            "name": tr(lang, "Comp Average $/sqft", "可比均值单价", "Promedio Comparables"),
            "values": vec![round_money(avg); cats.len()]
        }));
    }

    let mut kpis = Vec::new();
    if let Some(ps) = d.per_sqft {
        let delta = match comp_avg {
            Some(avg) if avg > 0.0 => format!("{:+.1}% vs comps", ((ps - avg) / avg) * 100.0),
            _ => String::new(),
        };
        kpis.push(json!({
            "label": tr(lang, "Subject $/sqft", "目标房源单价", "$/sqft Objetivo"),
            "value": format!("${ps:.0}"),
            "delta": delta,
            "tone": "info"
        }));
    }
    if let Some(avg) = comp_avg {
        kpis.push(json!({
            "label": tr(lang, "Comps Mean $/sqft", "周边可比均价", "Media Comparables"),
            "value": format!("${avg:.0}"),
            "delta": format!("{} comps", comp_ppsfs.len()),
            "tone": "info"
        }));
    }
    if let Some(sf) = d.sqft {
        kpis.push(json!({
            "label": tr(lang, "Living / Lot Sqft", "室内 / 占地面积", "Área / Terreno"),
            "value": format!("{sf:.0} sqft"),
            "delta": d.lot_sqft.map(|l| format!("Lot {l:.0} sqft")).unwrap_or_default(),
            "tone": "info"
        }));
    }
    if let Some(ev) = d.est_value {
        let band = match (d.est_low, d.est_high) {
            (Some(lo), Some(hi)) => format!("{}–{}", fmt_k_or_m(lo), fmt_k_or_m(hi)),
            _ => String::new(),
        };
        kpis.push(json!({
            "label": tr(lang, "Valuation", "估值参考", "Valuación"),
            "value": fmt_k_or_m(ev),
            "delta": band,
            "tone": "good"
        }));
    }

    let mut insights = Vec::new();
    if let (Some(ps), Some(avg)) = (d.per_sqft, comp_avg) {
        let spread_pct = if avg > 0.0 {
            ((ps - avg) / avg) * 100.0
        } else {
            0.0
        };
        insights.push(tr(
            lang,
            &format!("Subject prices around ${ps:.0}/sqft vs comp average ${avg:.0}/sqft ({spread_pct:+.1}% spread)."),
            &format!("本房源折合单价约 ${ps:.0}/sqft，周边可比均价 ${avg:.0}/sqft（偏离 {spread_pct:+.1}%）。"),
            &format!("La propiedad cotiza a ${ps:.0}/sqft frente a un promedio comparable de ${avg:.0}/sqft ({spread_pct:+.1}%)."),
        ));
    }
    insights.push(tr(
        lang,
        if !d.comps.is_empty() {
            "Compare bed/bath count and renovation finish across the listed comps before bidding above the high end."
        } else {
            "Comp sample is sparse in this record; request an updated CMA from your agent prior to waiving appraisal contingency."
        },
        if !d.comps.is_empty() {
            "出价高于估值上限前，请逐一对比上表可比成交的装修成色与厨卫翻新年份。"
        } else {
            "当前记录可比成交较少，放弃估值保护条款前务必补齐最新CMA成交报告。"
        },
        if !d.comps.is_empty() {
            "Compara el estado de remodelación de cada comparable antes de ofertar por encima de la banda alta."
        } else {
            "La muestra de comparables es limitada; solicita un CMA actualizado antes de renunciar a tasación."
        },
    ));

    let sub_parts = [
        Some(d.address.clone()),
        d.per_sqft.map(|ps| format!("Subject ${ps:.0}/sqft")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");

    json!({
        "id": artifact_id(&d.address, DeterministicRoute::CompsPpsf, lang),
        "title": title,
        "subtitle": sub_parts,
        "chartType": "bar",
        "unit": "$/sqft",
        "categories": cats,
        "series": series,
        "kpis": kpis,
        "tableHeaders": [
            tr(lang, "Property", "房源地址", "Propiedad"),
            tr(lang, "Price / Est", "总价", "Precio"),
            tr(lang, "Living Sqft", "室内面积", "Área (sqft)"),
            tr(lang, "Lot Sqft", "占地面积", "Lote (sqft)"),
            tr(lang, "$/Sqft", "单价", "$/sqft")
        ],
        "tableRows": rows,
        "insights": insights,
        "watchout": tr(
            lang,
            "Verify whether assessor vs MLS living area reflects unpermitted additions before paying $/sqft premiums.",
            "重点核查室内面积与县估值记录是否一致，排除未报建加建面积造成的单价误差。",
            "Confirma si el área habitable coincide con registros del condado."
        ),
        "synthesisMode": "instant"
    })
}

fn build_cost_donut_artifact(d: &ReportNumbers, lang: ArtifactLang) -> Value {
    let title = tr(
        lang,
        "Monthly Ownership Cost Breakdown (20% Down)",
        "月度持有成本构成（20%首付口径）",
        "Desglose Mensual de Propiedad (20% Enganche)",
    );
    let unit = tr(lang, "$/mo", "美元/月", "$/mes");
    let mut items: Vec<(String, f64)> = Vec::new();
    let mut pi_opt = None;
    if let (Some(ev), Some(r30)) = (d.est_value, d.rate30) {
        let pi = round_money(calc_monthly_pi(ev * 0.80, r30, 30));
        pi_opt = Some(pi);
        items.push((
            tr(
                lang,
                "Principal & Interest",
                "按揭本息 P&I",
                "Capital e Interés",
            ),
            pi,
        ));
    }
    if let Some(tax) = d.prop_tax_mo.filter(|v| *v > 0.0) {
        items.push((
            tr(lang, "Property Tax", "房产税", "Impuesto Predial"),
            round_money(tax),
        ));
    }
    if let Some(ins) = d.ins_mo.filter(|v| *v > 0.0) {
        items.push((
            tr(lang, "Home Insurance", "常规房屋保险", "Seguro Hogar"),
            round_money(ins),
        ));
    }
    if let Some(quake) = d.quake_mo.filter(|v| *v > 0.0) {
        items.push((
            tr(lang, "Earthquake Insurance", "地震险", "Seguro Sísmico"),
            round_money(quake),
        ));
    }
    if let Some(maint) = d.maint_mo.filter(|v| *v > 0.0) {
        items.push((
            tr(lang, "Maintenance Reserve", "维护修缮储备", "Mantenimiento"),
            round_money(maint),
        ));
    }
    if let Some(hoa) = d.hoa_mo.filter(|v| *v > 0.0) {
        items.push((
            tr(lang, "HOA Dues", "物业管理费 HOA", "Cuota HOA"),
            round_money(hoa),
        ));
    }

    let total: f64 = items.iter().map(|(_, v)| *v).sum();
    if items.len() < 2 || total <= 0.0 {
        return unavailable_artifact(
            d,
            DeterministicRoute::CostBreakdownDonut,
            lang,
            title,
            unit,
            tr(
                lang,
                "Monthly ownership cost components are absent from this report.",
                "本报告缺少月度持有成本分项数据。",
                "Faltan componentes de costo mensual en este informe.",
            ),
        );
    }

    let cats: Vec<String> = items.iter().map(|(c, _)| c.clone()).collect();
    let vals: Vec<f64> = items.iter().map(|(_, v)| *v).collect();
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|(c, v)| {
            vec![
                c.clone(),
                fmt_usd(*v),
                fmt_usd(*v * 12.0),
                format!("{:.1}%", (*v / total) * 100.0),
            ]
        })
        .collect();

    let mut kpis = vec![json!({
        "label": tr(lang, "Total Monthly Carry", "总月度持有成本", "Costo Total Mensual"),
        "value": fmt_usd(total),
        "delta": d.rate30.map(|r| format!("Rate {r:.2}%")).unwrap_or_default(),
        "tone": "info"
    })];
    if let Some(pi) = pi_opt {
        kpis.push(json!({
            "label": tr(lang, "P&I Share", "按揭本息占比", "Peso Hipoteca"),
            "value": format!("{:.0}%", (pi / total) * 100.0),
            "delta": fmt_usd(pi),
            "tone": "info"
        }));
    }
    if let Some(rm) = d.rent_est {
        kpis.push(json!({
            "label": tr(lang, "Est. Market Rent", "同屋市场估值月租", "Renta de Mercado"),
            "value": fmt_usd(rm),
            "delta": format!("Gap {}", fmt_usd(total - rm)),
            "tone": if rm >= total * 0.7 { "good" } else { "warn" }
        }));
    }
    if let Some(tr_pct) = d.tax_rate_pct {
        kpis.push(json!({
            "label": tr(lang, "Effective Tax Rate", "重估后税率参考", "Tasa Predial"),
            "value": format!("{tr_pct:.2}%"),
            "delta": d.prop_tax_mo.map(fmt_usd).unwrap_or_default(),
            "tone": "info"
        }));
    }

    let non_pi = total - pi_opt.unwrap_or(0.0);
    let insights = vec![tr(
        lang,
        &format!(
            "Non-mortgage ownership overhead totals {}/mo permanently even after loan payoff.",
            fmt_usd(non_pi)
        ),
        &format!(
            "即使未来全款无贷，刚性房产税、保险与维护仍需约 {}/月。",
            fmt_usd(non_pi)
        ),
        &format!(
            "Incluso sin hipoteca, los gastos fijos suman {}/mes.",
            fmt_usd(non_pi)
        ),
    )];

    json!({
        "id": artifact_id(&d.address, DeterministicRoute::CostBreakdownDonut, lang),
        "title": title,
        "subtitle": format!("{} · Total {}/mo", d.address, fmt_usd(total)),
        "chartType": "donut",
        "unit": unit,
        "categories": cats,
        "series": [
            { "name": tr(lang, "Monthly Share", "月度构成", "Costo Mensual"), "values": vals }
        ],
        "kpis": kpis,
        "tableHeaders": [
            tr(lang, "Component", "费用科目", "Concepto"),
            tr(lang, "Monthly ($)", "月均 ($)", "Mensual ($)"),
            tr(lang, "Annual ($)", "年均 ($)", "Anual ($)"),
            tr(lang, "Share (%)", "占比 (%)", "Porcentaje")
        ],
        "tableRows": rows,
        "insights": insights,
        "watchout": tr(
            lang,
            "Confirm reassessment supplemental tax bills and parcel-specific direct charges in escrow.",
            "注意过户后重估补充税单及市政专项附加费。",
            "Confirma cargos suplementarios de impuesto predial tras el cierre."
        ),
        "synthesisMode": "instant"
    })
}

fn build_scorecard_artifact(d: &ReportNumbers, lang: ArtifactLang) -> Value {
    let title = tr(
        lang,
        "Multi-Pillar Property & Livability Radar",
        "全维度房产综合素质与学区配套雷达",
        "Radar Integral de Propiedad y Entorno",
    );
    let unit = tr(lang, "Score (0-100)", "评分 (0-100)", "Puntaje (0-100)");

    let candidates = [
        (
            tr(lang, "Investment", "投资性价比", "Inversión"),
            d.inv_score,
        ),
        (
            tr(lang, "Natural Safety", "灾害安全度", "Seguridad"),
            d.safety_overall,
        ),
        (
            tr(lang, "Environment", "环境洁净度", "Ambiente"),
            d.env_overall,
        ),
        (
            tr(lang, "Livability", "居住便利度", "Habitabilidad"),
            d.livability_overall,
        ),
        (
            tr(lang, "Walkability", "步行指数", "Caminabilidad"),
            d.walk_score,
        ),
        (
            tr(lang, "General Overall", "综合总评", "General"),
            d.overall_score,
        ),
    ];

    let mut cats = Vec::new();
    let mut scores = Vec::new();
    let mut rows = Vec::new();
    for (lbl, sc_opt) in candidates {
        if let Some(sc) = sc_opt {
            let r = round1(sc);
            rows.push(vec![lbl.clone(), format!("{r:.0}/100")]);
            cats.push(lbl);
            scores.push(r);
        }
    }

    if cats.len() < 3 {
        return unavailable_artifact(
            d,
            DeterministicRoute::ScorecardLivability,
            lang,
            title,
            unit,
            tr(
                lang,
                "Multi-pillar property scores are absent from this report.",
                "本报告缺少多维房产评分数据。",
                "Faltan puntajes multidimensionales en este informe.",
            ),
        );
    }

    let mut kpis = Vec::new();
    if let Some(ov) = d.overall_score {
        kpis.push(json!({
            "label": tr(lang, "Overall Score", "综合评分", "Puntaje Global"),
            "value": format!("{ov:.0}/100"),
            "delta": d.est_value.map(fmt_usd).unwrap_or_default(),
            "tone": if ov >= 70.0 { "good" } else { "info" }
        }));
    }
    if d.walk_score.is_some() || d.livability_overall.is_some() {
        let wl_val = match (d.walk_score, d.livability_overall) {
            (Some(w), Some(l)) => format!("{w:.0} / {l:.0}"),
            (Some(w), None) => format!("Walk {w:.0}"),
            (None, Some(l)) => format!("{l:.0}/100"),
            (None, None) => String::new(),
        };
        kpis.push(json!({
            "label": tr(lang, "Walk & Livability", "步行 / 便利度", "Caminabilidad"),
            "value": wl_val,
            "delta": format!("{} schools", d.schools_count),
            "tone": "info"
        }));
    }
    if let Some(sf) = d.safety_overall {
        kpis.push(json!({
            "label": tr(lang, "Natural Safety", "灾害安全", "Seguridad"),
            "value": format!("{sf:.0}/100"),
            "delta": match (d.flood_safety, d.quake_safety) {
                (Some(f), Some(q)) => format!("Flood {f:.0} · Quake {q:.0}"),
                _ => String::new(),
            },
            "tone": if sf >= 68.0 { "good" } else { "warn" }
        }));
    }
    if let Some(lot) = d.lot_sqft {
        kpis.push(json!({
            "label": tr(lang, "Reconciled Lot", "核准占地面积", "Lote Conciliado"),
            "value": format!("{lot:.0} sqft"),
            "delta": d.year_built.map(|y| format!("Built {y}")).unwrap_or_default(),
            "tone": "info"
        }));
    }

    let mut sub_parts = vec![d.address.clone()];
    if d.schools_count > 0 {
        if let Some(ref ts) = d.top_school {
            sub_parts.push(format!("{} Schools Nearby ({ts})", d.schools_count));
        } else {
            sub_parts.push(format!("{} Schools Nearby", d.schools_count));
        }
    }
    if let Some(lot) = d.lot_sqft {
        sub_parts.push(format!("Lot {lot:.0} sqft"));
    }

    let max_sc = scores.iter().cloned().fold(0.0_f64, f64::max);
    let min_sc = scores.iter().cloned().fold(100.0_f64, f64::min);
    let insights = vec![tr(
        lang,
        &format!(
            "Strongest pillar scores {max_sc:.0}/100 while lowest score sits at {min_sc:.0}/100 across {} verified report dimensions.",
            scores.len()
        ),
        &format!(
            "已核验维度中最高分 {max_sc:.0}/100、最低分 {min_sc:.0}/100，建议结合披露文件重点复核薄弱项。"
        ),
        &format!(
            "El pilar más alto marca {max_sc:.0}/100 y el menor {min_sc:.0}/100; enfoca la diligencia en el punto más bajo."
        ),
    )];

    json!({
        "id": artifact_id(&d.address, DeterministicRoute::ScorecardLivability, lang),
        "title": title,
        "subtitle": sub_parts.join(" · "),
        "chartType": "radar",
        "unit": unit,
        "categories": cats,
        "series": [
            { "name": tr(lang, "Subject Property", "目标房源", "Propiedad"), "values": scores }
        ],
        "kpis": kpis,
        "tableHeaders": [
            tr(lang, "Pillar", "评估维度", "Pilar"),
            tr(lang, "Score", "报告分值", "Puntaje")
        ],
        "tableRows": rows,
        "insights": insights,
        "watchout": tr(
            lang,
            "Verify exact assigned school boundaries directly with the school district rather than proximity alone.",
            "学区归属请务必向学区办公室核实精确街道划片边界，切勿仅凭直线距离判断。",
            "Confirma la zonificación escolar oficial directamente con el distrito antes de ofertar."
        ),
        "synthesisMode": "instant"
    })
}

/// Deterministic `<1ms` router that maps any buyer visualization prompt + language into a complete,
/// chart-ready artifact grounded in `report` JSON.
pub fn build_deterministic_artifact(report: &Value, prompt: &str, lang: &str) -> Value {
    let resolved_lang = detect_lang(
        if lang.trim().is_empty() {
            None
        } else {
            Some(lang)
        },
        prompt,
    );
    let numbers = extract_report_numbers(report);
    let route = classify_prompt(prompt);

    let mut artifact = match route {
        DeterministicRoute::FinancingAdu => build_financing_adu_artifact(&numbers, resolved_lang),
        DeterministicRoute::EquityVsRent => build_equity_vs_rent_artifact(&numbers, resolved_lang),
        DeterministicRoute::RiskCapexSeismic => build_risk_capex_artifact(&numbers, resolved_lang),
        DeterministicRoute::CompsPpsf => build_comps_artifact(&numbers, resolved_lang),
        DeterministicRoute::CostBreakdownDonut => {
            build_cost_donut_artifact(&numbers, resolved_lang)
        }
        DeterministicRoute::ScorecardLivability => {
            build_scorecard_artifact(&numbers, resolved_lang)
        }
    };
    scrub_artifact_value(&mut artifact, resolved_lang.code());
    artifact
}

fn build_artifact_report_digest(report: &Value) -> Value {
    let n = extract_report_numbers(report);
    let comps_digest: Vec<Value> = n
        .comps
        .iter()
        .take(5)
        .map(|(a, ppsf, sf, lot, price)| {
            json!({
                "address": a,
                "pricePerSqft": ppsf,
                "sqft": sf,
                "lotSqft": lot,
                "price": price,
            })
        })
        .collect();

    json!({
        "address": n.address,
        "city": n.city,
        "state": n.state,
        "sqft": n.sqft,
        "lotSqft": n.lot_sqft,
        "yearBuilt": n.year_built,
        "valuation": {
            "estimate": n.est_value,
            "low": n.est_low,
            "high": n.est_high,
            "thirdPartyEstimate": n.zestimate,
            "perSqft": n.per_sqft,
        },
        "ownershipMonthly": {
            "rate30": n.rate30,
            "taxRatePct": n.tax_rate_pct,
            "propertyTax": n.prop_tax_mo,
            "homeInsurance": n.ins_mo,
            "earthquakeInsurance": n.quake_mo,
            "maintenance": n.maint_mo,
            "hoa": n.hoa_mo,
            "rentEstimate": n.rent_est,
        },
        "adu": {
            "estRentMonthly": n.adu_rent_mo,
            "feasibility": n.adu_feasibility,
        },
        "horizons": {
            "yr5": { "p10": n.p10_5, "p50": n.p50_5, "p90": n.p90_5, "lossProbPct": n.loss_prob_5y },
            "yr10": { "p10": n.p10_10, "p50": n.p50_10, "p90": n.p90_10 },
        },
        "scores": {
            "overall": n.overall_score,
            "investment": n.inv_score,
            "safety": n.safety_overall,
            "environment": n.env_overall,
            "livability": n.livability_overall,
            "walkability": n.walk_score,
            "floodSafety": n.flood_safety,
            "quakeSafety": n.quake_safety,
            "wildfireSafety": n.fire_safety,
            "envContamination": n.env_safety,
            "noise": n.noise_safety,
            "air": n.air_safety,
            "water": n.water_safety,
        },
        "schoolsCount": n.schools_count,
        "topSchool": n.top_school,
        "comps": comps_digest,
    })
}

fn validate_llm_artifact_candidate(v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let chart_type = v.get("chartType").and_then(|x| x.as_str()).unwrap_or("");
    if !matches!(
        chart_type,
        "bar" | "line" | "radar" | "horizontal_bar" | "donut"
    ) {
        errs.push("invalid chartType".to_string());
    }
    let cats_len = v
        .get("categories")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if !(2..=14).contains(&cats_len) {
        errs.push("categories must have 2..=14 labels".to_string());
    }
    let series = v.get("series").and_then(|x| x.as_array());
    if series.map(|s| s.is_empty() || s.len() > 6).unwrap_or(true) {
        errs.push("series must contain 1..=6 series".to_string());
    } else if let Some(s_arr) = series {
        for (i, s) in s_arr.iter().enumerate() {
            let vals = s.get("values").and_then(|x| x.as_array());
            if vals.map(|a| a.len() != cats_len).unwrap_or(true) {
                errs.push(format!("series[{i}].values length mismatch"));
            } else if let Some(a) = vals {
                if !a
                    .iter()
                    .all(|num| num.as_f64().map(f64::is_finite).unwrap_or(false))
                {
                    errs.push(format!("series[{i}].values must all be finite numbers"));
                }
            }
        }
    }
    let headers_len = v
        .get("tableHeaders")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if !(2..=6).contains(&headers_len) {
        errs.push("tableHeaders must have 2..=6 columns".to_string());
    }
    if let Some(rows) = v.get("tableRows").and_then(|x| x.as_array()) {
        if rows.is_empty() || rows.len() > 14 {
            errs.push("tableRows must have 1..=14 rows".to_string());
        }
        for (r_idx, r) in rows.iter().enumerate() {
            let row_len = r.as_array().map(|a| a.len()).unwrap_or(0);
            if row_len != headers_len {
                errs.push(format!("tableRows[{r_idx}] column count mismatch"));
            }
        }
    } else {
        errs.push("missing tableRows".to_string());
    }
    if let Ok(raw) = serde_json::to_string(v) {
        if let Some(reason) = inspect_output_guardrail(&raw) {
            errs.push(format!("output guardrail rejected: {reason}"));
        }
    }
    errs
}

/// Default role, task, and JSON/charting rules used by [`generate_llm_artifact_with_prompt`].
pub const DEFAULT_ARTIFACT_INSTRUCTIONS: &str = r#"ROLE: Real-estate quantitative chart & table architect for a US home-buyer command center.
TASK: Turn the buyer's question into one compact, accurate, interactive chart + KPI strip + structured table grounded ONLY in `<grounded_property_facts>`.
RULES:
1. Pick the single clearest `chartType`: `bar` (scenario comparison), `line` (multi-year trend), `horizontal_bar` (ranked breakdown), `radar` (multi-pillar 0-100 profile), or `donut` (share of total).
2. Keep ONE consistent numeric `unit` across all `series[i].values`! Every `series[i].values` array MUST match `categories.len()` exactly.
3. Never fabricate external comps, sales, rates, scores, or facts missing or `null` in `<grounded_property_facts>`; omit missing dimensions rather than inventing numbers, and perform exact financial arithmetic only from non-null supplied numbers.
4. Provide 3-4 concise `kpis`, a crisp `tableHeaders`/`tableRows` breakdown (2-6 columns, 3-8 rows), 2-3 `insights`, and 1 practical `watchout`.
5. Set `synthesisMode` to `"ai_enhanced"`.
6. Fair Housing strict compliance: zero demographic, racial, religious, or family-status characterizations."#;

/// Generate a live AI-enhanced interactive visualization artifact grounded in `report` JSON,
/// seamlessly falling back (<1ms) to `build_deterministic_artifact` when offline, unconfigured, or timed out.
pub async fn generate_llm_artifact_with_client(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    prompt: &str,
    lang: &str,
    cache: Option<&dyn LlmCacheBackend>,
) -> Value {
    generate_llm_artifact_with_prompt(http, env, report, prompt, lang, cache, None).await
}

/// Variant of [`generate_llm_artifact_with_client`] allowing an optional admin-tuned system prompt
/// override while keeping `IMMUTABLE_SYSTEM_PREAMBLE` and the output language directive pinned.
pub async fn generate_llm_artifact_with_prompt(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    prompt: &str,
    lang: &str,
    cache: Option<&dyn LlmCacheBackend>,
    custom_instructions: Option<&str>,
) -> Value {
    let resolved_lang = detect_lang(
        if lang.trim().is_empty() {
            None
        } else {
            Some(lang)
        },
        prompt,
    );
    let mut fallback = build_deterministic_artifact(report, prompt, resolved_lang.code());

    let digest = build_artifact_report_digest(report);
    let lang_directive = match resolved_lang {
        ArtifactLang::En => "Write ALL titles, labels, series names, table headers/cells, insights, and watchout in clear English.",
        ArtifactLang::Zh => "使用简体中文输出所有标题、图表分类、系列名称、表头、表格单元格、洞察与风险提示（数字与$符号保持清晰可读）。",
        ArtifactLang::Es => "Escribe TODOS los títulos, etiquetas, series, encabezados de tabla, celdas, hallazgos y advertencia en español claro.",
    };

    let base_instructions = custom_instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_ARTIFACT_INSTRUCTIONS);
    let branded_instructions = crate::shared::apply_brand_to_str(
        base_instructions,
        &env.brand_name(),
        &env.brand_domain(),
    );
    let instructions =
        format!("{IMMUTABLE_SYSTEM_PREAMBLE}\n\n{branded_instructions}\n\n7. {lang_directive}");

    let digest_json = serde_json::to_string(&digest).unwrap_or_else(|_| "{}".to_string());
    let user_input = format!(
        "<untrusted_buyer_question>\n{prompt}\n</untrusted_buyer_question>\n\n<grounded_property_facts>\n{digest_json}\n</grounded_property_facts>"
    );

    let mut opts = LlmCallOptions::new(
        "property_qa",
        "openestate_artifact_canvas",
        instructions,
        user_input,
        artifact_canvas_schema(),
    )
    .with_task("artifact_canvas")
    .with_profile("writing_light");
    if let Some(artifact_model) = env
        .get("LLM_ARTIFACT_MODEL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if crate::metering::MeterScope::active_llm_spec().is_none()
            && crate::metering::active_extension_id().is_none()
        {
            opts.models = vec![artifact_model.to_string()];
        }
    } else if ["SLIDES_LLM_MODEL", "SLIDES_LLM_EFFORT"]
        .iter()
        .any(|k| env.get(k).map(str::trim).filter(|s| !s.is_empty()).is_some())
        && crate::metering::MeterScope::active_llm_spec().is_none()
        && crate::metering::active_extension_id().is_none()
    {
        opts.models = resolve_interactive_model_specs(env);
    }
    opts.timeout_ms = Some(10_500);
    opts.max_output_tokens = Some(1_600);
    opts.validator = Some(Arc::new(validate_llm_artifact_candidate));

    match tokio::time::timeout(
        Duration::from_millis(ARTIFACT_LLM_TIMEOUT_MS),
        responses_json_with_cache(http, env, opts, cache),
    )
    .await
    {
        Ok(Ok(res)) if validate_llm_artifact_candidate(&res.data).is_empty() => {
            let mut enhanced = res.data;
            if let Some(obj) = enhanced.as_object_mut() {
                obj.insert("synthesisMode".to_string(), json!("ai_enhanced"));
                if obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    if let Some(fid) = fallback.get("id").cloned() {
                        obj.insert("id".to_string(), fid);
                    }
                }
            }
            scrub_artifact_value(&mut enhanced, resolved_lang.code());
            enhanced
        }
        _ => {
            scrub_artifact_value(&mut fallback, resolved_lang.code());
            fallback
        }
    }
}
