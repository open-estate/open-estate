// Grounded Dynamic LLM Property Analyst (`LLM_MODEL` -> `LLM_FALLBACK_MODEL`) + Deterministic Fallback.
// Produces executive verdict, 10 per-section analyst notes, 3x3 risk matrix, negotiation levers,
// seller questions, fit/tradeoffs, 1-yr/5-yr outlook, and bilingual EN + 简体中文 feng shui synthesis.

use crate::ai::brief::build_brief;
use crate::ai::llm::{
    responses_json_with_cache, LlmCache, LlmCallOptions,
};
use crate::shared::{
    compass_word, localize_compass_in_cjk, localize_compass_in_cjk_value, now_iso,
    sitting_facing_zh, EnvConfig, HttpClient, ModuleOutput, SourceItem,
};
use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub const ANALYST_INSTRUCTIONS: &str = r#"You are Open Estate Senior Real Estate Analyst — a rigorous, quantitative US residential due-diligence analyst (combining the eye of a valuation strategist, environmental/geotechnical reviewer, and classical 风水 consultant for the fengShui section).

STRICT RULES:
1. GROUNDED FACTS ONLY: Every number, dollar figure, percentage, date, school name, fault name, cleanup site, road, and distance MUST come directly from the JSON property brief provided by the user. Never invent comps, permits, HOA rules, interior finishes, or inspection findings. When public records have a conflict or gap (e.g. living-area discrepancy, unverified permit, unmapped hazard zone), explicitly call it out as something to verify.
2. CONCISE & DENSE: Write crisp, institutional-grade prose — zero fluff, zero generic real-estate clichés ("dream home", "nestled in", "boasts"). Prefer specific numbers ("0.18 mi from nearest arterial", "P10–P90 5-yr range $1.89M–$2.96M", "PGA 0.60g").
3. VALUATION HONESTY: Frame value estimates as statistical/comp blends, never as a licensed appraisal or guaranteed future price.
4. FAIR HOUSING COMPLIANCE (MANDATORY): Evaluate ONLY the physical property, financial math, environmental/hazard data, infrastructure, and amenities. NEVER mention or imply race, ethnicity, national origin, religion/churches/temples, sex/gender, disability, or familial status (do NOT say "great for families with kids", "empty nesters", "young couples", or "safe/unsafe neighborhood" — refer to objective metrics like "walkable to elementary/high school", "single-level vs split-level stairs", or "city-wide FBI agency crime rate"). In feng shui, never tie palace stars to specific gendered household members ("eldest son", "matriarch"). Apply the exact same Fair Housing standard across English, Simplified Chinese (`translations.zh`), and Spanish (`translations.es`).
5. FENG SHUI (CHINESE-ONLY SYNTHESIS): Frame strictly as traditional cultural analysis (峦头 form school + 玄空飞星 Xuan Kong Flying Stars + 八宅 Ba Zhai). Synthesize `fengShui.zh` strictly in authentic Simplified Chinese ONLY, and set `fengShui.en` to `""` (empty string — no English paragraph). In `fengShui.zh` write every compass direction in Chinese characters — state the orientation with the brief's `fengshui.orientation.sittingFacingZh` (e.g. 坐北朝南) and use 东/南/西/北/东南/东北/西南/西北 for sectors; never splice Latin abbreviations into Chinese (坐N朝S, 朝S and SE方 are wrong).
6. NATIVE MULTILINGUAL DISCIPLINE:
   - Write ALL top-level non-fengShui fields (`headline`, `summary`, `aboutHome`, `paragraphs`, `sectionNotes`, `upside`, `downside`, `verify`, `outlook`, `riskMatrix`, `fit`, `negotiation`, `sellerQuestions`) strictly in English (brief parenthetical Chinese terms in `sectionNotes.fengshui` are permitted).
   - Write ALL fields inside `translations.zh` natively in idiomatic Simplified Chinese suitable for Chinese-speaking US homebuyers (keep proper US street names, school names, dollar figures, distances, and acronyms like ADU/FEMA/SFHA/CAASPP accurate; spell compass directions in Chinese characters like 坐北朝南 and 东/南/西/北, never 坐N朝S).
   - Write ALL fields inside `translations.es` natively in idiomatic US real-estate Spanish (clear buyer-friendly terminology; keep proper US street/school names, dollar figures, distances, and acronyms accurate; write compass directions in Spanish like norte/sur/este/oeste/noreste/noroeste/sureste/suroeste).
7. NO COMPETITOR PORTALS OR TRADEMARKS (MANDATORY IN EN, 中文 & ES):
   - NEVER name other real-estate portals, brokerages, or their trademarks: Zillow, Zestimate, ZHVI, ZORI, Zillow 3D Home, Redfin, Redfin Estimate, Walk Score, Realtor.com, Homes.com, Trulia, Movoto, Compass (the brokerage — lowercase compass directions/bearings are fine), Opendoor, LoopNet, Crexi, HouseCanary, or similar. Public/open agencies (FEMA, USGS, EPA, Census, NCES, FBI, NASA, OpenStreetMap, HUD, FHFA, Freddie Mac) and tour hosts Matterport/iGUIDE are permitted.
   - Use ONLY this exact shared vocabulary:
     * Value Open Estate computes -> EN: "True Estimate" (or "Open Estate estimate") | 中文: "真实估值" | ES: "Estimación Real"
     * Another company's automated value (never call a third-party figure Open Estate's "True Estimate") -> EN: "Third-party estimate" | 中文: "第三方估值" | ES: "Estimación de terceros"
     * ZIP home-value index (instead of ZHVI) -> EN: "ZIP home-value index" | 中文: "邮编房价指数" | ES: "Índice de valor del código postal"
     * ZIP rent index (instead of ZORI) -> EN: "ZIP rent index" | 中文: "邮编租金指数" | ES: "Índice de renta del código postal"
     * Walkability (instead of Walk Score) -> EN: "Walkability score" (or "walkability score" mid-sentence) | 中文: "步行便利指数" | ES: "Índice de caminabilidad""#;

fn localized_narrative_schema() -> Value {
    let sec_props = json!({
        "value": { "type": "string" },
        "history": { "type": "string" },
        "comps": { "type": "string" },
        "market": { "type": "string" },
        "cost": { "type": "string" },
        "hazards": { "type": "string" },
        "environment": { "type": "string" },
        "schools": { "type": "string" },
        "neighborhood": { "type": "string" },
        "fengshui": { "type": "string" }
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "headline",
            "summary",
            "aboutHome",
            "paragraphs",
            "upside",
            "downside",
            "verify",
            "sectionNotes",
            "outlook",
            "riskMatrix",
            "fit",
            "negotiation",
            "sellerQuestions"
        ],
        "properties": {
            "headline": { "type": "string" },
            "summary": { "type": "string" },
            "aboutHome": { "type": "string" },
            "paragraphs": { "type": "array", "items": { "type": "string" } },
            "upside": { "type": "array", "items": { "type": "string" } },
            "downside": { "type": "array", "items": { "type": "string" } },
            "verify": { "type": "array", "items": { "type": "string" } },
            "sectionNotes": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "value", "history", "comps", "market", "cost",
                    "hazards", "environment", "schools", "neighborhood", "fengshui"
                ],
                "properties": sec_props
            },
            "outlook": {
                "type": "object",
                "additionalProperties": false,
                "required": ["oneYear", "fiveYear"],
                "properties": {
                    "oneYear": { "type": "string" },
                    "fiveYear": { "type": "string" }
                }
            },
            "riskMatrix": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["risk", "likelihood", "impact", "mitigation"],
                    "properties": {
                        "risk": { "type": "string" },
                        "likelihood": { "type": "string", "enum": ["low", "medium", "high"] },
                        "impact": { "type": "string", "enum": ["low", "medium", "high"] },
                        "mitigation": { "type": "string" }
                    }
                }
            },
            "fit": {
                "type": "object",
                "additionalProperties": false,
                "required": ["prioritize", "tradeoffs"],
                "properties": {
                    "prioritize": { "type": "array", "items": { "type": "string" } },
                    "tradeoffs": { "type": "array", "items": { "type": "string" } }
                }
            },
            "negotiation": { "type": "array", "items": { "type": "string" } },
            "sellerQuestions": { "type": "array", "items": { "type": "string" } }
        }
    })
}

