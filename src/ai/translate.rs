// Multi-language Open Estate report translator (`zh` Simplified Chinese, `es` Spanish, etc.).
// Priority:
//   1. Embedded `report.translations[lang]` content-hash cache (0 ms)
//   2. L1 `moka` / L2 `LlmCache` content-hash cache (< 1 ms)
//   3. Native OpenAI Responses API structured real-estate localization (zero external Google Translate calls)

use crate::ai::llm::{
    global_llm_cache, parse_model_spec, resolve_model_specs_with_effort, responses_json_with_cache,
    LlmCache, LlmCallOptions, DEFAULT_LLM_MODEL,
};
use crate::guardrails::GuardrailError;
use crate::shared::{localize_compass_in_cjk_value, now_iso, EnvConfig, HttpClient};
use anyhow::Result;
use moka::future::Cache;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Instant;

pub struct LangConfig {
    pub norm_code: &'static str,
    pub label: &'static str,
}

pub fn resolve_lang(raw: &str) -> Result<LangConfig, GuardrailError> {
    let lc = raw.trim().to_ascii_lowercase().replace('_', "-");
    if lc.is_empty() || lc == "zh" || lc == "zh-cn" || lc == "zh-hans" || lc == "zh-sg" {
        Ok(LangConfig {
            norm_code: "zh",
            label: "Simplified Chinese (简体中文)",
        })
    } else if lc == "zh-tw" || lc == "zh-hk" || lc == "zh-hant" {
        Ok(LangConfig {
            norm_code: "zh-tw",
            label: "Traditional Chinese (繁體中文)",
        })
    } else if lc == "es" || lc.starts_with("es-") {
        Ok(LangConfig {
            norm_code: "es",
            label: "Spanish (Español)",
        })
    } else if lc == "ko" || lc.starts_with("ko-") {
        Ok(LangConfig {
            norm_code: "ko",
            label: "Korean (한국어)",
        })
    } else if lc == "ja" || lc.starts_with("ja-") {
        Ok(LangConfig {
            norm_code: "ja",
            label: "Japanese (日本語)",
        })
    } else if lc == "vi" || lc.starts_with("vi-") {
        Ok(LangConfig {
            norm_code: "vi",
            label: "Vietnamese (Tiếng Việt)",
        })
    } else {
        let preview: String = raw.trim().chars().take(20).collect();
        Err(GuardrailError::new(
            "UNSUPPORTED_LANGUAGE",
            format!(
                "Unsupported translation language \"{preview}\". Supported languages: zh, zh-tw, es, ko, ja, vi."
            ),
            400,
        ))
    }
}

pub fn translation_schema() -> Value {
    let s = json!({ "type": "string" });
    let arr_s = json!({ "type": "array", "items": { "type": "string" } });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "verdict",
            "headline",
            "execSummary",
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
            "decisionNotes"
        ],
        "properties": {
            "verdict": s,
            "headline": s,
            "execSummary": s,
            "aboutHome": s,
            "paragraphs": arr_s,
            "upside": arr_s,
            "downside": arr_s,
            "verify": arr_s,
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
            "negotiation": arr_s,
            "sellerQuestions": arr_s,
            "decisionNotes": {
                "type": "object",
                "additionalProperties": false,
                "required": ["aduFeasibility", "aduNote", "solarNote"],
                "properties": {
                    "aduFeasibility": { "type": "string" },
                    "aduNote": { "type": "string" },
                    "solarNote": { "type": "string" }
                }
            }
        }
    })
}