pub fn analysis_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "headline",
            "summary",
            "aboutHome",
            "paragraphs",
            "upside",
            "downside",
            "verify",
            "sectionNotes",
            "outlook",
            "riskMatrix",
            "fit",
            "negotiation",
            "sellerQuestions",
            "fengShui",
            "translations"
        ],
        "properties": {
            "headline": { "type": "string", "description": "Crisp 1-sentence executive verdict (<170 chars)." },
            "summary": { "type": "string", "description": "2–3 sentences synthesizing estimated value vs last sale/comps, location perks, and watchouts." },
            "aboutHome": { "type": "string", "description": "1 rich factual paragraph (80–130 words) describing the physical house, lot, layout/stories, renovations, systems, and immediate setting." },
            "paragraphs": { "type": "array", "items": { "type": "string" }, "description": "2–3 analytical paragraphs covering valuation/market, hazards/environment, and livability/schools/ownership math." },
            "upside": { "type": "array", "items": { "type": "string" }, "description": "5–6 concrete upside bullets citing exact figures, distances, or scores." },
            "downside": { "type": "array", "items": { "type": "string" }, "description": "5–6 concrete downside/risk bullets citing exact figures, distances, or scores." },
            "verify": { "type": "array", "items": { "type": "string" }, "description": "5–6 concrete due-diligence items to verify before removing contingencies." },
            "sectionNotes": {
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "value",
                    "history",
                    "comps",
                    "market",
                    "cost",
                    "hazards",
                    "environment",
                    "schools",
                    "neighborhood",
                    "fengshui"
                ],
                "properties": {
                    "value": { "type": "string" },
                    "history": { "type": "string" },
                    "comps": { "type": "string" },
                    "market": { "type": "string" },
                    "cost": { "type": "string" },
                    "hazards": { "type": "string" },
                    "environment": { "type": "string" },
                    "schools": { "type": "string" },
                    "neighborhood": { "type": "string" },
                    "fengshui": { "type": "string" }
                }
            },
            "outlook": {
                "type": "object",
                "additionalProperties": false,
                "required": ["oneYear", "fiveYear"],
                "properties": {
                    "oneYear": { "type": "string" },
                    "fiveYear": { "type": "string" }
                }
            },
            "riskMatrix": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["risk", "likelihood", "impact", "mitigation"],
                    "properties": {
                        "risk": { "type": "string" },
                        "likelihood": { "type": "string", "enum": ["low", "medium", "high"] },
                        "impact": { "type": "string", "enum": ["low", "medium", "high"] },
                        "mitigation": { "type": "string" }
                    }
                }
            },
            "fit": {
                "type": "object",
                "additionalProperties": false,
                "required": ["prioritize", "tradeoffs"],
                "properties": {
                    "prioritize": { "type": "array", "items": { "type": "string" } },
                    "tradeoffs": { "type": "array", "items": { "type": "string" } }
                }
            },
            "negotiation": { "type": "array", "items": { "type": "string" } },
            "sellerQuestions": { "type": "array", "items": { "type": "string" } },
            "fengShui": {
                "type": "object",
                "additionalProperties": false,
                "required": ["en", "zh"],
                "properties": {
                    "en": { "type": "string", "description": "Leave empty string \"\" — Feng Shui output is strictly Chinese-only." },
                    "zh": { "type": "string", "description": "Authentic Simplified Chinese 堪舆风水 synthesis ONLY (峦头形势、玄空飞星、八宅)." }
                }
            },
            "translations": {
                "type": "object",
                "additionalProperties": false,
                "required": ["zh", "es"],
                "properties": {
                    "zh": localized_narrative_schema(),
                    "es": localized_narrative_schema()
                }
            }
        }
    })
}

static FH_BANNED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:ideal for families|great for families|perfect for families|family[- ]friendly|empty[- ]nesters?|young couples?|christian|muslim|jewish|hindu|buddhist|racial|ethnic)\b")
        .expect("fh banned regex")
});
static FH_FAMILIES_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:ideal|great|perfect) for families(?: with (?:kids|children))?\b").unwrap()
});
static FH_FRIENDLY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bfamily[- ]friendly\b").unwrap());
static FH_EMPTY_NESTER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bempty[- ]nesters?\b").unwrap());
static CJK_CHAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\u4e00-\u9fff]").unwrap());

pub fn sanitize_fair_housing(val: Value) -> Value {
    match val {
        Value::String(s) => {
            let s1 = FH_FAMILIES_RE.replace_all(
                &s,
                "suited to households prioritizing school proximity and multi-bedroom space",
            );
            let s2 = FH_FRIENDLY_RE.replace_all(&s1, "residential");
            let s3 = FH_EMPTY_NESTER_RE
                .replace_all(&s2, "households seeking lower-maintenance living")
                .into_owned();
            Value::String(crate::shared::scrub_competitor_brands(&s3))
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sanitize_fair_housing).collect()),
        Value::Object(obj) => {
            let mut out = Map::new();
            for (k, v) in obj {
                out.insert(k, sanitize_fair_housing(v));
            }
            Value::Object(out)
        }
        other => other,
    }
}

pub fn validate_analyst_data(d: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let cjk_count = |s: Option<&str>| -> usize {
        s.map(|txt| CJK_CHAR_RE.find_iter(txt).count()).unwrap_or(0)
    };
    let headline = d.get("headline").and_then(|v| v.as_str()).unwrap_or("");
    if headline.chars().count() < 20 {
        errs.push("headline too short".to_string());
    }
    let summary = d.get("summary").and_then(|v| v.as_str());
    let hist_note = d.pointer("/sectionNotes/history").and_then(|v| v.as_str());
    if cjk_count(Some(headline)) > 4 || cjk_count(summary) > 6 || cjk_count(hist_note) > 4 {
        errs.push("non-fengShui fields must be written in English".to_string());
    }
    if d.get("upside")
        .and_then(|v| v.as_array())
        .map(|a| a.len() < 3)
        .unwrap_or(true)
    {
        errs.push("upside needs >=3 items".to_string());
    }
    if d.get("downside")
        .and_then(|v| v.as_array())
        .map(|a| a.len() < 3)
        .unwrap_or(true)
    {
        errs.push("downside needs >=3 items".to_string());
    }
    if d.get("riskMatrix")
        .and_then(|v| v.as_array())
        .map(|a| a.len() < 4)
        .unwrap_or(true)
    {
        errs.push("riskMatrix needs >=4 items".to_string());
    }
    let fs_zh = d.pointer("/fengShui/zh").and_then(|v| v.as_str()).unwrap_or("");
    if fs_zh.is_empty() || !CJK_CHAR_RE.is_match(fs_zh) {
        errs.push("fengShui.zh must contain Chinese characters".to_string());
    }
    if let Some(zh) = d.pointer("/translations/zh") {
        let zh_hl = zh.get("headline").and_then(|v| v.as_str()).unwrap_or("");
        if zh_hl.chars().count() < 8 || !CJK_CHAR_RE.is_match(zh_hl) {
            errs.push("translations.zh.headline must be substantive Simplified Chinese".to_string());
        }
        let zh_val = zh.pointer("/sectionNotes/value").and_then(|v| v.as_str()).unwrap_or("");
        if zh_val.is_empty() || !CJK_CHAR_RE.is_match(zh_val) {
            errs.push("translations.zh.sectionNotes.value must contain Chinese characters".to_string());
        }
        if zh.get("upside").and_then(|v| v.as_array()).map(|a| a.len() < 3).unwrap_or(true) {
            errs.push("translations.zh.upside needs >=3 items".to_string());
        }
        if zh.get("downside").and_then(|v| v.as_array()).map(|a| a.len() < 3).unwrap_or(true) {
            errs.push("translations.zh.downside needs >=3 items".to_string());
        }
        if zh.get("riskMatrix").and_then(|v| v.as_array()).map(|a| a.len() < 4).unwrap_or(true) {
            errs.push("translations.zh.riskMatrix needs >=4 items".to_string());
        }
    } else {
        errs.push("translations.zh is required".to_string());
    }
    if let Some(es) = d.pointer("/translations/es") {
        let es_hl = es.get("headline").and_then(|v| v.as_str()).unwrap_or("");
        if es_hl.chars().count() < 15 {
            errs.push("translations.es.headline too short".to_string());
        }
        if es.pointer("/sectionNotes/value").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
            errs.push("translations.es.sectionNotes.value required".to_string());
        }
        if es.get("upside").and_then(|v| v.as_array()).map(|a| a.len() < 3).unwrap_or(true) {
            errs.push("translations.es.upside needs >=3 items".to_string());
        }
        if es.get("downside").and_then(|v| v.as_array()).map(|a| a.len() < 3).unwrap_or(true) {
            errs.push("translations.es.downside needs >=3 items".to_string());
        }
        if es.get("riskMatrix").and_then(|v| v.as_array()).map(|a| a.len() < 4).unwrap_or(true) {
            errs.push("translations.es.riskMatrix needs >=4 items".to_string());
        }
    } else {
        errs.push("translations.es is required".to_string());
    }
    errs
}

fn compass_word_es(compass: &str) -> &str {
    match compass.trim().to_ascii_uppercase().as_str() {
        "N" => "norte",
        "NE" => "noreste",
        "E" => "este",
        "SE" => "sureste",
        "S" => "sur",
        "SW" => "suroeste",
        "W" => "oeste",
        "NW" => "noroeste",
        _ => compass,
    }
}

fn prop_type_zh(prop_type: &str) -> &str {
    let l = prop_type.to_ascii_lowercase();
    if l.contains("condo") || l.contains("co-op") {
        "公寓"
    } else if l.contains("town") {
        "联排住宅"
    } else if l.contains("multi") {
        "多户住宅"
    } else if l.contains("land") || l.contains("lot") {
        "土地"
    } else {
        "独栋住宅"
    }
}

fn prop_type_es(prop_type: &str) -> &str {
    let l = prop_type.to_ascii_lowercase();
    if l.contains("condo") || l.contains("co-op") {
        "condominio"
    } else if l.contains("town") {
        "casa adosada"
    } else if l.contains("multi") {
        "propiedad multifamiliar"
    } else if l.contains("land") || l.contains("lot") {
        "terreno"
    } else {
        "vivienda unifamiliar"
    }
}