fn str_or_empty(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn arr_strings(v: Option<&Value>, max_items: Option<usize>) -> Vec<String> {
    let Some(arr) = v.and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    let iter = arr.iter().map(|item| match item {
        Value::String(s) => s.clone(),
        Value::Object(o) => {
            let pt = o.get("point").and_then(|p| p.as_str()).unwrap_or("");
            let ask = o.get("suggestedAsk").and_then(|a| a.as_str()).unwrap_or("");
            if !pt.is_empty() || !ask.is_empty() {
                format!("{pt} — {ask}")
            } else {
                item.to_string()
            }
        }
        other => other.to_string(),
    });
    match max_items {
        Some(limit) => iter.take(limit).collect(),
        None => iter.collect(),
    }
}

/// Extracts all narrative, AI analyst, section-note, risk-matrix, negotiation, and decision-factor strings
/// matching `template.html`'s `activeText()` client-side language switcher.
pub fn extract_translatable_payload(report: &Value) -> Value {
    let ai = report.get("ai");
    let sum = report.get("summary");
    let nar = sum.and_then(|s| s.get("narrative"));
    let sn = ai.and_then(|a| a.get("sectionNotes"));
    let df = report.get("decisionFactors");

    let verdict = str_or_empty(sum.and_then(|s| s.get("verdict")));
    let headline_ai = str_or_empty(ai.and_then(|a| a.get("headline")));
    let headline_nar = str_or_empty(nar.and_then(|n| n.get("headline")));
    let headline = if !headline_ai.is_empty() {
        headline_ai
    } else if !headline_nar.is_empty() {
        headline_nar
    } else {
        verdict.clone()
    };

    let paragraphs = {
        let ai_p = arr_strings(ai.and_then(|a| a.get("paragraphs")), None);
        if !ai_p.is_empty() {
            ai_p
        } else {
            arr_strings(nar.and_then(|n| n.get("paragraphs")), None)
        }
    };

    let upside = {
        let ai_u = arr_strings(ai.and_then(|a| a.get("upside")), Some(8));
        if !ai_u.is_empty() {
            ai_u
        } else {
            arr_strings(sum.and_then(|s| s.pointer("/findings/strengths")), Some(8))
        }
    };
    let downside = {
        let ai_d = arr_strings(ai.and_then(|a| a.get("downside")), Some(8));
        if !ai_d.is_empty() {
            ai_d
        } else {
            arr_strings(sum.and_then(|s| s.pointer("/findings/risks")), Some(8))
        }
    };
    let verify = {
        let ai_v = arr_strings(ai.and_then(|a| a.get("verify")), Some(8));
        if !ai_v.is_empty() {
            ai_v
        } else {
            arr_strings(sum.and_then(|s| s.pointer("/findings/watch")), Some(8))
        }
    };

    let risk_matrix: Vec<Value> = ai
        .and_then(|a| a.get("riskMatrix"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    let lk = match r.get("likelihood").and_then(|x| x.as_str()) {
                        Some("low") => "low",
                        Some("high") => "high",
                        _ => "medium",
                    };
                    let imp = match r.get("impact").and_then(|x| x.as_str()) {
                        Some("low") => "low",
                        Some("high") => "high",
                        _ => "medium",
                    };
                    json!({
                        "risk": str_or_empty(r.get("risk")),
                        "likelihood": lk,
                        "impact": imp,
                        "mitigation": str_or_empty(r.get("mitigation")),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "verdict": verdict,
        "headline": headline,
        "execSummary": str_or_empty(ai.and_then(|a| a.get("summary"))),
        "aboutHome": str_or_empty(ai.and_then(|a| a.get("aboutHome"))),
        "paragraphs": paragraphs,
        "upside": upside,
        "downside": downside,
        "verify": verify,
        "sectionNotes": {
            "value": str_or_empty(sn.and_then(|x| x.get("value"))),
            "history": str_or_empty(sn.and_then(|x| x.get("history"))),
            "comps": str_or_empty(sn.and_then(|x| x.get("comps"))),
            "market": str_or_empty(sn.and_then(|x| x.get("market"))),
            "cost": str_or_empty(sn.and_then(|x| x.get("cost"))),
            "hazards": str_or_empty(sn.and_then(|x| x.get("hazards"))),
            "environment": str_or_empty(sn.and_then(|x| x.get("environment"))),
            "schools": str_or_empty(sn.and_then(|x| x.get("schools"))),
            "neighborhood": str_or_empty(sn.and_then(|x| x.get("neighborhood"))),
            "fengshui": str_or_empty(sn.and_then(|x| x.get("fengshui"))),
        },
        "outlook": {
            "oneYear": str_or_empty(ai.and_then(|a| a.pointer("/outlook/oneYear"))),
            "fiveYear": str_or_empty(ai.and_then(|a| a.pointer("/outlook/fiveYear"))),
        },
        "riskMatrix": risk_matrix,
        "fit": {
            "prioritize": arr_strings(ai.and_then(|a| a.pointer("/fit/prioritize")), None),
            "tradeoffs": arr_strings(ai.and_then(|a| a.pointer("/fit/tradeoffs")), None),
        },
        "negotiation": arr_strings(ai.and_then(|a| a.get("negotiation")), None),
        "sellerQuestions": arr_strings(ai.and_then(|a| a.get("sellerQuestions")), None),
        "decisionNotes": {
            "aduFeasibility": str_or_empty(df.and_then(|d| d.pointer("/lotAndAdu/feasibility"))),
            "aduNote": str_or_empty(df.and_then(|d| d.pointer("/lotAndAdu/note"))),
            "solarNote": str_or_empty(df.and_then(|d| d.pointer("/solar/note"))),
        }
    })
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Translates a Open Estate report's narrative, AI takeaways, risk matrix, and decision notes into `target_lang` (`"zh"` default).
pub async fn translate_report(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    target_lang: &str,
    refresh: bool,
    cache: Option<&dyn LlmCache>,
) -> Result<Value> {
    let mut out = translate_report_inner(http, env, report, target_lang, refresh, cache).await?;
    // Translations cached before the compass rule existed are served from three cache layers;
    // normalising the response covers all of them ("坐N朝S" -> 坐北朝南).
    let traditional = match out.get("lang").and_then(|l| l.as_str()) {
        Some("zh") => Some(false),
        Some("zh-tw") => Some(true),
        _ => None,
    };
    if let Some(traditional) = traditional {
        for ptr in ["/data", "/record/data"] {
            if let Some(v) = out.pointer_mut(ptr) {
                localize_compass_in_cjk_value(v, traditional);
            }
        }
    }
    Ok(out)
}

async fn translate_report_inner(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    target_lang: &str,
    refresh: bool,
    cache: Option<&dyn LlmCache>,
) -> Result<Value> {
    let t0 = Instant::now();
    let lang_cfg = resolve_lang(target_lang)?;
    let norm_lang = lang_cfg.norm_code;

    let has_address = report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !report.is_object() || !has_address {
        return Err(GuardrailError::new(
            "INVALID_REPORT_OBJECT",
            format!(
                "Translation is restricted to verified {} property reports.",
                env.brand_name()
            ),
            400,
        )
        .into());
    }

    let payload = extract_translatable_payload(report);
    let content_hash = sha256_hex(
        &serde_json::to_string(&json!({
            "v": 1,
            "targetLang": norm_lang,
            "payload": payload,
        }))
        .unwrap_or_default(),
    );

    // 1. Check embedded report translation cache first
    if !refresh {
        if let Some(existing) = report
            .pointer(&format!("/translations/{norm_lang}"))
            .or_else(|| (norm_lang == "zh").then(|| report.pointer("/translations/zh-CN")).flatten())
        {
            let hash_matches =
                existing.get("contentHash").and_then(|h| h.as_str()) == Some(&content_hash);
            let is_native = existing.get("native").and_then(|v| v.as_bool()) == Some(true);
            if (hash_matches || is_native)
                && existing.get("data").filter(|d| d.is_object()).is_some()
            {
                let mut clean_rec = existing.clone();
                if let Some(d) = clean_rec.get_mut("data") {
                    *d = crate::ai::analyst::sanitize_fair_housing(std::mem::replace(
                        d,
                        Value::Null,
                    ));
                }
                return Ok(json!({
                    "ok": true,
                    "lang": norm_lang,
                    "cached": true,
                    "data": clean_rec.get("data"),
                    "record": clean_rec,
                    "ms": t0.elapsed().as_millis() as u64,
                }));
            }
        }
    }

    // 2. Check L1 `moka` cache + optional L2 `LlmCache`
    let cache_key = format!("translate:v2:{norm_lang}:{content_hash}");
    if !refresh {
        if let Some(mut hit) = Cache::get(global_llm_cache(), &cache_key).await {
            if hit.get("data").filter(|d| d.is_object()).is_some() {
                if let Some(d) = hit.get_mut("data") {
                    *d = crate::ai::analyst::sanitize_fair_housing(std::mem::replace(
                        d,
                        Value::Null,
                    ));
                }
                return Ok(json!({
                    "ok": true,
                    "lang": norm_lang,
                    "cached": true,
                    "data": hit.get("data"),
                    "record": hit,
                    "ms": t0.elapsed().as_millis() as u64,
                }));
            }
        }
        if let Some(ext) = cache {
            if let Some(mut hit) = ext.get(&cache_key).await {
                if hit.get("data").filter(|d| d.is_object()).is_some() {
                    if let Some(d) = hit.get_mut("data") {
                        *d = crate::ai::analyst::sanitize_fair_housing(std::mem::replace(
                            d,
                            Value::Null,
                        ));
                    }
                    Cache::insert(global_llm_cache(), cache_key.clone(), hit.clone()).await;
                    return Ok(json!({
                        "ok": true,
                        "lang": norm_lang,
                        "cached": true,
                        "data": hit.get("data"),
                        "record": hit,
                        "ms": t0.elapsed().as_millis() as u64,
                    }));
                }
            }
        }
    }

    let mut translated_data: Option<Value> = None;
    let mut source_tag = String::new();
    let mut warnings: Vec<String> = Vec::new();

    // 3. OpenAI Responses API structured real-estate localization (ONLY when not already embedded / cached)
    if env.llm_configured() {
        let instructions = format!(
            "You are a professional multilingual US real-estate analyst localizing a {brand} property due-diligence report into {label}.\n\
             Rules:\n\
             1. Keep ALL dollar figures, percentages, distances, proper US street names, school names, and official acronyms (Prop 13, ADU, JADU, FEMA, SFHA, CAASPP, Superfund/NPL, AVM) accurate and clear — keep proper street/school names in English (or English + target-language note where helpful).\n\
             2. Use natural, idiomatic {label} suitable for homebuyers.\n\
             3. Keep riskMatrix[].likelihood and riskMatrix[].impact strictly as 'low' | 'medium' | 'high'. Preserve exact array lengths. Empty strings must remain \"\".\n\
             4. Localize compass directions and abbreviations into the target language (in Chinese: N/S/E/W/NE/NW/SE/SW become 北/南/东/西/东北/西北/东南/西南; a house sitting N and facing S is 坐北朝南 — never 坐N朝S. In Spanish: norte/sur/este/oeste/noreste/noroeste/sureste/suroeste).\n\
             5. NEVER mention other real-estate portals, brokerages, or their trademarks (Zillow, Zestimate, ZHVI, ZORI, Redfin, Walk Score, Realtor.com, Homes.com, Trulia, Movoto, Compass, Opendoor). Translate or replace any source references using ONLY: \"Third-party estimate\" / \"第三方估值\" / \"Estimación de terceros\" (never call a third-party figure \"True Estimate\" / \"真实估值\" / \"Estimación Real\"), \"ZIP home-value index\" / \"邮编房价指数\" / \"Índice de valor del código postal\", \"ZIP rent index\" / \"邮编租金指数\" / \"Índice de renta del código postal\", and \"Walkability score\" / \"步行便利指数\" / \"Índice de caminabilidad\".",
            label = lang_cfg.label,
            brand = env.brand_name()
        );

        let user_prompt = format!(
            "SOURCE_REPORT_STRINGS_JSON:\n{}",
            serde_json::to_string(&payload)?
        );

        let mut opts = LlmCallOptions::new(
            "report_translation",
            format!("openestate_translate_{norm_lang}"),
            instructions,
            user_prompt,
            translation_schema(),
        );
        let mut models = resolve_model_specs_with_effort(env, Some("low"));
        let base_model =
            parse_model_spec(env.get("LLM_MODEL").unwrap_or(DEFAULT_LLM_MODEL), None).model;
        if !base_model.is_empty() && !models.contains(&base_model) {
            models.push(base_model);
        }
        opts.models = models;
        opts.timeout_ms = Some(90_000);
        opts.refresh = refresh;

        let res = responses_json_with_cache(http, env, opts, cache).await?;
        translated_data = Some(res.data);
        source_tag = "llm".to_string();
    }

    if let Some(d) = translated_data.as_mut() {
        *d = crate::ai::analyst::sanitize_fair_housing(std::mem::replace(d, Value::Null));
        match norm_lang {
            "zh" => localize_compass_in_cjk_value(d, false),
            "zh-tw" => localize_compass_in_cjk_value(d, true),
            _ => {}
        }
    }

    let Some(data) = translated_data else {
        warnings.push(
            "OPENAI_API_KEY is not configured; UI labels and deterministic summaries switch languages client-side."
                .to_string(),
        );
        return Ok(json!({
            "ok": false,
            "lang": norm_lang,
            "cached": false,
            "data": Value::Null,
            "ms": t0.elapsed().as_millis() as u64,
            "warnings": warnings,
        }));
    };

    let record = json!({
        "lang": norm_lang,
        "contentHash": content_hash,
        "source": source_tag,
        "translatedAt": now_iso(),
        "data": data,
    });

    Cache::insert(global_llm_cache(), cache_key.clone(), record.clone()).await;
    if let Some(ext) = cache {
        ext.set(&cache_key, record.clone()).await;
    }

    Ok(json!({
        "ok": true,
        "lang": norm_lang,
        "cached": false,
        "data": record.get("data"),
        "record": record,
        "ms": t0.elapsed().as_millis() as u64,
        "warnings": warnings,
    }))
}