fn fmt_int_commas(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    if neg {
        out.push('-');
    }
    for (idx, ch) in s.chars().enumerate() {
        if idx > 0 && (s.len() - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn fmt_usd_mo(n: Option<f64>) -> Option<String> {
    n.filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| format!("${}/mo", fmt_int_commas(v.round() as i64)))
}

fn fmt_usd(n: Option<f64>) -> String {
    match n {
        Some(v) if v.is_finite() && v > 0.0 => {
            if v >= 1_000_000.0 {
                format!("${:.2}M", v / 1_000_000.0)
            } else {
                format!("${:.0}K", v / 1_000.0)
            }
        }
        _ => "N/A".to_string(),
    }
}

/// Deterministic, schema-valid analyst synthesis grounded strictly in `report` facts, valuation, hazards,
/// environment, civic, and feng-shui sections (used when `OPENAI_API_KEY` is unconfigured or in offline mode).
pub fn build_deterministic_analyst_fallback(report: &Value) -> Value {
    let (brand_name, _) = crate::shared::report_brand(report);
    let address = report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .and_then(|v| v.as_str())
        .unwrap_or("Subject Property");
    let beds_num = report.pointer("/facts/beds").and_then(|v| v.as_f64());
    let baths_num = report.pointer("/facts/baths").and_then(|v| v.as_f64());
    let sqft_num = report.pointer("/facts/sqft").and_then(|v| v.as_i64());
    let lot_num = report.pointer("/facts/lotSqft").and_then(|v| v.as_i64());
    let year_num = report.pointer("/facts/yearBuilt").and_then(|v| v.as_i64());

    let beds = beds_num
        .map(|v| format!("{v:.0}-bed"))
        .unwrap_or_else(|| "residential".to_string());
    let baths = baths_num
        .map(|v| format!("{v} bath"))
        .unwrap_or_else(|| "multi-bath".to_string());
    let sqft = sqft_num
        .map(|v| format!("{} sq ft", fmt_int_commas(v)))
        .unwrap_or_else(|| "living area".to_string());
    let lot_sqft = lot_num
        .map(|v| format!("{} sq ft lot", fmt_int_commas(v)))
        .unwrap_or_else(|| "standard parcel".to_string());
    let year_built = year_num
        .map(|v| v.to_string())
        .unwrap_or_else(|| "established vintage".to_string());

    let beds_zh = beds_num
        .map(|v| format!("{v:.0}卧"))
        .unwrap_or_else(|| "住宅".to_string());
    let baths_zh = baths_num
        .map(|v| format!("{v}卫"))
        .unwrap_or_else(|| "多卫".to_string());
    let sqft_zh = sqft_num
        .map(|v| format!("{}平方英尺", fmt_int_commas(v)))
        .unwrap_or_else(|| "居住面积".to_string());
    let lot_zh = lot_num
        .map(|v| format!("{}平方英尺占地", fmt_int_commas(v)))
        .unwrap_or_else(|| "标准地块".to_string());
    let year_zh = year_num
        .map(|v| format!("{v}年建"))
        .unwrap_or_else(|| "既有年份建造".to_string());

    let beds_es = beds_num
        .map(|v| format!("{v:.0} hab."))
        .unwrap_or_else(|| "residencial".to_string());
    let baths_es = baths_num
        .map(|v| format!("{v} baños"))
        .unwrap_or_else(|| "múltiples baños".to_string());
    let sqft_es = sqft_num
        .map(|v| format!("{} pies²", fmt_int_commas(v)))
        .unwrap_or_else(|| "superficie habitable".to_string());
    let lot_es = lot_num
        .map(|v| format!("lote de {} pies²", fmt_int_commas(v)))
        .unwrap_or_else(|| "parcela estándar".to_string());
    let year_es = year_num
        .map(|v| v.to_string())
        .unwrap_or_else(|| "época establecida".to_string());

    let prop_type = report
        .pointer("/facts/propertyType")
        .and_then(|v| v.as_str())
        .unwrap_or("Single-family residence");
    let prop_zh = prop_type_zh(prop_type);
    let prop_es = prop_type_es(prop_type);

    let est_val = report
        .pointer("/modules/valuation/data/estimate/value")
        .and_then(|v| v.as_f64());
    let est_low = report
        .pointer("/modules/valuation/data/estimate/low")
        .and_then(|v| v.as_f64());
    let est_high = report
        .pointer("/modules/valuation/data/estimate/high")
        .and_then(|v| v.as_f64());
    let last_sale = report
        .pointer("/facts/lastSale/price")
        .and_then(|v| v.as_f64());
    let last_sale_raw = report.pointer("/facts/lastSale/date").and_then(|v| v.as_str());
    let last_sale_date = last_sale_raw.unwrap_or("prior closing");
    let last_sale_date_zh = last_sale_raw.unwrap_or("过往过户日");
    let last_sale_date_es = last_sale_raw.unwrap_or("cierre anterior");
    let monthly_cost = report
        .pointer("/modules/valuation/data/ownership/monthly/total")
        .or_else(|| report.pointer("/modules/valuation/data/ownership/monthly"))
        .and_then(|v| v.as_f64());
    let rent_est = report
        .pointer("/modules/valuation/data/ownership/rentEstimate")
        .or_else(|| report.pointer("/facts/rentEstimate"))
        .and_then(|v| v.as_f64());
    let overall_score = report
        .pointer("/summary/scores/overall")
        .and_then(|v| v.as_i64())
        .unwrap_or(70);

    let flood_raw = report
        .pointer("/modules/hazards/data/flood/zone")
        .and_then(|v| v.as_str());
    let flood_zone = flood_raw.unwrap_or("mapped FEMA zone");
    let flood_zone_zh = flood_raw.unwrap_or("已测绘FEMA区域");
    let flood_zone_es = flood_raw.unwrap_or("zona FEMA cartografiada");
    let pga_num = report
        .pointer("/modules/hazards/data/earthquake/designMaps/pgaM")
        .and_then(|v| v.as_f64());
    let pga = pga_num
        .map(|g| format!("{g:.2}g"))
        .unwrap_or_else(|| "modeled USGS PGA".to_string());
    let pga_zh = pga_num
        .map(|g| format!("{g:.2}g"))
        .unwrap_or_else(|| "USGS模型峰值加速度".to_string());
    let pga_es = pga_num
        .map(|g| format!("{g:.2}g"))
        .unwrap_or_else(|| "PGA modelado del USGS".to_string());
    let facing_compass = report
        .pointer("/modules/fengshui/data/orientation/facingCompass")
        .and_then(|v| v.as_str())
        .unwrap_or("NE");
    let sitting_compass = report
        .pointer("/modules/fengshui/data/orientation/sittingCompass")
        .and_then(|v| v.as_str())
        .unwrap_or("SW");
    let facing_deg_num = report
        .pointer("/modules/fengshui/data/orientation/facingTrue")
        .and_then(|v| v.as_f64());
    let facing_deg = facing_deg_num
        .map(|d| format!("{d:.1}°"))
        .unwrap_or_else(|| "measured compass bearing".to_string());
    let facing_deg_zh = facing_deg_num
        .map(|d| format!("{d:.1}°"))
        .unwrap_or_else(|| "实测方位角".to_string());
    let facing_deg_es = facing_deg_num
        .map(|d| format!("{d:.1}°"))
        .unwrap_or_else(|| "rumbo medido con brújula".to_string());
    // Prose spells directions out: "sits north" in English, 坐北朝南 in Chinese. The raw
    // abbreviations read as codes in a sentence and survive machine translation as "坐N朝S".
    let sitting_word = compass_word(sitting_compass).unwrap_or(sitting_compass);
    let facing_word = compass_word(facing_compass).unwrap_or(facing_compass);
    let sitting_word_es = compass_word_es(sitting_compass);
    let facing_word_es = compass_word_es(facing_compass);
    let sitting_facing_cn = sitting_facing_zh(sitting_compass, facing_compass)
        .unwrap_or_else(|| "坐向待现场罗盘确认".to_string());

    let monthly_zh = monthly_cost
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| format!("${}/月", fmt_int_commas(v.round() as i64)))
        .unwrap_or_else(|| "按当前利率测算的月供".to_string());
    let rent_zh = rent_est
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| format!("${}/月", fmt_int_commas(v.round() as i64)))
        .unwrap_or_else(|| "周边市场租金".to_string());
    let monthly_es = monthly_cost
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| format!("${}/mes", fmt_int_commas(v.round() as i64)))
        .unwrap_or_else(|| "costo mensual modelado".to_string());
    let rent_es = rent_est
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| format!("${}/mes", fmt_int_commas(v.round() as i64)))
        .unwrap_or_else(|| "alquiler estimado del área".to_string());

    let extract_str_list = |ptr: &str, fallback: Vec<String>| -> Vec<String> {
        let arr = report
            .pointer(ptr)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if arr.len() >= 3 {
            arr
        } else {
            fallback
        }
    };

    let upside = extract_str_list(
        "/summary/findings/strengths",
        vec![
            format!(
                "Blended statistical valuation centers at {} (range {}–{}).",
                fmt_usd(est_val),
                fmt_usd(est_low),
                fmt_usd(est_high)
            ),
            format!("Overall {brand_name} due-diligence composite score is {overall_score}/100."),
            format!("Physical footprint offers {beds}, {baths}, {sqft} on a {lot_sqft}."),
            "Public water utility and neighborhood amenity access metrics are documented in public datasets."
                .to_string(),
        ],
    );

    let downside = extract_str_list(
        "/summary/findings/risks",
        vec![
            format!(
                "Seismic design peak ground acceleration is {pga} and flood designation is FEMA Zone {flood_zone}."
            ),
            format!(
                "Estimated monthly ownership cost ({}) exceeds modeled market rent ({}).",
                fmt_usd_mo(monthly_cost)
                    .unwrap_or_else(|| "modeled PITI".to_string()),
                fmt_usd_mo(rent_est)
                    .unwrap_or_else(|| "area rent".to_string())
            ),
            format!("Built in {year_built}; mechanical, roof, electrical, and sewer systems require physical inspection."),
            "Public parcel and MLS records should be verified for permit finality and square-footage consistency."
                .to_string(),
        ],
    );

    let verify = extract_str_list(
        "/summary/findings/watch",
        vec![
            "Pull municipal building permits for any additions, sunroom enclosures, kitchen/bath remodels, roof, and HVAC work."
                .to_string(),
            "Order general home, termite/WDO, roof, sewer lateral, and foundation/seismic-bracing inspections."
                .to_string(),
            format!("Confirm FEMA Zone {flood_zone} and earthquake insurance quotes prior to contingency removal."),
            "Verify school attendance boundaries directly with the assigned school districts.".to_string(),
            "Review Preliminary Title Report Schedule A/B vesting, easements, CC&Rs, and tax assessments in escrow."
                .to_string(),
        ],
    );

    let headline = format!(
        "{beds} {prop_type} at {address} ({overall_score}/100 score; blended value {} vs {} last sale).",
        fmt_usd(est_val),
        fmt_usd(last_sale)
    );

    let summary = format!(
        "Statistical valuation blends at {} (range {}–{}) compared against the {} sale on {last_sale_date}. Key due-diligence priorities include verifying municipal permits, assessing {pga} seismic and Zone {flood_zone} hazard exposure, and underwriting carrying costs against {} estimated rent.",
        fmt_usd(est_val),
        fmt_usd(est_low),
        fmt_usd(est_high),
        fmt_usd(last_sale),
        fmt_usd_mo(rent_est).unwrap_or_else(|| "local".to_string())
    );

    let about_home = format!(
        "{address} is a {prop_type} built in {year_built} featuring {beds}, {baths}, and {sqft} on a {lot_sqft}. Public and listing records document its parcel configuration, roof and HVAC systems, and immediate street orientation, sitting {sitting_word} and facing {facing_word} ({facing_deg} true). Buyers should cross-check living-area measurements, municipal permit history, and seller disclosures during physical inspection."
    );

    sanitize_fair_housing(json!({
        "headline": headline,
        "summary": summary,
        "aboutHome": about_home,
        "paragraphs": [
            format!(
                "Valuation & Market Context: Blended value is {} ({}–{}), compared with the {} recorded sale ({last_sale_date}). Nearby comparable sales and ZIP home-value index trends frame pricing discipline.",
                fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high), fmt_usd(last_sale)
            ),
            format!(
                "Hazards & Environment: FEMA flood mapping places the parcel in Zone {flood_zone}, while USGS ASCE 7-22 seismic design PGA is {pga}. Environmental screening covers nearby regulated cleanup cases, regional air quality, and public water compliance."
            ),
            format!(
                "Livability & Ownership Math: Modeled monthly carrying cost is {} versus {} estimated rent. Confirm school district assignment, commute times, and property-tax reassessment impacts before closing.",
                fmt_usd_mo(monthly_cost).unwrap_or_else(|| "modeled at current 30-yr rates".to_string()),
                fmt_usd_mo(rent_est).unwrap_or_else(|| "market rent".to_string())
            )
        ],
        "upside": upside,
        "downside": downside,
        "verify": verify,
        "sectionNotes": {
            "value": format!("Blended statistical estimate is {} with a {}–{} confidence interval across repeat-sales, adjusted comps, and third-party estimates.", fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high)),
            "history": format!("Latest recorded transfer was {} on {last_sale_date}; review prior assessments and holding-period appreciation.", fmt_usd(last_sale)),
            "comps": "Weighted comparable sales adjust for living area, lot size, age, bathroom count, and market timing.",
            "market": "ZIP home-value index, inventory shifts, days-to-pending, and 30-year mortgage rates govern near-term pricing power.",
            "cost": format!("Monthly PITI + maintenance carrying cost ({}) should be stress-tested including statutory property-tax reassessment.", fmt_usd_mo(monthly_cost).unwrap_or_else(|| "at current rates".to_string())),
            "hazards": format!("Parcel sits in FEMA Zone {flood_zone} with modeled seismic PGA of {pga}; verify structural bracing and insurance quotes."),
            "environment": "Environmental screening synthesizes EPA/state cleanup databases, modeled 24-h transportation noise, AQI, and SDWIS water records.",
            "schools": "Assigned public school districts and nearby NCES campuses are mapped; always confirm current attendance caps with the district.",
            "neighborhood": "Amenity access, supermarket/park proximity, walkability score, and arterial/freeway distances shape daily convenience.",
            "fengshui": format!("Cultural site reading notes the house sits {sitting_word} and faces {facing_word} ({facing_deg} true); verify exact door bearing on site with a luopan.")
        },
        "outlook": {
            "oneYear": "12-month trajectory reflects the 12-month ZIP forecast, current metro sale-to-list ratio, active inventory, and mortgage-rate sensitivity.",
            "fiveYear": "5-year risk/reward profile is anchored by historical ZIP P10/P50/P90 forward returns, land-supply constraints, and historical max-drawdown stress tests."
        },
        "riskMatrix": [
            {
                "risk": "Seismic shaking & foundation bracing",
                "likelihood": "medium",
                "impact": "high",
                "mitigation": format!("Inspect foundation bolting/crawlspace and price earthquake coverage for {pga} PGA.")
            },
            {
                "risk": "Permit & living-area verification",
                "likelihood": "medium",
                "impact": "medium",
                "mitigation": "Pull city permit records and verify permitted square footage against assessor records."
            },
            {
                "risk": "Aging mechanical, roof & sewer systems",
                "likelihood": "medium",
                "impact": "medium",
                "mitigation": format!("Complete roof, HVAC, sewer lateral, and WDO inspections on this {year_built} structure.")
            },
            {
                "risk": "Carrying cost & tax reassessment reset",
                "likelihood": "high",
                "impact": "medium",
                "mitigation": "Budget for supplemental and reassessed property taxes at purchase price."
            }
        ],
        "fit": {
            "prioritize": [
                format!("Buyers seeking {beds}, {baths}, and {sqft} on a {lot_sqft}."),
                "Households prioritizing documented proximity to groceries, parks, and transit corridors.",
                "Data-driven buyers who value transparent comp, hazard, and environmental screening."
            ],
            "tradeoffs": [
                format!("Built in {year_built}, requiring lifecycle budgeting for roof, plumbing, electrical, and HVAC."),
                "Monthly ownership carrying cost exceeds local rental parity at prevailing mortgage rates.",
                format!("Requires routine seismic ({pga}) and flood-zone ({flood_zone}) due diligence.")
            ]
        },
        "negotiation": [
            format!("Anchor offer strategy to the {} blended valuation and adjusted-comp median rather than peak asking prices.", fmt_usd(est_val)),
            "Retain inspection contingencies to price Section-1 WDO, roof, sewer lateral, or electrical panel findings.",
            "Request complete city permit documentation and seller disclosures early in contingency review.",
            "Factor insurance and property-tax reassessment into net effective monthly budget."
        ],
        "sellerQuestions": [
            "Can you provide finaled municipal permits for all renovations, additions, or enclosed spaces?",
            "What are the installation dates and service histories for the roof, HVAC, water heater, and electrical panel?",
            "Has the sewer lateral been camera-inspected, cleaned, or replaced?",
            "Are there any past water intrusion, drainage, foundation, or pest/WDO reports?",
            "Are there any unrecorded easements, shared-fence agreements, solar leases/UCC-1 filings, or special assessments?"
        ],
        "fengShui": {
            "en": "",
            "zh": format!("从传统堪舆文化角度看，本宅{sitting_facing_cn}（真北朝向约{facing_deg_zh}），结合峦头道路水势、靠山明堂与九运玄空飞星、八宅门位综合参看，建议现场以罗盘复核入户门精确度数（仅作传统文化参考）。")
        },
        "translations": {
            "zh": {
                "headline": format!(
                    "{address} {beds_zh}{prop_zh}（综合尽调评分 {overall_score}/100；混合估值 {}，前次成交 {}）。",
                    fmt_usd(est_val),
                    fmt_usd(last_sale)
                ),
                "summary": format!(
                    "基于模型与可比成交的混合估值为 {}（置信区间 {}–{}），对比 {last_sale_date_zh} 记录的 {} 成交价。建议重点核查市政施工许可、{pga_zh} 地震设防与 FEMA {flood_zone_zh} 区险情及投保成本，并结合 {rent_zh} 的周边参考租金测算月度持有支出。",
                    fmt_usd(est_val),
                    fmt_usd(est_low),
                    fmt_usd(est_high),
                    fmt_usd(last_sale)
                ),
                "aboutHome": format!(
                    "{address} 为{year_zh}的{prop_zh}，配置为 {beds_zh}、{baths_zh}，室内居住面积 {sqft_zh}，{lot_zh}。公开产权与挂牌档案记录了地块轮廓、屋面及暖通配置与街道朝向关系（{sitting_facing_cn}，真北朝向约 {facing_deg_zh}）。建议买家在实地验房期间交叉核对实测面积、历史竣工许可与卖家披露文件。"
                ),
                "paragraphs": [
                    format!(
                        "估值与市场脉络：混合统计估值为 {}（区间 {}–{}），对比 {last_sale_date_zh} 的 {} 成交记录。周边可比成交及邮编房价指数共同界定合理出价区间。",
                        fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high), fmt_usd(last_sale)
                    ),
                    format!(
                        "自然灾害与环境筛查：FEMA 洪水图斑将该地块划为 {flood_zone_zh}，USGS ASCE 7-22 设计地震峰值地面加速度为 {pga_zh}。环境模块同时覆盖周边监管污染点位、空气质量及市政饮用水合规记录。"
                    ),
                    format!(
                        "居住便利与持有成本：模型测算月度持有成本约 {monthly_zh}，对比估算市场租金 {rent_zh}。签字过户前请逐项确认学区划片边界、通勤路线以及成交后地税重估幅度。"
                    )
                ],
                "upside": [
                    format!("混合统计估值中枢为 {}（区间 {}–{}）。", fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high)),
                    format!("{brand_name} 综合尽调评分为 {overall_score}/100。"),
                    format!("实体规模提供 {beds_zh}、{baths_zh}、{sqft_zh}，对应 {lot_zh}。"),
                    "公共供水系统合规模态与周边生活配套可达性均已由公开数据集完整记录。".to_string()
                ],
                "downside": [
                    format!("设计地震峰值地面加速度为 {pga_zh}，FEMA 洪水分区为 {flood_zone_zh}。"),
                    format!("测算月度持有支出（{monthly_zh}）高于当前市场估算租金（{rent_zh}）。"),
                    format!("{year_zh}建筑；屋面、管线、电气及排水主管等老化设备须经专业实地查验。"),
                    "需将公开地籍与 MLS 面积记录、加建许可核对一致。".to_string()
                ],
                "verify": [
                    "调取市政建管局历史施工许可，核实加建、封闭阳光房、厨卫改造及屋面/暖通工程完工验收记录。".to_string(),
                    "安排全屋结构、白蚁木蠹虫（WDO）、屋顶、下水主管道内窥镜及基础抗震锚栓专项检查。".to_string(),
                    format!("在解除检查保留条款前确认 FEMA {flood_zone_zh} 区与地震险实际核保报价。"),
                    "直接向对应学区办公室确认现行就读划片边界与学位名额状态。".to_string(),
                    "过户托管期间仔细审阅产权初审报告（Schedule A/B）归属权、地役权、社区公约及税费记录。".to_string()
                ],
                "sectionNotes": {
                    "value": format!("融合重复成交指数、修正可比成交与第三方估值的混合模型中枢为 {}，区间 {}–{}。", fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high)),
                    "history": format!("最近一次过户成交记录为 {last_sale_date_zh} 的 {}；建议比对历年计税评估与持有期涨幅。", fmt_usd(last_sale)),
                    "comps": "加权可比成交已对居住面积、地块规模、房龄、卫浴数量及成交时间差进行统计修正。".to_string(),
                    "market": "邮编房价指数、活跃库存、待售成交天数与 30 年期房贷利率共同主导短期议价空间。".to_string(),
                    "cost": format!("月度持有成本（按揭本息+地税+保险+维护，{monthly_zh}）需纳入房产税重估上调空间进行压力测试。"),
                    "hazards": format!("地块属 FEMA {flood_zone_zh} 洪水区，地震设计峰值加速度 {pga_zh}；建议核实结构抗震加固与专项保险报价。"),
                    "environment": "环境尽调汇总 EPA/州级清理监管点位、24 小时交通噪声模型、空气质量指数与 SDWIS 供水记录。".to_string(),
                    "schools": "已标示对应公立学区与周边 NCES 学校；报名入学前务必向学区核实当期入学政策与名额。".to_string(),
                    "neighborhood": "商超公园步行距离、步行便利指数以及主干道/高速公路间距共同决定日常通勤与居住便利度。".to_string(),
                    "fengshui": format!("堪舆方位初判显示本宅{sitting_facing_cn}（真北朝向约 {facing_deg_zh}）；入户门精确度数为准，建议现场以罗盘实测复核。")
                },
                "outlook": {
                    "oneYear": "12 个月价格走势主要受邮编 12 个月预测、都会区成交售价比、在售库存周期及按揭利率波动影响。".to_string(),
                    "fiveYear": "5 年维度风险收益依托同邮编历史 P10/P50/P90 远期回报分布、可开发土地约束与历史最大回撤压力测试。".to_string()
                },
                "riskMatrix": [
                    {
                        "risk": "地震晃动与基础抗震锚栓情况",
                        "likelihood": "medium",
                        "impact": "high",
                        "mitigation": format!("检查地基螺栓加固及爬行空间状况，并结合 {pga_zh} 峰值加速度评估地震险报价。")
                    },
                    {
                        "risk": "施工许可与合法居住面积一致性",
                        "likelihood": "medium",
                        "impact": "medium",
                        "mitigation": "调取市政府完整许可档案，逐项核对实际面积与地税局登记数据。"
                    },
                    {
                        "risk": "屋面、暖通机械与排水主管老化成本",
                        "likelihood": "medium",
                        "impact": "medium",
                        "mitigation": format!("针对该 {year_zh} 房屋完成屋面、HVAC、排水管道内窥及白蚁防治全面查验。")
                    },
                    {
                        "risk": "持有成本与成交后房产税重估跳升",
                        "likelihood": "high",
                        "impact": "medium",
                        "mitigation": "按实际购入价重新测算补充税单与常规年度地税增幅。"
                    }
                ],
                "fit": {
                    "prioritize": [
                        format!("偏好 {beds_zh}、{baths_zh}、{sqft_zh} 空间尺度与 {lot_zh} 条件的置业家庭或买家。"),
                        "重视商超、公园及交通走廊有明确量化可达性数据的购房者。".to_string(),
                        "注重可比成交修正、自然灾害量化与环境点位透明溯源的理性买家。".to_string()
                    ],
                    "tradeoffs": [
                        format!("{year_zh}，需为屋面、给排水、电气配电与暖通设备预留中长期更新预算。"),
                        "按现行房贷利率测算，月度持有成本显著高于区域同户型长租水平。".to_string(),
                        format!("须持续关注 {pga_zh} 抗震与 FEMA {flood_zone_zh} 区相关查验及保障条款。")
                    ]
                },
                "negotiation": [
                    format!("以 {} 混合估值及修正可比成交中位数作为核心议价锚点，避免追高挂牌溢价。", fmt_usd(est_val)),
                    "保留完整验房条款，将白蚁一类危害、屋顶修缮、排水管或配电盘改造纳入信用抵扣或降价依据。".to_string(),
                    "在解约保护期前期索取市政竣工许可清单与完整卖家披露文件。".to_string(),
                    "将房屋保险及交易触发的房产税重估增量纳入净有效月供预算进行整体议价。".to_string()
                ],
                "sellerQuestions": [
                    "能否提供历次装修改建、加建或封闭空间对应的市政终验合格许可文件？".to_string(),
                    "屋顶、暖通空调（HVAC）、热水器及主配电箱各自的更换年份与维保记录为何？".to_string(),
                    "室外排污主管是否进行过内窥镜检测、疏通或整体更换？".to_string(),
                    "过往是否发生过渗水、积水排涝问题，或留存过地基/白蚁防治处理报告？".to_string(),
                    "是否存在未登记地役权、共有围栏协议、太阳能租约/UCC-1 留置登记或特殊税费摊派？".to_string()
                ]
            },
            "es": {
                "headline": format!(
                    "{prop_es} de {beds_es} en {address} (puntuación {overall_score}/100; valor estimado {} vs {} última venta).",
                    fmt_usd(est_val),
                    fmt_usd(last_sale)
                ),
                "summary": format!(
                    "La valoración estadística combinada se sitúa en {} (rango {}–{}) frente a la venta de {} el {last_sale_date_es}. Las prioridades clave de diligencia debida incluyen verificar permisos municipales, evaluar la exposición sísmica ({pga_es}) y la zona FEMA {flood_zone_es}, y comparar los costos de tenencia frente al alquiler estimado ({rent_es}).",
                    fmt_usd(est_val),
                    fmt_usd(est_low),
                    fmt_usd(est_high),
                    fmt_usd(last_sale)
                ),
                "aboutHome": format!(
                    "{address} es una {prop_es} construida en {year_es} con {beds_es}, {baths_es} y {sqft_es} sobre un {lot_es}. Los registros públicos y de listado documentan su configuración parcelaria, techo, climatización y orientación respecto a la vía pública, con fondo hacia el {sitting_word_es} y frente al {facing_word_es} ({facing_deg_es} norte verdadero). Conviene contrastar medidas habitables, historial de permisos municipales y declaraciones del vendedor durante la inspección física."
                ),
                "paragraphs": [
                    format!(
                        "Valoración y mercado: El valor estadístico combinado es {} ({}–{}), frente a la compraventa registrada de {} ({last_sale_date_es}). Las ventas comparables cercanas y la tendencia del índice de valor del código postal enmarcan la disciplina de oferta.",
                        fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high), fmt_usd(last_sale)
                    ),
                    format!(
                        "Riesgos naturales y entorno: La cartografía FEMA ubica la parcela en Zona {flood_zone_es}, mientras que la aceleración sísmica de diseño USGS ASCE 7-22 es {pga_es}. La evaluación ambiental revisa sitios regulados cercanos, calidad del aire y cumplimiento del agua pública."
                    ),
                    format!(
                        "Habitabilidad y economía de tenencia: El costo mensual modelado es {monthly_es} frente a {rent_es} de alquiler estimado. Confirme límites escolares, tiempos de traslado y el reajuste del impuesto predial antes del cierre."
                    )
                ],
                "upside": [
                    format!("Valoración estadística combinada centrada en {} (rango {}–{}).", fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high)),
                    format!("Puntuación compuesta de diligencia debida {brand_name} de {overall_score}/100."),
                    format!("Huella física con {beds_es}, {baths_es}, {sqft_es} en un {lot_es}."),
                    "Registros públicos documentan el sistema de agua potable y la accesibilidad a servicios del vecindario.".to_string()
                ],
                "downside": [
                    format!("Aceleración sísmica de diseño {pga_es} y designación de inundación FEMA Zona {flood_zone_es}."),
                    format!("El costo mensual estimado de propiedad ({monthly_es}) supera el alquiler de mercado modelado ({rent_es})."),
                    format!("Construcción de {year_es}; techo, sistemas mecánicos, eléctricos y drenaje requieren inspección física."),
                    "Conviene verificar coherencia de superficie habitable y cierre final de permisos en registros municipales.".to_string()
                ],
                "verify": [
                    "Solicitar permisos municipales finalizados para ampliaciones, cerramientos, remodelaciones de cocina/baños, techo y climatización.".to_string(),
                    "Encargar inspección general, termitas/WDO, techo, cámara en alcantarillado lateral y anclaje sísmico de cimentación.".to_string(),
                    format!("Confirmar cotizaciones de seguro contra inundación FEMA Zona {flood_zone_es} y terremoto antes de levantar contingencias."),
                    "Verificar directamente los límites vigentes de asignación escolar con el distrito correspondiente.".to_string(),
                    "Revisar anexos A/B del informe preliminar de título, servidumbres, estatutos HOA y gravámenes fiscales en escrow.".to_string()
                ],
                "sectionNotes": {
                    "value": format!("La estimación estadística combinada alcanza {} con un intervalo de confianza de {}–{} entre ventas repetidas, comparables ajustados y estimaciones de terceros.", fmt_usd(est_val), fmt_usd(est_low), fmt_usd(est_high)),
                    "history": format!("La última transferencia registrada fue de {} el {last_sale_date_es}; revise tasaciones fiscales previas y plusvalía acumulada.", fmt_usd(last_sale)),
                    "comps": "Las ventas comparables ponderadas ajustan superficie habitable, terreno, antigüedad, número de baños y fecha de venta.".to_string(),
                    "market": "El índice de valor del código postal, inventario activo, días en mercado y tasa hipotecaria a 30 años marcan el poder negociador.".to_string(),
                    "cost": format!("El costo mensual PITI más mantenimiento ({monthly_es}) debe evaluarse incorporando el reajuste estatutario del impuesto predial."),
                    "hazards": format!("La parcela figura en FEMA Zona {flood_zone_es} con PGA sísmico modelado de {pga_es}; verifique refuerzo estructural y pólizas."),
                    "environment": "La evaluación ambiental integra bases EPA/estatales de remediación, ruido de transporte 24h, AQI e historial de agua SDWIS.".to_string(),
                    "schools": "Se muestran los distritos escolares públicos asignados y planteles NCES cercanos; confirme cupos directamente con el distrito.".to_string(),
                    "neighborhood": "La proximidad a supermercados, parques, el índice de caminabilidad y corredores viales define la conveniencia cotidiana.".to_string(),
                    "fengshui": format!("Nota cultural: la vivienda asienta hacia el {sitting_word_es} y mira al {facing_word_es} ({facing_deg_es}); verificar rumbo exacto de puerta con brújula en sitio.")
                },
                "outlook": {
                    "oneYear": "La trayectoria a 12 meses responde al pronóstico a 12 meses del código postal, relación precio-venta metropolitana, inventario y tasas hipotecarias.".to_string(),
                    "fiveYear": "El perfil riesgo/retorno a 5 años se apoya en distribuciones históricas P10/P50/P90, restricciones de suelo y pruebas de caída máxima.".to_string()
                },
                "riskMatrix": [
                    {
                        "risk": "Sacudida sísmica y anclaje de cimentación",
                        "likelihood": "medium",
                        "impact": "high",
                        "mitigation": format!("Inspeccionar pernos de cimentación y cotizar cobertura sísmica para PGA de {pga_es}.")
                    },
                    {
                        "risk": "Permisos y verificación de superficie habitable",
                        "likelihood": "medium",
                        "impact": "medium",
                        "mitigation": "Obtener archivo municipal de permisos y conciliar metros cuadrados legales con el catastro."
                    },
                    {
                        "risk": "Desgaste de techo, climatización y drenaje",
                        "likelihood": "medium",
                        "impact": "medium",
                        "mitigation": format!("Completar inspecciones de techo, HVAC, alcantarillado lateral y termitas en esta vivienda de {year_es}.")
                    },
                    {
                        "risk": "Costo de tenencia y reajuste del impuesto predial",
                        "likelihood": "high",
                        "impact": "medium",
                        "mitigation": "Presupuestar impuestos suplementarios y reajuste predial al precio real de compra."
                    }
                ],
                "fit": {
                    "prioritize": [
                        format!("Compradores que buscan {beds_es}, {baths_es} y {sqft_es} sobre un {lot_es}."),
                        "Hogares que priorizan cercanía documentada a supermercados, parques y transporte.".to_string(),
                        "Compradores analíticos que valoran transparencia en comparables, riesgos naturales y entorno.".to_string()
                    ],
                    "tradeoffs": [
                        format!("Construida en {year_es}, lo que exige reserva de capital para techo, plomería, electricidad y HVAC."),
                        "El gasto mensual de propiedad supera la paridad de alquiler local con las tasas hipotecarias actuales.".to_string(),
                        format!("Requiere debida diligencia específica sobre sismicidad ({pga_es}) y zona de inundación ({flood_zone_es}).")
                    ]
                },
                "negotiation": [
                    format!("Anclar la estrategia de oferta a la valoración combinada de {} y la mediana de comparables ajustados.", fmt_usd(est_val)),
                    "Mantener contingencias de inspección para cuantificar hallazgos WDO de Sección 1, techo, drenaje o tablero eléctrico.".to_string(),
                    "Exigir historial completo de permisos municipales y declaraciones del vendedor al inicio del período de revisión.".to_string(),
                    "Incorporar seguros y reajuste fiscal en el presupuesto mensual efectivo antes de fijar precio definitivo.".to_string()
                ],
                "sellerQuestions": [
                    "¿Puede entregar permisos municipales finalizados para todas las remodelaciones, ampliaciones o cerramientos?".to_string(),
                    "¿Cuáles son las fechas de instalación y mantenimiento de techo, HVAC, calentador de agua y panel eléctrico?".to_string(),
                    "¿El alcantarillado lateral ha recibido inspección con cámara, limpieza o reemplazo?".to_string(),
                    "¿Existen antecedentes de filtraciones, drenaje deficiente, cimentación o reportes previos de plagas/WDO?".to_string(),
                    "¿Hay servidumbres no inscritas, acuerdos de cerca compartida, contratos solares/UCC-1 o gravámenes especiales?".to_string()
                ]
            }
        }
    }))
}

/// Run grounded AI Analyst synthesis (`LLM_MODEL` -> `LLM_FALLBACK_MODEL`) over a Open Estate `report` JSON,
/// with automatic Fair-Housing sanitization and deterministic fallback when AI keys are absent.
pub async fn run_ai_analyst(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    cache: Option<&dyn LlmCache>,
) -> Result<ModuleOutput> {
    run_ai_analyst_with_refresh(http, env, report, cache, false).await
}

pub async fn run_ai_analyst_with_refresh(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    cache: Option<&dyn LlmCache>,
    refresh: bool,
) -> Result<ModuleOutput> {
    let brand_name = env.brand_name();
    let mut warnings = Vec::new();

    if !env.llm_configured() {
        let mut fallback = build_deterministic_analyst_fallback(report);
        if let Some(obj) = fallback.as_object_mut() {
            obj.insert(
                "model".to_string(),
                Value::String("deterministic-fallback".to_string()),
            );
            obj.insert("effort".to_string(), Value::Null);
            obj.insert("fallbackUsed".to_string(), Value::Bool(true));
            obj.insert("cached".to_string(), Value::Bool(false));
            obj.insert("ms".to_string(), json!(0));
            obj.insert("usage".to_string(), Value::Null);
            obj.insert("generatedAt".to_string(), Value::String(now_iso()));
        }
        warnings.push("OPENAI_API_KEY not set — deterministic analyst synthesis used.".to_string());
        return Ok(ModuleOutput {
            data: fallback,
            sources: vec![SourceItem::new(
                format!(
                    "{brand_name} Deterministic Property Synthesis (grounded on cited report data)"
                ),
                "https://census.gov/",
            )],
            warnings,
        });
    }

    let brief = build_brief(report);
    let user_prompt = format!(
        "Analyze the following US residential property brief and return the structured JSON assessment.\n\nPROPERTY_BRIEF_JSON:\n{}",
        serde_json::to_string(&brief)?
    );

    let instructions = crate::shared::apply_brand_to_str(ANALYST_INSTRUCTIONS, &brand_name, "");
    let mut opts = LlmCallOptions::new(
        "report_synthesis",
        "property_analysis",
        instructions,
        user_prompt,
        analysis_schema(),
    );
    opts.timeout_ms = Some(
        env.get("LLM_TIMEOUT_MS")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(240_000),
    );
    opts.refresh = refresh;
    opts.validator = Some(Arc::new(validate_analyst_data));

    let res = responses_json_with_cache(http, env, opts, cache).await?;

    if res.fallback_used {
        warnings.push(format!("Primary LLM unavailable; fell back to {}.", res.model));
    }
    let raw_str = serde_json::to_string(&res.data).unwrap_or_default();
    if FH_BANNED_RE.is_match(&raw_str) {
        warnings.push("Fair-housing phrasing sanitizer normalized demographic wording.".to_string());
    }

    let mut data = sanitize_fair_housing(res.data);
    // Safety net behind the prompt rule: the brief carries directions as N/S/…, and a model can
    // still copy the letters into the Chinese synthesis or translations.zh prose.
    if let Some(zh) = data.pointer_mut("/fengShui/zh") {
        if let Some(text) = zh.as_str() {
            *zh = Value::String(localize_compass_in_cjk(text, false));
        }
    }
    if let Some(zh_tr) = data.pointer_mut("/translations/zh") {
        localize_compass_in_cjk_value(zh_tr, false);
    }
    if let Some(obj) = data.as_object_mut() {
        obj.insert("model".to_string(), Value::String(res.model.clone()));
        obj.insert(
            "effort".to_string(),
            res.effort
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        obj.insert("fallbackUsed".to_string(), Value::Bool(res.fallback_used));
        obj.insert("cached".to_string(), Value::Bool(res.cached));
        obj.insert("ms".to_string(), json!(res.ms));
        obj.insert(
            "usage".to_string(),
            res.usage
                .as_ref()
                .and_then(|u| serde_json::to_value(u).ok())
                .unwrap_or(Value::Null),
        );
        obj.insert("generatedAt".to_string(), Value::String(res.created_at));
    }

    let src_label = match res.effort.as_deref() {
        Some(eff) => format!(
            "{brand_name} AI Analyst ({} · {eff} reasoning) grounded on cited report data",
            res.model
        ),
        None => format!(
            "{brand_name} AI Analyst ({}) grounded on cited report data",
            res.model
        ),
    };

    Ok(ModuleOutput {
        data,
        sources: vec![SourceItem::new(src_label, "https://openai.com/")],
        warnings,
    })
}
