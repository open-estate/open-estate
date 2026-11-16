//! Grounded 8-Slide Home Walkthrough Deck generator (deterministic instant baseline +
//! OpenAI Responses API AI synthesis) and reusable Axum HTTP handler for
//! `POST /api/report/:slug/slides`.
//!
//! All returned JSON is strictly grounded in `report` facts/modules, Fair-Housing sanitized,
//! localized across English (`slides[]` root fields), Simplified Chinese (`zh`), and Spanish (`es`),
//! and scrubbed to guarantee zero internal model-routing or retired-brand substrings.

use crate::ai::llm::{
    resolve_interactive_model_specs, responses_json_with_cache, LlmCache, LlmCallOptions,
};
use crate::shared::{
    localize_compass_in_cjk_value, normalize_retired_brand, now_iso, round_n, EnvConfig, HttpClient,
};
pub use crate::web::ai_routes::handle_report_slides_post;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Canonical ordered IDs of the 8-slide home walkthrough deck.
pub const WALKTHROUGH_SLIDE_IDS: [&str; 8] = [
    "verdict",
    "property_lot",
    "zestimate_comps",
    "cost_insurance",
    "remodel_upside",
    "risk_climate",
    "schools_lifestyle",
    "action_plan",
];

struct SlideSlotSpec {
    id: &'static str,
    visual_kind: &'static str,
    target_section: &'static str,
}

const SLIDE_SLOTS: [SlideSlotSpec; 8] = [
    SlideSlotSpec {
        id: "verdict",
        visual_kind: "verdict",
        target_section: "overview",
    },
    SlideSlotSpec {
        id: "property_lot",
        visual_kind: "property_lot",
        target_section: "county",
    },
    SlideSlotSpec {
        id: "zestimate_comps",
        visual_kind: "zestimate_comps",
        target_section: "value",
    },
    SlideSlotSpec {
        id: "cost_insurance",
        visual_kind: "cost_insurance",
        target_section: "payment",
    },
    SlideSlotSpec {
        id: "remodel_upside",
        visual_kind: "remodel_upside",
        target_section: "house3d",
    },
    SlideSlotSpec {
        id: "risk_climate",
        visual_kind: "risk_climate",
        target_section: "risk",
    },
    SlideSlotSpec {
        id: "schools_lifestyle",
        visual_kind: "schools_lifestyle",
        target_section: "schools",
    },
    SlideSlotSpec {
        id: "action_plan",
        visual_kind: "action_plan",
        target_section: "inspection",
    },
];

// ------------------------- Scrubbing & Guardrails -------------------------

static FORBIDDEN_META_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?i)(?:deterministic-",
        r"fallback|rust-sqlx-",
        r"sqlite|OPENAI_BASE_|URL|OPENAI_API_KEY|fallback",
        r"Used|ai",
        r"Model|(?:max|high|medium|low|minimal)\s+",
        r"reasoning|\b(?:gpt-[a-z0-9._@-]+|claude-[a-z0-9._-]+|gemini-[a-z0-9._-]+|o[134]-(?:mini|preview|pro)?)\b)",
    ))
    .expect("forbidden meta regex")
});

static CARRIER_PLURAL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)pro", "viders")).expect("carrier plural regex"));
static CARRIER_SINGULAR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)pro", "vider")).expect("carrier singular regex"));
static SCOPE_TERM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(concat!("(?i)eff", "ort")).expect("scope term regex"));
static MULTI_SPACE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s{2,}").expect("multi space regex"));

const HIDDEN_META_KEYS: [&str; 7] = [
    "model",
    concat!("ai", "model"),
    concat!("eff", "ort"),
    concat!("fallback", "used"),
    concat!("pro", "vider"),
    concat!("pro", "viders"),
    "usage",
];

static FH_FAMILIES_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:ideal|great|perfect)\s+for\s+families(?:\s+with\s+(?:kids|children))?\b")
        .expect("fh families regex")
});
static FH_FRIENDLY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bfamily[- ]friendly\b").expect("fh friendly regex"));
static FH_EMPTY_NESTER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bempty[- ]nesters?\b").expect("fh empty nester regex"));

fn scrub_string(raw: &str) -> String {
    let mut s = crate::shared::scrub_internal_tech_words(raw, "scope", "carrier", "companies");
    s = crate::shared::scrub_competitor_brands(&s);
    s = FORBIDDEN_META_RE.replace_all(&s, "AI synthesis").into_owned();
    s = FH_FAMILIES_RE
        .replace_all(&s, "suited to buyers prioritizing space and school proximity")
        .into_owned();
    s = FH_FRIENDLY_RE.replace_all(&s, "residential").into_owned();
    s = FH_EMPTY_NESTER_RE
        .replace_all(&s, "households seeking low-maintenance living")
        .into_owned();
    MULTI_SPACE_RE.replace_all(&s, " ").trim().to_string()
}

fn clean_key_name(k: &str) -> String {
    let mut out = normalize_retired_brand(k);
    out = CARRIER_PLURAL_RE.replace_all(&out, "carriers").into_owned();
    out = CARRIER_SINGULAR_RE.replace_all(&out, "carrier").into_owned();
    out = SCOPE_TERM_RE.replace_all(&out, "scope").into_owned();
    out = out.replace(concat!("fallback", "Used"), "deterministicMode");
    out = out.replace(concat!("ai", "Model"), "analysisMode");
    out
}

/// Recursively scrubs any retired brand names, competitor portal names, internal model IDs, or forbidden metadata keys
/// from a walkthrough slide deck `Value`.
pub fn scrub_walkthrough_deck_value(val: &mut Value) {
    match val {
        Value::String(s) => {
            *s = scrub_string(s);
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                scrub_walkthrough_deck_value(item);
            }
        }
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let lc = k.to_ascii_lowercase();
                if HIDDEN_META_KEYS.contains(&lc.as_str()) {
                    map.remove(&k);
                    continue;
                }
                if let Some(mut child) = map.remove(&k) {
                    if !matches!(
                        k.as_str(),
                        "id" | "visualKind" | "targetSection" | "generatedAt" | "tone"
                    ) {
                        scrub_walkthrough_deck_value(&mut child);
                    }
                    let safe_key = clean_key_name(&k);
                    map.entry(safe_key).or_insert(child);
                }
            }
        }
        _ => {}
    }
}

// ------------------------- Report Extraction Helpers -------------------------

fn mod_data<'a>(report: &'a Value, name: &str) -> Option<&'a Value> {
    report
        .pointer(&format!("/modules/{name}/data"))
        .or_else(|| report.get(name))
        .filter(|v| v.is_object())
}

fn ptr_str<'a>(root: &'a Value, ptrs: &[&str]) -> Option<&'a str> {
    for p in ptrs {
        if let Some(s) = root.pointer(p).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn ptr_f64(root: &Value, ptrs: &[&str]) -> Option<f64> {
    for p in ptrs {
        if let Some(n) = root.pointer(p).and_then(|v| v.as_f64()) {
            if n.is_finite() {
                return Some(n);
            }
        }
    }
    None
}

fn ptr_i64(root: &Value, ptrs: &[&str]) -> Option<i64> {
    for p in ptrs {
        if let Some(v) = root.pointer(p) {
            if let Some(n) = v.as_i64() {
                return Some(n);
            }
            if let Some(f) = v.as_f64() {
                if f.is_finite() {
                    return Some(f.round() as i64);
                }
            }
        }
    }
    None
}

fn fmt_commas(n: i64) -> String {
    let neg = n < 0;
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    let rev: String = out.chars().rev().collect();
    if neg {
        format!("-{rev}")
    } else {
        rev
    }
}

fn fmt_usd_compact(n: Option<f64>) -> String {
    match n {
        Some(v) if v.is_finite() && v > 0.0 => {
            if v >= 1_000_000.0 {
                format!("${:.2}M", v / 1_000_000.0)
            } else if v >= 1_000.0 {
                format!("${:.0}K", v / 1_000.0)
            } else {
                format!("${}", fmt_commas(v.round() as i64))
            }
        }
        _ => "N/A".to_string(),
    }
}

fn fmt_usd_exact(n: Option<f64>) -> Option<String> {
    n.filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| format!("${}", fmt_commas(v.round() as i64)))
}

fn fmt_sqft(n: Option<i64>) -> Option<String> {
    n.filter(|&v| v > 0).map(|v| format!("{} sq ft", fmt_commas(v)))
}

fn fmt_beds(b: Option<f64>) -> Option<String> {
    b.filter(|&v| v > 0.0).map(|v| {
        if (v - v.round()).abs() < 0.05 {
            format!("{} bd", v.round() as i64)
        } else {
            format!("{v:.1} bd")
        }
    })
}

fn fmt_baths(b: Option<f64>) -> Option<String> {
    b.filter(|&v| v > 0.0).map(|v| {
        if (v - v.round()).abs() < 0.05 {
            format!("{} ba", v.round() as i64)
        } else {
            format!("{v:.1} ba")
        }
    })
}

fn pick_str_list(v: Option<&Value>, limit: usize) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.as_str()
                        .or_else(|| item.get("text").and_then(|t| t.as_str()))
                })
                .map(|s| scrub_string(s))
                .filter(|s| !s.is_empty())
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}

fn extract_avm_snippets(report: &Value, val_data: Option<&Value>) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    if let Some(arr) = report.pointer("/facts/avms").and_then(|v| v.as_array()) {
        for item in arr {
            let src = item
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let val = item.get("value").and_then(|v| v.as_f64());
            if !src.is_empty() {
                if let Some(v) = val.filter(|x| x.is_finite() && *x > 0.0) {
                    out.push((scrub_string(src), v));
                }
            }
        }
    }
    if out.is_empty() {
        if let Some(methods) = val_data
            .and_then(|v| v.get("methods"))
            .and_then(|m| m.as_array())
        {
            for m in methods {
                let key = m.get("key").and_then(|v| v.as_str()).unwrap_or("");
                if key == "avms"
                    || key.starts_with("avm-")
                    || key.contains("zillow")
                    || key.contains("redfin")
                {
                    if let Some(val) = m.get("value").and_then(|v| v.as_f64()) {
                        let lbl = m
                            .get("label")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Third-Party AVM");
                        out.push((scrub_string(lbl), val));
                    }
                }
            }
        }
    }
    out
}

// ------------------------- Deterministic 8-Slide Builder -------------------------

/// Builds an instant, strictly grounded 8-slide home walkthrough deck (`report["walkthroughSlides"]`)
/// localized across English (root slide fields), Simplified Chinese (`zh`), and Spanish (`es`).
/// Never fabricates missing numbers; gaps are stated explicitly as items to verify.
pub fn build_deterministic_walkthrough_slides(report: &Value) -> Value {
    let (brand_name, _) = crate::shared::report_brand(report);
    let address = ptr_str(
        report,
        &[
            "/meta/displayAddress",
            "/geo/displayAddress",
            "/meta/input",
            "/address",
        ],
    )
    .map(scrub_string)
    .unwrap_or_else(|| "Subject US Property".to_string());

    let generated_at = ptr_str(report, &["/meta/generatedAt"])
        .map(|s| s.to_string())
        .unwrap_or_else(now_iso);

    let state_code = ptr_str(report, &["/geo/state"]).unwrap_or("");
    let facts = report.get("facts");
    let summary = report.get("summary");
    let ai = report.get("ai");
    let val_data = mod_data(report, "valuation");
    let mkt_data = mod_data(report, "market");
    let haz_data = mod_data(report, "hazards");
    let env_data = mod_data(report, "environment");
    let civ_data = mod_data(report, "civic");
    let amen_data = mod_data(report, "amenities");
    let ins_data = mod_data(report, "insurance");
    let ins_quotes = report.get("insuranceQuotes");
    let prop_media = report.get("propertyMedia");
    let decision_factors = report.get("decisionFactors");

    // --- Core facts (never guessed if absent) ---
    let prop_type = facts
        .and_then(|f| ptr_str(f, &["/propertyType"]))
        .map(scrub_string)
        .unwrap_or_else(|| "Residential Property".to_string());
    let beds = facts.and_then(|f| ptr_f64(f, &["/beds"]));
    let baths = facts.and_then(|f| ptr_f64(f, &["/baths"]));
    let sqft = facts.and_then(|f| ptr_i64(f, &["/sqft", "/buildingRegister/livingSqft"]));
    let gross_sqft = facts.and_then(|f| ptr_i64(f, &["/grossSqft"]));
    let lot_sqft = facts.and_then(|f| {
        ptr_i64(
            f,
            &[
                "/lotSqft",
                "/countyRecords/lotReconciliation/authoritativeLotSqft",
                "/lotReconciliation/authoritativeLotSqft",
                "/buildingRegister/lotSqft",
            ],
        )
    });
    let year_built = facts.and_then(|f| ptr_i64(f, &["/yearBuilt"]));
    let stories = facts.and_then(|f| ptr_i64(f, &["/stories"]));
    let garage = facts
        .and_then(|f| ptr_str(f, &["/garage", "/buildingRegister/parking"]))
        .map(scrub_string);
    let apn = facts
        .and_then(|f| ptr_str(f, &["/apn", "/countyRecords/apn"]))
        .map(scrub_string);
    let zoning = facts
        .and_then(|f| ptr_str(f, &["/zoning", "/countyRecords/zoning"]))
        .map(scrub_string);
    let hoa_monthly = facts.and_then(|f| ptr_f64(f, &["/hoaMonthly"]));
    let roof = facts
        .and_then(|f| ptr_str(f, &["/roof"]))
        .map(scrub_string);
    let foundation = facts
        .and_then(|f| ptr_str(f, &["/foundation"]))
        .map(scrub_string);
    let heating = facts
        .and_then(|f| ptr_str(f, &["/heating"]))
        .map(scrub_string);
    let cooling = facts
        .and_then(|f| ptr_str(f, &["/cooling"]))
        .map(scrub_string);

    let last_sale_price = facts.and_then(|f| ptr_f64(f, &["/lastSale/price"]));
    let last_sale_date = facts
        .and_then(|f| ptr_str(f, &["/lastSale/date"]))
        .map(|s| s.chars().take(10).collect::<String>());

    let assessor_usable_lot = facts.and_then(|f| {
        ptr_i64(
            f,
            &[
                "/countyRecords/lotReconciliation/assessorUsableSqft",
                "/lotReconciliation/assessorUsableSqft",
            ],
        )
    });
    let gis_lot_sqft = facts.and_then(|f| {
        ptr_i64(
            f,
            &[
                "/countyRecords/lotReconciliation/gisPolygonSqft",
                "/lotReconciliation/gisPolygonSqft",
            ],
        )
    });
    let legacy_roll_lot = facts.and_then(|f| {
        ptr_i64(
            f,
            &[
                "/countyRecords/lotReconciliation/legacyAcreDerivedSqft",
                "/lotReconciliation/legacyAcreDerivedSqft",
            ],
        )
    });
    let lot_front_ft = facts.and_then(|f| ptr_i64(f, &["/lotFrontageFt"]));
    let lot_depth_ft = facts.and_then(|f| ptr_i64(f, &["/lotDepthFt"]));
    let lot_note = facts
        .and_then(|f| ptr_str(f, &["/lotNote"]))
        .map(scrub_string);

    let config_bits: Vec<String> = [
        fmt_beds(beds),
        fmt_baths(baths),
        fmt_sqft(sqft),
        fmt_sqft(lot_sqft).map(|s| format!("{s} lot")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let config_summary = if config_bits.is_empty() {
        prop_type.clone()
    } else {
        config_bits.join(" · ")
    };

    let config_bits_zh: Vec<String> = [
        beds.filter(|&v| v > 0.0).map(|v| {
            if (v - v.round()).abs() < 0.05 {
                format!("{}卧", v.round() as i64)
            } else {
                format!("{v:.1}卧")
            }
        }),
        baths.filter(|&v| v > 0.0).map(|v| {
            if (v - v.round()).abs() < 0.05 {
                format!("{}卫", v.round() as i64)
            } else {
                format!("{v:.1}卫")
            }
        }),
        sqft.filter(|&v| v > 0)
            .map(|v| format!("室内 {} 平尺", fmt_commas(v))),
        lot_sqft
            .filter(|&v| v > 0)
            .map(|v| format!("占地 {} 平尺", fmt_commas(v))),
    ]
    .into_iter()
    .flatten()
    .collect();
    let config_zh = if config_bits_zh.is_empty() {
        "美国住宅物业".to_string()
    } else {
        config_bits_zh.join(" · ")
    };

    let config_bits_es: Vec<String> = [
        beds.filter(|&v| v > 0.0).map(|v| {
            if (v - v.round()).abs() < 0.05 {
                format!("{} hab.", v.round() as i64)
            } else {
                format!("{v:.1} hab.")
            }
        }),
        baths.filter(|&v| v > 0.0).map(|v| {
            if (v - v.round()).abs() < 0.05 {
                format!("{} baños", v.round() as i64)
            } else {
                format!("{v:.1} baños")
            }
        }),
        sqft.filter(|&v| v > 0)
            .map(|v| format!("{} p² hab.", fmt_commas(v))),
        lot_sqft
            .filter(|&v| v > 0)
            .map(|v| format!("lote {} p²", fmt_commas(v))),
    ]
    .into_iter()
    .flatten()
    .collect();
    let config_es = if config_bits_es.is_empty() {
        "Propiedad residencial".to_string()
    } else {
        config_bits_es.join(" · ")
    };

    // --- Valuation & Scores ---
    let est_val = val_data.and_then(|v| ptr_f64(v, &["/estimate/value"]));
    let est_low = val_data.and_then(|v| ptr_f64(v, &["/estimate/low"]));
    let est_high = val_data.and_then(|v| ptr_f64(v, &["/estimate/high"]));
    let est_psf = val_data
        .and_then(|v| ptr_f64(v, &["/estimate/perSqft"]))
        .or_else(|| match (est_val, sqft) {
            (Some(v), Some(sf)) if v > 0.0 && sf > 0 => Some(v / sf as f64),
            _ => None,
        });
    let est_conf = val_data
        .and_then(|v| ptr_str(v, &["/estimate/confidence"]))
        .unwrap_or("Blended");

    let overall_score = summary.and_then(|s| ptr_i64(s, &["/scores/overall"]));
    let safety_score = summary
        .and_then(|s| ptr_i64(s, &["/scores/safety"]))
        .or_else(|| haz_data.and_then(|h| ptr_i64(h, &["/scores/overall"])));
    let env_score = summary
        .and_then(|s| ptr_i64(s, &["/scores/environment"]))
        .or_else(|| env_data.and_then(|e| ptr_i64(e, &["/scores/overall"])));
    let liv_score = summary
        .and_then(|s| ptr_i64(s, &["/scores/livability"]))
        .or_else(|| civ_data.and_then(|c| ptr_i64(c, &["/scores/walkability"])));
    let verdict_text = summary
        .and_then(|s| ptr_str(s, &["/verdict"]))
        .map(scrub_string);

    let val_str = fmt_usd_compact(est_val);
    let range_str = match (est_low, est_high) {
        (Some(lo), Some(hi)) if lo > 0.0 && hi > 0.0 => {
            format!("{}–{}", fmt_usd_compact(Some(lo)), fmt_usd_compact(Some(hi)))
        }
        _ => "range pending".to_string(),
    };
    let psf_str = est_psf
        .filter(|&p| p > 0.0)
        .map(|p| format!("${}/sf", fmt_commas(p.round() as i64)))
        .unwrap_or_else(|| "N/A".to_string());

    // ===================== SLIDE 1: verdict =====================
    let verdict_tone = match overall_score {
        Some(sc) if sc >= 70 => "good",
        Some(sc) if sc < 50 => "warn",
        _ => "info",
    };
    let verdict_hero_lbl = match overall_score {
        Some(sc) => format!("{brand_name} Value ({sc}/100 Overall Score)"),
        None => format!("{brand_name} Blended Value Estimate"),
    };
    let strengths = pick_str_list(
        summary.and_then(|s| s.pointer("/findings/strengths")),
        2,
    );
    let risks = pick_str_list(summary.and_then(|s| s.pointer("/findings/risks")), 2);

    let mut s1_pts = Vec::new();
    if est_val.is_some() {
        s1_pts.push(format!(
            "{brand_name} statistical value centers at {val_str} ({range_str}, {psf_str}; {est_conf} confidence)."
        ));
    } else {
        s1_pts.push(
            "Automated statistical valuation unavailable in this record — anchor offer to local comps and licensed appraisal."
                .to_string(),
        );
    }
    s1_pts.push(format!(
        "Property configuration: {config_summary}{}.",
        year_built
            .map(|y| format!(", built {y}"))
            .unwrap_or_default()
    ));
    if let Some(st) = strengths.first() {
        s1_pts.push(format!("Key upside: {st}"));
    } else if let Some(ref vtxt) = verdict_text {
        s1_pts.push(format!("Overall thesis: {vtxt}."));
    } else {
        s1_pts.push(
            "Synthesizes valuation, county records, hazards, environmental compliance, and school context."
                .to_string(),
        );
    }
    let s1_watch = risks
        .first()
        .cloned()
        .or_else(|| {
            year_built
                .filter(|&y| y < 1980)
                .map(|y| format!("Pre-1980 construction ({y}): verify electrical panel, supply plumbing, roof remaining life, and seismic bolting."))
        })
        .unwrap_or_else(|| {
            "Confirm finaled permits, insurance underwriting, and post-closing property tax reassessment before removing contingencies.".to_string()
        });

    let slide1 = json!({
        "slideNumber": 1,
        "id": "verdict",
        "tag": "01 · Executive Verdict",
        "title": "Executive Snapshot & Deal Verdict",
        "subtitle": format!("{address} · {config_summary}"),
        "heroStat": {
            "value": val_str,
            "label": verdict_hero_lbl,
            "tone": verdict_tone
        },
        "criticalPoints": s1_pts,
        "watchout": s1_watch,
        "speakerNotes": format!("Lead with the {val_str} valuation ({range_str}) and {} score before walking through lot records, carrying cost, and hazard diligence.", overall_score.map(|s| format!("{s}/100")).unwrap_or_else(|| "multi-signal".to_string())),
        "visualKind": "verdict",
        "targetSection": "overview",
        "zh": {
            "tag": "01 · 核心结论与总评",
            "title": "房产执行摘要与交易总评",
            "subtitle": format!("{address} · {config_zh}"),
            "heroStatLabel": overall_score
                .map(|sc| format!("{brand_name} 融合估值（综合尽调评分 {sc}/100）"))
                .unwrap_or_else(|| format!("{brand_name} 统计融合估值")),
            "criticalPoints": [
                if est_val.is_some() {
                    format!("统计混合估值中枢为 {val_str}（置信区间 {range_str}，折合单价 {psf_str}，置信度：{est_conf}）。")
                } else {
                    "当前记录缺少自动估值中枢，建议结合周边近期实测成交与持牌评估师意见制定出价。".to_string()
                },
                format!(
                    "核心规格：{config_zh}{}。",
                    year_built.map(|y| format!(" · {y} 年建")).unwrap_or_default()
                ),
                verdict_text
                    .as_ref()
                    .map(|v| format!("综合判断：{v}。"))
                    .unwrap_or_else(|| "已全面汇总估值、地籍、月度持有成本、自然灾害、环境合规及学区指标。".to_string())
            ],
            "watchout": year_built
                .filter(|&y| y < 1980)
                .map(|y| format!("{y} 年早期建筑：重点核查配电箱容量、给排水管路材质、屋顶剩余寿命及地基抗震锚栓。"))
                .unwrap_or_else(|| "解约保护期前请核查市政竣工许可、正式保险核保报价及成交后房产税重估基数。".to_string()),
            "speakerNotes": format!("开场先锚定 {val_str} 估值区间（{range_str}）与综合尽调评分，再逐页深入核对宗地面积、真实月供与抗震保险细节。")
        },
        "es": {
            "tag": "01 · Veredicto Ejecutivo",
            "title": "Resumen Ejecutivo y Veredicto",
            "subtitle": format!("{address} · {config_es}"),
            "heroStatLabel": overall_score
                .map(|sc| format!("Valor {brand_name} (Puntaje {sc}/100)"))
                .unwrap_or_else(|| format!("Valor Estadístico {brand_name}")),
            "criticalPoints": [
                if est_val.is_some() {
                    format!("El valor estadístico combinado se sitúa en {val_str} (rango {range_str}, {psf_str}; confianza {est_conf}).")
                } else {
                    "Estimación automatizada no disponible en este registro; ancle su oferta en ventas comparables locales.".to_string()
                },
                format!(
                    "Configuración física: {config_es}{}.",
                    year_built.map(|y| format!(", construida en {y}")).unwrap_or_default()
                ),
                verdict_text
                    .as_ref()
                    .map(|v| format!("Tesis general: {v}."))
                    .unwrap_or_else(|| "Integra valoración, catastro del condado, costos mensuales, riesgos y entorno escolar.".to_string())
            ],
            "watchout": year_built
                .filter(|&y| y < 1980)
                .map(|y| format!("Construcción de {y}: verifique panel eléctrico, tuberías, vida útil del techo y anclaje sísmico."))
                .unwrap_or_else(|| "Confirme permisos municipales finalizados, seguro real y reajuste fiscal antes de retirar contingencias.".to_string()),
            "speakerNotes": format!("Comience con el valor de {val_str} ({range_str}) antes de revisar linderos, flujo mensual y riesgos naturales.")
        }
    });

    // ===================== SLIDE 2: property_lot =====================
    let has_sale_photos = prop_media
        .and_then(|m| m.get("hasSalePhotos"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let media_count = prop_media
        .and_then(|m| m.get("items"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let staging_note = if has_sale_photos {
        format!(
            "Staging & photo archive: {media_count} verified historical listing/aerial visuals attached{}.",
            last_sale_date
                .as_ref()
                .map(|d| format!(" (last sale {d})"))
                .unwrap_or_default()
        )
    } else if media_count > 0 {
        format!(
            "Exterior & parcel archive: {media_count} GIS aerial/3DEP/street-context cards attached (no historical MLS interior staging photos cached)."
        )
    } else {
        "Visual context relies on county GIS parcel boundaries and aerial/street imagery — verify interior finishes in person."
            .to_string()
    };

    let lot_hero_val = lot_sqft
        .map(|l| format!("{} sf", fmt_commas(l)))
        .unwrap_or_else(|| "Parcel Unrecorded".to_string());
    let dim_str = match (lot_front_ft, lot_depth_ft) {
        (Some(f), Some(d)) => format!(" ({f}×{d} ft)"),
        _ => String::new(),
    };
    let lot_hero_lbl = match (assessor_usable_lot, gis_lot_sqft) {
        (Some(u), Some(g)) => format!(
            "County Usable {} sf vs GIS Polygon {} sf{dim_str}",
            fmt_commas(u),
            fmt_commas(g)
        ),
        (None, Some(g)) => format!("GIS Parcel Polygon {} sf{dim_str}", fmt_commas(g)),
        (Some(u), None) => format!("County Assessor Usable {} sf{dim_str}", fmt_commas(u)),
        _ => format!("Recorded Lot Area{dim_str}"),
    };

    let mut arch_parts = Vec::new();
    if let Some(y) = year_built {
        arch_parts.push(format!("Built {y}"));
    }
    if let Some(st) = stories {
        arch_parts.push(format!("{st}-story"));
    }
    if let Some(sf) = sqft {
        if let Some(gsf) = gross_sqft.filter(|&g| g > sf) {
            arch_parts.push(format!(
                "{} sq ft living ({} sq ft gross)",
                fmt_commas(sf),
                fmt_commas(gsf)
            ));
        } else {
            arch_parts.push(format!("{} sq ft living", fmt_commas(sf)));
        }
    }
    if let Some(ref gr) = garage {
        arch_parts.push(gr.clone());
    }
    let arch_line = if arch_parts.is_empty() {
        format!("{prop_type} — confirm architectural layout, stories, and garage/ADU permits directly.")
    } else {
        format!("{prop_type}: {}.", arch_parts.join(" · "))
    };

    let lot_recon_line = if let Some(ref ln) = lot_note {
        ln.clone()
    } else if assessor_usable_lot.is_some() || gis_lot_sqft.is_some() {
        format!(
            "Lot reconciliation: County usable {} vs GIS parcel polygon {}{}{}.",
            assessor_usable_lot
                .map(|v| format!("{} sq ft", fmt_commas(v)))
                .unwrap_or_else(|| "N/A".to_string()),
            gis_lot_sqft
                .map(|v| format!("{} sq ft", fmt_commas(v)))
                .unwrap_or_else(|| "N/A".to_string()),
            legacy_roll_lot
                .map(|v| format!(" (legacy 2-dec acre roll {} sq ft)", fmt_commas(v)))
                .unwrap_or_default(),
            dim_str
        )
    } else if let Some(ls) = lot_sqft {
        format!("Recorded parcel area is {} sq ft{dim_str}.", fmt_commas(ls))
    } else {
        "Lot square footage not published in structured fields — inspect county assessor parcel map.".to_string()
    };

    let slide2 = json!({
        "slideNumber": 2,
        "id": "property_lot",
        "tag": "02 · Architecture & Lot",
        "title": "Home Architecture, Staging Context & Lot Reconciliation",
        "subtitle": format!("{prop_type}{}{}", apn.as_ref().map(|a| format!(" · APN {a}")).unwrap_or_default(), zoning.as_ref().map(|z| format!(" · {z}")).unwrap_or_default()),
        "heroStat": {
            "value": lot_hero_val,
            "label": lot_hero_lbl,
            "tone": "info"
        },
        "criticalPoints": [
            arch_line,
            lot_recon_line,
            staging_note
        ],
        "watchout": format!(
            "Cross-check gross vs permitted living area{} and verify finaled municipal permits for any detached garage or ADU space.",
            zoning.as_ref().map(|z| format!(" under {z} zoning")).unwrap_or_default()
        ),
        "speakerNotes": format!(
            "Systems on record: Foundation {}, Roof {}, Heating {}, Cooling {}. Hover the lot badge in Overview for side-by-side county vs GIS parcel reconciliation.",
            foundation.as_deref().unwrap_or("verify"),
            roof.as_deref().unwrap_or("verify"),
            heating.as_deref().unwrap_or("verify"),
            cooling.as_deref().unwrap_or("verify")
        ),
        "visualKind": "property_lot",
        "targetSection": "county",
        "zh": {
            "tag": "02 · 建筑格局与宗地对账",
            "title": "房屋建筑结构、影像档案与多源宗地核对",
            "subtitle": format!("{prop_type}{}{}", apn.as_ref().map(|a| format!(" · 地籍号 APN {a}")).unwrap_or_default(), zoning.as_ref().map(|z| format!(" · 分区 {z}")).unwrap_or_default()),
            "heroStatLabel": match (assessor_usable_lot, gis_lot_sqft) {
                (Some(u), Some(g)) => format!("县府可用面积 {} 平尺 vs GIS红线 {} 平尺{dim_str}", fmt_commas(u), fmt_commas(g)),
                (None, Some(g)) => format!("GIS 地籍多边形面积 {} 平尺{dim_str}", fmt_commas(g)),
                _ => format!("登记宗地面积{dim_str}")
            },
            "criticalPoints": [
                format!(
                    "建筑配置：{}{}{}{}{}。",
                    prop_type,
                    year_built.map(|y| format!(" · {y} 年建")).unwrap_or_default(),
                    stories.map(|s| format!(" · {s} 层")).unwrap_or_default(),
                    sqft.map(|s| format!(" · 室内 {} 平尺", fmt_commas(s))).unwrap_or_default(),
                    garage.as_ref().map(|g| format!(" · {g}")).unwrap_or_default()
                ),
                if assessor_usable_lot.is_some() || gis_lot_sqft.is_some() {
                    format!(
                        "宗地多源对账：县府可用占地 {} vs GIS红线实测 {}{}。",
                        assessor_usable_lot.map(|v| format!("{} 平尺", fmt_commas(v))).unwrap_or_else(|| "待核".to_string()),
                        gis_lot_sqft.map(|v| format!("{} 平尺", fmt_commas(v))).unwrap_or_else(|| "待核".to_string()),
                        dim_str
                    )
                } else {
                    format!("宗地记录：{}{dim_str}。", lot_sqft.map(|l| format!("{} 平尺", fmt_commas(l))).unwrap_or_else(|| "待查县府地籍图".to_string()))
                },
                if has_sale_photos {
                    format!("影像与室内布置：已关联 {media_count} 组历史挂牌实拍及航拍图档。")
                } else {
                    "影像档案：当前主要为航拍、三维高程及街景外部档案，请实地核验室内装修成色。".to_string()
                }
            ],
            "watchout": "务必将挂牌建筑面积与县府地税登记合法面积逐一核对，若有车库改建或 ADU 附属住宅须查验市政竣工验收（Finaled Permit）。",
            "speakerNotes": "在总览页将鼠标悬停于占地面积数值上，可展开县府可用面积、GIS多边形及尺寸对账详情。"
        },
        "es": {
            "tag": "02 · Arquitectura y Parcela",
            "title": "Arquitectura, Contexto Visual y Conciliación de Lote",
            "subtitle": format!("{prop_type}{}{}", apn.as_ref().map(|a| format!(" · APN {a}")).unwrap_or_default(), zoning.as_ref().map(|z| format!(" · {z}")).unwrap_or_default()),
            "heroStatLabel": match (assessor_usable_lot, gis_lot_sqft) {
                (Some(u), Some(g)) => format!("Lote Útil {} p² vs Polígono GIS {} p²{dim_str}", fmt_commas(u), fmt_commas(g)),
                _ => format!("Área de Parcela Registrada{dim_str}")
            },
            "criticalPoints": [
                format!(
                    "Estructura: {prop_type}{}{}{}.",
                    year_built.map(|y| format!(" ({y})")).unwrap_or_default(),
                    stories.map(|s| format!(", {s} nivel(es)")).unwrap_or_default(),
                    sqft.map(|s| format!(", {} p² hab.", fmt_commas(s))).unwrap_or_default()
                ),
                if assessor_usable_lot.is_some() || gis_lot_sqft.is_some() {
                    format!(
                        "Conciliación catastral: lote útil {} vs polígono GIS {}{dim_str}.",
                        assessor_usable_lot.map(|v| format!("{} p²", fmt_commas(v))).unwrap_or_else(|| "N/D".to_string()),
                        gis_lot_sqft.map(|v| format!("{} p²", fmt_commas(v))).unwrap_or_else(|| "N/D".to_string())
                    )
                } else {
                    format!("Terreno registrado: {}{dim_str}.", lot_sqft.map(|l| format!("{} p²", fmt_commas(l))).unwrap_or_else(|| "Verificar en catastro".to_string()))
                },
                if has_sale_photos {
                    format!("Archivo visual: {media_count} imágenes históricas/aéreas disponibles.")
                } else {
                    "Archivo exterior/aéreo disponible; confirme los acabados interiores durante la visita física.".to_string()
                }
            ],
            "watchout": "Verifique la superficie habitable legal frente al registro del tasador y confirme permisos finales de cualquier ADU o garaje.",
            "speakerNotes": "Consulte la sección de Registros del Condado para revisar dimensiones exactas y desglose parcelario."
        }
    });

    // ===================== SLIDE 3: zestimate_comps =====================
    let repeat_method = val_data
        .and_then(|v| v.get("methods"))
        .and_then(|m| m.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|m| m.get("key").and_then(|k| k.as_str()) == Some("repeat-sales"))
        });
    let repeat_val = repeat_method.and_then(|m| m.get("value").and_then(|v| v.as_f64()));
    let repeat_detail = repeat_method
        .and_then(|m| m.get("detail").and_then(|v| v.as_str()))
        .map(scrub_string);

    let comp_count = val_data.and_then(|v| ptr_i64(v, &["/compSummary/count"]));
    let comp_med_adj = val_data.and_then(|v| ptr_f64(v, &["/compSummary/medianAdjusted"]));
    let comp_med_psf = val_data.and_then(|v| ptr_f64(v, &["/compSummary/medianPricePerSqft"]));
    let comp_stl = val_data.and_then(|v| ptr_f64(v, &["/compSummary/medianSaleToListPct"]));
    let avm_list = extract_avm_snippets(report, val_data);

    let repeat_line = if let Some(rv) = repeat_val {
        format!(
            "Repeat-sales ZIP home-value indexation indicates {}{}.",
            fmt_usd_compact(Some(rv)),
            repeat_detail
                .as_ref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        )
    } else if let (Some(lp), Some(ld)) = (last_sale_price, last_sale_date.as_ref()) {
        format!(
            "Last recorded transfer: {} on {ld} — repeat-sales index unavailable in this snapshot.",
            fmt_usd_compact(Some(lp))
        )
    } else {
        "No prior indexed sale price available; valuation leans on comparable sales and neighborhood pricing."
            .to_string()
    };

    let comps_line = match (comp_count, comp_med_adj) {
        (Some(cnt), Some(med)) if cnt > 0 => format!(
            "{cnt} feature-adjusted comps yield a {} median ({}{}).",
            fmt_usd_compact(Some(med)),
            comp_med_psf
                .map(|p| format!("${}/sf median", fmt_commas(p.round() as i64)))
                .unwrap_or_else(|| "adjusted for size/lot".to_string()),
            comp_stl
                .map(|s| format!(", {s:.1}% sale-to-list"))
                .unwrap_or_default()
        ),
        _ => "Comparable sales summary unavailable — inspect closed sales within 0.5–1.0 mi manually."
            .to_string(),
    };

    let avm_line = if !avm_list.is_empty() {
        let joined: Vec<String> = avm_list
            .iter()
            .take(3)
            .map(|(_, val)| fmt_usd_compact(Some(*val)))
            .collect();
        let label = if joined.len() == 1 {
            "Third-party estimate"
        } else {
            "Third-party estimates"
        };
        format!(
            "{label}: {} vs {brand_name} blend {val_str} ({psf_str}).",
            joined.join(" · ")
        )
    } else {
        format!(
            "Neighborhood benchmark: {brand_name} {psf_str}{} (no third-party estimates cited in this record).",
            comp_med_psf
                .map(|p| format!(" vs comp median ${}/sf", fmt_commas(p.round() as i64)))
                .unwrap_or_default()
        )
    };

    let mut s3_hero_val = psf_str.clone();
    if s3_hero_val == "N/A" {
        s3_hero_val = val_str.clone();
    }
    let s3_hero_lbl = match comp_med_psf {
        Some(cp) if est_psf.is_some() => format!(
            "{brand_name} $/Sq Ft vs ${}/sf Comp Median ({est_conf})",
            fmt_commas(cp.round() as i64)
        ),
        _ => format!("Estimated Price / Sq Ft ({est_conf} Confidence)"),
    };

    let slide3 = json!({
        "slideNumber": 3,
        "id": "zestimate_comps",
        "tag": "03 · Valuation & Comps",
        "title": "Valuation Benchmark & Comparable Sales",
        "subtitle": "Repeat-sales indexing, adjusted comps & third-party estimates",
        "heroStat": {
            "value": s3_hero_val,
            "label": s3_hero_lbl,
            "tone": if est_val.is_some() { "good" } else { "info" }
        },
        "criticalPoints": [
            repeat_line,
            comps_line,
            avm_line
        ],
        "watchout": "Automated models and public comps cannot verify interior condition, natural light, or unpermitted renovations — adjust ±5–8% after touring.",
        "speakerNotes": format!(
            "ZIP market context: {} forecast / {} YoY.",
            mkt_data
                .and_then(|m| m.pointer("/forecast/horizons"))
                .and_then(|h| h.as_array())
                .and_then(|a| a.last())
                .and_then(|x| x.get("pct"))
                .and_then(|v| v.as_f64())
                .map(|p| format!("{p:+.1}% 12-mo ZIP"))
                .unwrap_or_else(|| "check ZIP trend".to_string()),
            mkt_data
                .and_then(|m| ptr_f64(m, &["/zhviSfr/yoy", "/zhvi/yoy"]))
                .map(|y| format!("{y:+.1}%"))
                .unwrap_or_else(|| "N/A".to_string())
        ),
        "visualKind": "zestimate_comps",
        "targetSection": "value",
        "zh": {
            "tag": "03 · 估值基准与可比成交",
            "title": "多维估值基准对标与周边成交拆解",
            "subtitle": "重售指数、特征修正可比成交与第三方估值对照",
            "heroStatLabel": comp_med_psf
                .map(|cp| format!("{brand_name} 预估单价 vs 可比成交中位 ${}/平尺", fmt_commas(cp.round() as i64)))
                .unwrap_or_else(|| format!("预估每平尺单价（置信度：{est_conf}）")),
            "criticalPoints": [
                if let Some(rv) = repeat_val {
                    format!("历史重售指数推算：{}（{}）。", fmt_usd_compact(Some(rv)), repeat_detail.as_deref().unwrap_or("按邮编房价指数折算"))
                } else {
                    "暂无足够历史重售配对记录，估值主要参考可比成交与邮编房价指数。".to_string()
                },
                match (comp_count, comp_med_adj) {
                    (Some(cnt), Some(med)) if cnt > 0 => format!(
                        "{cnt} 套特征修正可比成交中位价 {}（中位单价 {}{}）。",
                        fmt_usd_compact(Some(med)),
                        comp_med_psf.map(|p| format!("${}/平尺", fmt_commas(p.round() as i64))).unwrap_or_else(|| "已按面积地段修正".to_string()),
                        comp_stl.map(|s| format!("，成交/挂牌比 {s:.1}%")).unwrap_or_default()
                    ),
                    _ => "当前暂无已结构化的周边可比成交，建议人工复核 1 英里内近期过户记录。".to_string()
                },
                if !avm_list.is_empty() {
                    let joined: Vec<String> = avm_list.iter().take(3).map(|(_, val)| fmt_usd_compact(Some(*val))).collect();
                    format!("第三方估值：{}（对比 {brand_name} 混合估值 {val_str}）。", joined.join(" · "))
                } else {
                    format!("单价对标：{brand_name} 估算 {psf_str}（本报告暂无第三方估值记录）。")
                }
            ],
            "watchout": "线上估值与挂牌记录无法穿透室内实际翻新成色、采光与隐蔽缺陷，实地看房后建议结合装修差异预留 ±5%–8% 调整空间。",
            "speakerNotes": "可在“估值与成交”章节查看重售法、可比修正法、邮编房价指数及价格历史回撤统计。"
        },
        "es": {
            "tag": "03 · Valoración y Comparables",
            "title": "Referencia de Valoración y Ventas Comparables",
            "subtitle": "Índice de ventas repetidas, comparables ajustados y estimaciones de terceros",
            "heroStatLabel": comp_med_psf
                .map(|cp| format!("{brand_name} $/p² vs Mediana Comps ${}/p²", fmt_commas(cp.round() as i64)))
                .unwrap_or_else(|| format!("Precio Estimado / Pie² (Confianza {est_conf})")),
            "criticalPoints": [
                if let Some(rv) = repeat_val {
                    format!("Indexación por ventas repetidas: {}{}.", fmt_usd_compact(Some(rv)), repeat_detail.as_ref().map(|d| format!(" ({d})")).unwrap_or_default())
                } else {
                    "Sin historial indexado de venta previa; valoración apoyada en comparables cercanos.".to_string()
                },
                match (comp_count, comp_med_adj) {
                    (Some(cnt), Some(med)) if cnt > 0 => format!(
                        "{cnt} ventas comparables ajustadas arrojan mediana de {} ({}{}).",
                        fmt_usd_compact(Some(med)),
                        comp_med_psf.map(|p| format!("${}/p²", fmt_commas(p.round() as i64))).unwrap_or_else(|| "ajustado".to_string()),
                        comp_stl.map(|s| format!(", {s:.1}% venta/lista")).unwrap_or_default()
                    ),
                    _ => "Sin resumen de comparables disponible; revise cierres recientes en un radio de 1 milla.".to_string()
                },
                if !avm_list.is_empty() {
                    let joined: Vec<String> = avm_list.iter().take(3).map(|(_, val)| fmt_usd_compact(Some(*val))).collect();
                    let label = if joined.len() == 1 {
                        "Estimación de terceros"
                    } else {
                        "Estimaciones de terceros"
                    };
                    format!("{label}: {} frente a mezcla {brand_name} {val_str}.", joined.join(" · "))
                } else {
                    format!("Referencia por pie²: {brand_name} {psf_str} (sin estimaciones de terceros citadas).")
                }
            ],
            "watchout": "Los modelos estadísticos no inspeccionan acabados interiores ni permisos; ajuste ±5–8% tras la visita.",
            "speakerNotes": "Consulte el Hub de Valoración para ver cada venta comparable y las curvas históricas del índice de valor del código postal."
        }
    });

    // ===================== SLIDE 4: cost_insurance =====================
    let pi_mo = val_data.and_then(|v| ptr_f64(v, &["/ownership/monthly/principalInterest"]));
    let tax_mo = val_data.and_then(|v| ptr_f64(v, &["/ownership/monthly/propertyTax"]));
    let maint_mo = val_data.and_then(|v| ptr_f64(v, &["/ownership/monthly/maintenance"]));
    let rate30 = val_data.and_then(|v| ptr_f64(v, &["/ownership/rate30"]));
    let down_pct = val_data.and_then(|v| ptr_f64(v, &["/ownership/downPaymentPct"]));
    let tax_rate_pct = val_data
        .and_then(|v| ptr_f64(v, &["/ownership/taxRatePct"]))
        .or_else(|| facts.and_then(|f| ptr_f64(f, &["/taxRate"])).map(|r| r * 100.0));
    let tax_note = val_data
        .and_then(|v| ptr_str(v, &["/ownership/taxNote"]))
        .map(scrub_string);

    let ho3_mo = ins_quotes
        .and_then(|iq| ptr_f64(iq, &["/monthlyTotal"]))
        .or_else(|| ins_data.and_then(|i| ptr_f64(i, &["/realisticMonthlyUSD"])))
        .or_else(|| val_data.and_then(|v| ptr_f64(v, &["/ownership/monthly/insurance"])));
    let quake_mo = ins_quotes
        .and_then(|iq| ptr_f64(iq, &["/specialtyAddOns/earthquake/estMonthly"]))
        .or_else(|| {
            ins_data.and_then(|i| ptr_f64(i, &["/perilBreakdown/earthquakeOptional/monthly"]))
        })
        .or_else(|| val_data.and_then(|v| ptr_f64(v, &["/ownership/monthly/earthquakeInsurance"])));
    let flood_mo = ins_quotes
        .and_then(|iq| ptr_f64(iq, &["/specialtyAddOns/flood/estMonthly"]))
        .or_else(|| {
            ins_data.and_then(|i| ptr_f64(i, &["/perilBreakdown/floodRequiredOrRecommended/monthly"]))
        });
    let cov_a = ins_quotes
        .and_then(|iq| ptr_f64(iq, &["/coverage/dwellingA"]))
        .or_else(|| ins_data.and_then(|i| ptr_f64(i, &["/coverageAUSD"])));

    let monthly_total = val_data
        .and_then(|v| ptr_f64(v, &["/ownership/monthly/total"]))
        .or_else(|| val_data.and_then(|v| ptr_f64(v, &["/ownership/monthly"])));
    let monthly_with_quake =
        val_data.and_then(|v| ptr_f64(v, &["/ownership/monthly/totalWithQuake"]));
    let rent_est = val_data
        .and_then(|v| ptr_f64(v, &["/ownership/rentEstimate"]))
        .or_else(|| mkt_data.and_then(|m| ptr_f64(m, &["/rent/zori"])))
        .or_else(|| facts.and_then(|f| ptr_f64(f, &["/rentEstimate"])));

    let cost_hero_val = fmt_usd_exact(monthly_total)
        .map(|s| format!("{s}/mo"))
        .or_else(|| fmt_usd_exact(ho3_mo).map(|s| format!("{s}/mo Ins.")))
        .unwrap_or_else(|| "Verify Carry".to_string());
    let cost_hero_lbl = match (down_pct, rate30) {
        (Some(d), Some(r)) => format!("Est. Monthly Carry ({d:.0}% Down @ {r:.2}% 30Y)"),
        _ => "True Monthly Ownership Carry (PITI + Insurance)".to_string(),
    };

    let piti_line = match (pi_mo, tax_mo) {
        (Some(pi), Some(tx)) => format!(
            "P&I is {}/mo plus {}/mo reassessed property tax{}{}.",
            fmt_usd_exact(Some(pi)).unwrap_or_default(),
            fmt_usd_exact(Some(tx)).unwrap_or_default(),
            tax_rate_pct
                .map(|t| format!(" ({t:.3}% effective rate)"))
                .unwrap_or_default(),
            hoa_monthly
                .filter(|&h| h > 0.0)
                .and_then(|h| fmt_usd_exact(Some(h)).map(|hs| format!(", HOA {hs}/mo")))
                .unwrap_or_default()
        ),
        _ => format!(
            "Confirm 30-yr loan P&I and statutory property tax reassessment{} before offer.",
            tax_rate_pct
                .map(|t| format!(" (local rate {t:.3}%)"))
                .unwrap_or_default()
        ),
    };

    let ins_line = match (ho3_mo, quake_mo, flood_mo) {
        (Some(ho), qm, fm) => {
            let mut extras = Vec::new();
            if let Some(q) = qm.filter(|&x| x > 0.0) {
                extras.push(format!("Earthquake +${}/mo", fmt_commas(q.round() as i64)));
            }
            if let Some(f) = fm.filter(|&x| x > 0.0) {
                extras.push(format!("Flood +${}/mo", fmt_commas(f.round() as i64)));
            }
            format!(
                "Realistic HO-3 insurance runs ~${}/mo{}{}.",
                fmt_commas(ho.round() as i64),
                cov_a
                    .map(|c| format!(" ({} Coverage A rebuild)", fmt_usd_compact(Some(c))))
                    .unwrap_or_default(),
                if extras.is_empty() {
                    String::new()
                } else {
                    format!("; specialty add-ons: {}", extras.join(", "))
                }
            )
        }
        _ => "Request binding HO-3, earthquake, and flood quotes during inspection contingency — vintage or hazard location can shift rates."
            .to_string(),
    };

    let rvo_line = match (monthly_total, rent_est) {
        (Some(tot), Some(rent)) if rent > 0.0 => {
            let diff = (tot - rent).round() as i64;
            let verb = if diff >= 0 {
                format!("+${}/mo premium over owning vs renting", fmt_commas(diff))
            } else {
                format!("${}/mo savings vs renting", fmt_commas(-diff))
            };
            format!(
                "Rent-vs-own: {}/mo total carry{} vs {}/mo estimated market rent ({verb}).",
                fmt_usd_exact(Some(tot)).unwrap_or_default(),
                monthly_with_quake
                    .map(|wq| format!(" ({}/mo w/ quake)", fmt_usd_exact(Some(wq)).unwrap_or_default()))
                    .unwrap_or_default(),
                fmt_usd_exact(Some(rent)).unwrap_or_default()
            )
        }
        _ => format!(
            "Budget {} maintenance reserve and compare net monthly cash outflow against local rental alternatives.",
            maint_mo
                .and_then(|m| fmt_usd_exact(Some(m)))
                .map(|s| format!("{s}/mo"))
                .unwrap_or_else(|| "annual".to_string())
        ),
    };

    let prop13_watch = if state_code.eq_ignore_ascii_case("CA") {
        "California Prop 13 resets property tax to the new purchase price at closing — ignore the seller's legacy tax bill and budget supplemental tax bills."
            .to_string()
    } else if let Some(ref tn) = tax_note {
        tn.clone()
    } else {
        "Never rely on naive 0.35% lender insurance placeholders or seller's prior tax assessment — obtain binding quotes and post-sale tax reset numbers."
            .to_string()
    };

    let slide4 = json!({
        "slideNumber": 4,
        "id": "cost_insurance",
        "tag": "04 · Monthly Cost & Insurance",
        "title": "True Monthly Cost & Insurance Reality Check",
        "subtitle": "Reassessed property tax, P&I, HO-3 & hazard insurance impact",
        "heroStat": {
            "value": cost_hero_val,
            "label": cost_hero_lbl,
            "tone": "warn"
        },
        "criticalPoints": [
            piti_line,
            ins_line,
            rvo_line
        ],
        "watchout": prop13_watch,
        "speakerNotes": "Test $1,000–$10,000 deductibles plus Earthquake (CEA) and Flood (NFIP) toggles in the Chapter 4 Insurance Quote Studio.",
        "visualKind": "cost_insurance",
        "targetSection": "payment",
        "zh": {
            "tag": "04 · 真实月供与保险核价",
            "title": "全口径月持有成本与房屋保险压力测试",
            "subtitle": "成交重评地税 + 按揭本息 + HO-3 主险及地震/洪水附加险",
            "heroStatLabel": match (down_pct, rate30) {
                (Some(d), Some(r)) => format!("测算月持有总支出（首付 {d:.0}% · {r:.2}% 利率）"),
                _ => "全口径月持有支出（本息+重评税+保险）".to_string()
            },
            "criticalPoints": [
                match (pi_mo, tax_mo) {
                    (Some(pi), Some(tx)) => format!(
                        "月供本息 ${}/月 + 按购入价重评后房产税 ${}/月{}。",
                        fmt_commas(pi.round() as i64),
                        fmt_commas(tx.round() as i64),
                        tax_rate_pct.map(|t| format!("（有效税率 {t:.3}%）")).unwrap_or_default()
                    ),
                    _ => "请按实际成交价与当地有效税率核定新房产税与月度本息支出。".to_string()
                },
                match ho3_mo {
                    Some(ho) => format!(
                        "HO-3 房屋主险精算约 ${}/月{}；{}{}",
                        fmt_commas(ho.round() as i64),
                        cov_a.map(|c| format!("（重建保额 {}）", fmt_usd_compact(Some(c)))).unwrap_or_default(),
                        quake_mo.filter(|&q| q > 0.0).map(|q| format!("地震险约 +${}/月", fmt_commas(q.round() as i64))).unwrap_or_else(|| "地震险另计".to_string()),
                        flood_mo.filter(|&f| f > 0.0).map(|f| format!(" · 洪水险约 +${}/月", fmt_commas(f.round() as i64))).unwrap_or_default()
                    ),
                    None => "老房龄与地质灾害区会显著影响保费，建议在验房期内取得正式核保报价。".to_string()
                },
                match (monthly_total, rent_est) {
                    (Some(tot), Some(rent)) if rent > 0.0 => format!(
                        "租售对比：月均持有支出 ${}/月（对比周边估算市场月租 ${}/月）。",
                        fmt_commas(tot.round() as i64),
                        fmt_commas(rent.round() as i64)
                    ),
                    _ => "建议预留年度房屋维护金并对比区域同户型长租成本。".to_string()
                }
            ],
            "watchout": if state_code.eq_ignore_ascii_case("CA") {
                "加州 Prop 13 将按新成交价重评计税基数并加收过户补充税单（Supplemental Tax），切勿套用原业主历史地税单，也不宜轻信偏低保险估算。".to_string()
            } else {
                "切勿沿用原房主低地税或贷款模板 0.35% 粗略保费，签约期务必锁定真实核保报价。".to_string()
            },
            "speakerNotes": "可在第四章“保险报价工作台”中即时切换免赔额、地震险及洪水险开关并对比各承保公司。"
        },
        "es": {
            "tag": "04 · Costo Mensual y Seguro",
            "title": "Costo Mensual Real y Realidad de Seguros",
            "subtitle": "Impuesto predial reajustado, hipoteca, HO-3 y coberturas especiales",
            "heroStatLabel": match (down_pct, rate30) {
                (Some(d), Some(r)) => format!("Gasto Mensual Est. ({d:.0}% inicial @ {r:.2}%)"),
                _ => "Gasto Mensual Real de Propiedad".to_string()
            },
            "criticalPoints": [
                match (pi_mo, tax_mo) {
                    (Some(pi), Some(tx)) => format!(
                        "Capital e interés ${}/mes más impuesto predial reajustado ${}/mes{}.",
                        fmt_commas(pi.round() as i64),
                        fmt_commas(tx.round() as i64),
                        tax_rate_pct.map(|t| format!(" ({t:.3}%)")).unwrap_or_default()
                    ),
                    _ => "Calcule capital, interés e impuesto predial reajustado al precio de compra.".to_string()
                },
                match ho3_mo {
                    Some(ho) => format!(
                        "Seguro HO-3 estimado ~${}/mes{}{}.",
                        fmt_commas(ho.round() as i64),
                        cov_a.map(|c| format!(" (Cobertura A {})", fmt_usd_compact(Some(c)))).unwrap_or_default(),
                        quake_mo.filter(|&q| q > 0.0).map(|q| format!("; sismo +${}/mes", fmt_commas(q.round() as i64))).unwrap_or_default()
                    ),
                    None => "Solicite cotizaciones reales de póliza HO-3, terremoto e inundación durante contingencia.".to_string()
                },
                match (monthly_total, rent_est) {
                    (Some(tot), Some(rent)) if rent > 0.0 => format!(
                        "Compra vs alquiler: gasto mensual ${}/mes frente a renta de mercado ~${}/mes.",
                        fmt_commas(tot.round() as i64),
                        fmt_commas(rent.round() as i64)
                    ),
                    _ => "Reserve presupuesto de mantenimiento anual frente al costo local de alquiler.".to_string()
                }
            ],
            "watchout": if state_code.eq_ignore_ascii_case("CA") {
                "En California (Prop 13) el impuesto predial se reajusta al precio de compra al cierre; no use el recibo fiscal previo del vendedor.".to_string()
            } else {
                "No confíe en estimaciones genéricas de seguro del prestamista; verifique cotizaciones firmes antes de cerrar.".to_string()
            },
            "speakerNotes": "Use el Estudio de Seguros en el Capítulo 4 para probar deducibles de $1K–$10K y coberturas de sismo o inundación."
        }
    });

    // ===================== SLIDE 5: remodel_upside =====================
    let footprint_sqft = facts
        .and_then(|f| ptr_i64(f, &["/buildingRegister/footprintSqft"]))
        .or(gross_sqft)
        .or_else(|| match (sqft, stories) {
            (Some(s), Some(st)) if s > 0 && st > 0 => Some((s as f64 / st as f64).round() as i64),
            (Some(s), None) if s > 0 => Some(s),
            _ => None,
        });
    let df_open_lot = decision_factors.and_then(|d| ptr_i64(d, &["/lotAndAdu/openLotSqft"]));
    let df_bcr = decision_factors.and_then(|d| ptr_f64(d, &["/lotAndAdu/lotCoveragePct"]));
    let df_adu_rent = decision_factors.and_then(|d| ptr_f64(d, &["/lotAndAdu/estRentMonthly"]));
    let df_feasibility = decision_factors
        .and_then(|d| ptr_str(d, &["/lotAndAdu/feasibility"]))
        .map(scrub_string);

    let (open_lot_opt, bcr_opt) = match (df_open_lot, df_bcr) {
        (Some(o), Some(b)) => (Some(o), Some(b)),
        _ => match (lot_sqft, footprint_sqft) {
            (Some(l), Some(fp)) if l > 0 && fp > 0 && l >= fp => {
                let bcr = round_n((fp as f64 / l as f64) * 100.0, 1);
                (Some(l - fp), Some(bcr))
            }
            _ => (None, None),
        },
    };

    let has_adu_already = prop_type.to_ascii_lowercase().contains("adu")
        || garage
            .as_deref()
            .map(|g| g.to_ascii_lowercase().contains("adu"))
            .unwrap_or(false);
    let permit_count = facts
        .and_then(|f| f.get("permits"))
        .and_then(|p| p.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let s5_hero_val = if let Some(open) = open_lot_opt {
        format!("{} sf", fmt_commas(open))
    } else if let Some(bcr) = bcr_opt {
        format!("{bcr:.1}% BCR")
    } else if let Some(yb) = year_built {
        format!("{yb} Vintage")
    } else {
        "CapEx Plan".to_string()
    };

    let s5_hero_lbl = match (bcr_opt, df_feasibility.as_ref()) {
        (Some(bcr), Some(feas)) => format!("Open Yard ({bcr:.1}% BCR · {feas})"),
        (Some(bcr), None) => format!("Est. Open Lot Area ({bcr:.1}% Building Coverage Ratio)"),
        _ => "Lot Expansion, ADU & Vintage CapEx Priorities".to_string(),
    };

    let lot_bcr_line = match (open_lot_opt, bcr_opt, lot_sqft) {
        (Some(open), Some(bcr), Some(l)) => format!(
            "Lot coverage ratio is ~{bcr:.1}% on {} sq ft, leaving ~{} sq ft of open yard area.",
            fmt_commas(l),
            fmt_commas(open)
        ),
        _ => "Verify building footprint, side/rear setbacks, and municipal lot coverage limits with local planning."
            .to_string(),
    };

    let adu_sb9_line = if has_adu_already {
        format!(
            "Property already notes an ADU ({prop_type}); verify finaled CO/permits, separate utility metering, and{} SB 9 / JADU expansion rules.",
            if state_code.eq_ignore_ascii_case("CA") { " California" } else { "" }
        )
    } else if let Some(rent) = df_adu_rent.filter(|&r| r > 0.0) {
        format!(
            "ADU feasibility: estimated ~${}/mo rental offset{}; confirm 4-ft setbacks and utility capacity.",
            fmt_commas(rent.round() as i64),
            if state_code.eq_ignore_ascii_case("CA") { " (verify CA ADU & SB 9 lot-split eligibility)" } else { "" }
        )
    } else if state_code.eq_ignore_ascii_case("CA") {
        "California ADU / SB 9 check: confirm zoning setbacks, 4-ft ADU side/rear rules, and utility/sewer lateral capacity."
            .to_string()
    } else {
        "Check municipal zoning allowances for accessory dwelling units (ADU), garage conversions, and setback rules."
            .to_string()
    };

    let capex_line = match year_built {
        Some(y) if y < 1960 => format!(
            "Built {y} ({permit_count} permit(s) cited): prioritize seismic anchor bolting/cripple-wall bracing, electrical panel (100A/200A & wiring), galvanized/clay sewer lateral, roof life, and HVAC."
        ),
        Some(y) if y < 1990 => format!(
            "Built {y} ({permit_count} permit(s) cited): inspect roof membrane age, electrical panel, plumbing supply lines, sewer lateral, and HVAC/water-heater lifecycle."
        ),
        Some(y) => format!(
            "Built {y} ({permit_count} permit(s) cited): verify roof, HVAC, water heater service history, and drainage/grading."
        ),
        None => "Year built not listed — inspect roof remaining life, HVAC, electrical panel amperage, and sewer lateral via camera."
            .to_string(),
    };

    let slide5 = json!({
        "slideNumber": 5,
        "id": "remodel_upside",
        "tag": "05 · Lot, ADU & CapEx",
        "title": "Lot Potential, ADU & Vintage CapEx Triage",
        "subtitle": "Open lot area, BCR, zoning upside & age-calibrated systems",
        "heroStat": {
            "value": s5_hero_val,
            "label": s5_hero_lbl,
            "tone": if year_built.map(|y| y < 1960).unwrap_or(false) { "warn" } else { "good" }
        },
        "criticalPoints": [
            lot_bcr_line,
            adu_sb9_line,
            capex_line
        ],
        "watchout": "Before budgeting a remodel or ADU addition, camera-inspect the sewer lateral and verify 200A main electrical panel service capacity.",
        "speakerNotes": "Inspect the 3D Lot Model in Chapter 2 for roof geometry, tree overhang, and solar azimuth exposure.",
        "visualKind": "remodel_upside",
        "targetSection": "house3d",
        "zh": {
            "tag": "05 · 加建潜力与大修排期",
            "title": "宗地余量、ADU / SB 9 潜力与按房龄大修清单",
            "subtitle": "建筑占地率（BCR）、空地余量与屋面/水电/抗震优先级",
            "heroStatLabel": match bcr_opt {
                Some(bcr) => format!("测算剩余空地（建筑占地率约 {bcr:.1}%）"),
                None => "土地利用率与系统大修评估".to_string()
            },
            "criticalPoints": [
                match (open_lot_opt, bcr_opt) {
                    (Some(open), Some(bcr)) => format!("当前建筑占地率约 {bcr:.1}%，留有约 {} 平方英尺院落空地空间。", fmt_commas(open)),
                    _ => "请结合市政退界与容积率限制核实院落扩展与加建空间。".to_string()
                },
                if has_adu_already {
                    "档案显示已含附属单元（ADU），请重点核验其完工验收许可（CO）与独立水电分表情况。".to_string()
                } else if state_code.eq_ignore_ascii_case("CA") {
                    "可结合加州 ADU 4 英尺退界及 SB 9 分割新规核实后院加建与市政管网扩容条件。".to_string()
                } else {
                    "规划扩建或车库改造前请向市政规划局核实 ADU 许可与退界条件。".to_string()
                },
                match year_built {
                    Some(y) if y < 1960 => format!("{y} 年建（档案见 {permit_count} 项许可）：优先排查地基抗震螺栓与矮墙加固、主配电箱（是否 200A/老式布线）、老旧上下水管、下水主管内窥镜及屋顶寿命。"),
                    Some(y) => format!("{y} 年建（档案见 {permit_count} 项许可）：重点核查屋顶材料年限、暖通空调（HVAC）、热水器及电路容量。"),
                    None => "房龄待核实：建议重点排查屋顶、配电盘、暖通设备与排污主管剩余使用年限。".to_string()
                }
            ],
            "watchout": "筹划厨卫翻新或 ADU 加建前，务必先用内窥镜检查排污主管（Sewer Lateral）并核实主配电盘安培余量。",
            "speakerNotes": "可在第二章“三维模型（3D）”查看屋顶坡度、周边树木遮挡及日照光伏朝向。"
        },
        "es": {
            "tag": "05 · Potencial, ADU y CapEx",
            "title": "Potencial del Terreno, ADU y Prioridades CapEx",
            "subtitle": "Área libre, ocupación del lote y sistemas según antigüedad",
            "heroStatLabel": match bcr_opt {
                Some(bcr) => format!("Patio Libre Est. ({bcr:.1}% Ocupación BCR)"),
                None => "Potencial de Lote y Mantenimiento Mayor".to_string()
            },
            "criticalPoints": [
                match (open_lot_opt, bcr_opt) {
                    (Some(open), Some(bcr)) => format!("Ocupación del lote ~{bcr:.1}%, dejando ~{} p² de área libre.", fmt_commas(open)),
                    _ => "Confirme retiros y cobertura máxima permitida con planificación municipal.".to_string()
                },
                if has_adu_already {
                    "La propiedad ya registra una ADU; verifique permisos finales y medidores independientes.".to_string()
                } else {
                    "Evalúe reglas locales de ADU (y SB 9 si aplica en California) junto con capacidad de alcantarillado.".to_string()
                },
                match year_built {
                    Some(y) => format!("Construida en {y} ({permit_count} permiso(s)): inspeccione techo, panel eléctrico (200A), plomería, alcantarillado lateral, HVAC y anclaje sísmico."),
                    None => "Inspeccione techo, panel eléctrico, HVAC y alcantarillado lateral con cámara.".to_string()
                }
            ],
            "watchout": "Verifique capacidad eléctrica de 200A y estado de tubería de drenaje antes de presupuestar ampliaciones o ADU.",
            "speakerNotes": "Revise el Modelo 3D en el Capítulo 2 para analizar sombras, altura de techo y potencial solar."
        }
    });

    // ===================== SLIDE 6: risk_climate =====================
    let pga = haz_data.and_then(|h| ptr_f64(h, &["/earthquake/designMaps/pgaM", "/earthquake/pga"]));
    let sdc = haz_data
        .and_then(|h| ptr_str(h, &["/earthquake/designMaps/sdc"]))
        .map(scrub_string);
    let nearest_fault = haz_data
        .and_then(|h| ptr_str(h, &["/earthquake/faults/nearest/0/name"]))
        .map(scrub_string);
    let nearest_fault_mi =
        haz_data.and_then(|h| ptr_f64(h, &["/earthquake/faults/nearest/0/distMi"]));
    let flood_zone = haz_data
        .and_then(|h| ptr_str(h, &["/flood/zone", "/flood/femaZone"]))
        .unwrap_or("Unverified");
    let flood_sfha = haz_data
        .and_then(|h| h.pointer("/flood/sfha"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let nearest_sfha_zone = haz_data
        .and_then(|h| ptr_str(h, &["/flood/nearestSfha/zone"]))
        .map(scrub_string);
    let nearest_sfha_mi =
        haz_data.and_then(|h| ptr_f64(h, &["/flood/nearestSfha/distMi"]));
    let wildfire_desc = haz_data
        .and_then(|h| {
            ptr_str(
                h,
                &["/wildfire/california/fhsz", "/wildfire/whp/label"],
            )
        })
        .map(scrub_string);

    let pm25 = env_data.and_then(|e| ptr_f64(e, &["/air/pm25/mean"]));
    let aqi_mean_max = env_data.and_then(|e| ptr_i64(e, &["/air/usAqi/meanDailyMax"]));
    let npl_total = env_data
        .and_then(|e| e.pointer("/sites/counts"))
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|x| x.get("program").and_then(|p| p.as_str()) == Some("npl"))
        })
        .and_then(|n| n.get("total").and_then(|v| v.as_i64()));
    let npl_1mi = env_data
        .and_then(|e| e.pointer("/sites/counts"))
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|x| x.get("program").and_then(|p| p.as_str()) == Some("npl"))
        })
        .and_then(|n| n.pointer("/within/1mi").and_then(|v| v.as_i64()));
    let water_sys = env_data
        .and_then(|e| ptr_str(e, &["/water/systems/0/name"]))
        .map(scrub_string);
    let noise_lbl = env_data
        .and_then(|e| ptr_str(e, &["/noise/atProperty/all/label"]))
        .map(scrub_string);

    let s6_hero_val = safety_score
        .map(|s| format!("{s}/100"))
        .unwrap_or_else(|| format!("Zone {flood_zone}"));
    let s6_hero_lbl = format!(
        "Hazard Safety · FEMA Zone {flood_zone}{}",
        pga.map(|g| format!(" · PGA {g:.2}g")).unwrap_or_default()
    );
    let s6_tone = match (safety_score, flood_sfha) {
        (_, true) => "warn",
        (Some(s), false) if s >= 65 => "good",
        _ => "warn",
    };

    let eq_flood_line = format!(
        "FEMA Flood Zone {flood_zone}{}{}. Seismic PGA: {}{}{}.",
        if flood_sfha { " (SFHA — mandatory lender flood policy)" } else { " (outside SFHA)" },
        match (nearest_sfha_zone.as_ref(), nearest_sfha_mi) {
            (Some(z), Some(mi)) if !flood_sfha => format!("; nearest Zone {z} {mi:.2} mi away"),
            _ => String::new(),
        },
        pga.map(|g| format!("{g:.2}g")).unwrap_or_else(|| "N/A".to_string()),
        sdc.as_ref().map(|s| format!(" (SDC {s})")).unwrap_or_default(),
        match (nearest_fault.as_ref(), nearest_fault_mi) {
            (Some(f), Some(mi)) => format!("; nearest fault {f} at {mi:.2} mi"),
            _ => String::new(),
        }
    );

    let wf_line = format!(
        "Wildfire & noise: {}{}.",
        wildfire_desc
            .as_deref()
            .unwrap_or("Check state/USFS wildfire hazard map"),
        noise_lbl
            .as_ref()
            .map(|n| format!("; DOT transportation noise {n}"))
            .unwrap_or_default()
    );

    let env_line = format!(
        "Environment ({}): {}{}{}.",
        env_score
            .map(|s| format!("{s}/100"))
            .unwrap_or_else(|| "screened".to_string()),
        match (pm25, aqi_mean_max) {
            (Some(pm), Some(aq)) => format!("30-day PM2.5 {pm:.1} µg/m³ (mean daily max US AQI {aq})"),
            (Some(pm), None) => format!("30-day PM2.5 {pm:.1} µg/m³"),
            _ => "Air & water compliance screened".to_string(),
        },
        npl_total
            .map(|tot| format!(
                "; {tot} Superfund NPL within 5 mi ({} within 1 mi)",
                npl_1mi.unwrap_or(0)
            ))
            .unwrap_or_default(),
        water_sys
            .as_ref()
            .map(|w| format!("; water: {w}"))
            .unwrap_or_default()
    );

    let s6_watch = if pga.map(|g| g >= 0.4).unwrap_or(false) {
        "High seismic design shaking: verify foundation anchor bolts, cripple-wall shear bracing, and seismic gas shutoff valve."
            .to_string()
    } else if flood_sfha {
        "Parcel lies inside a Special Flood Hazard Area — obtain Elevation Certificate and binding NFIP flood quote early."
            .to_string()
    } else {
        "Review state Natural Hazard Disclosure (NHD) statutory report and EPA cleanup buffers in escrow."
            .to_string()
    };

    let slide6 = json!({
        "slideNumber": 6,
        "id": "risk_climate",
        "tag": "06 · Hazards & Environment",
        "title": "Natural Hazards, Climate & Environmental Screen",
        "subtitle": "Earthquake PGA, FEMA flood, wildfire, air quality & Superfund proximity",
        "heroStat": {
            "value": s6_hero_val,
            "label": s6_hero_lbl,
            "tone": s6_tone
        },
        "criticalPoints": [
            eq_flood_line,
            wf_line,
            env_line
        ],
        "watchout": s6_watch,
        "speakerNotes": "Chapter 5 maps live FEMA flood overlays, mapped Quaternary faults, wildfire zones, and EPA cleanup sites.",
        "visualKind": "risk_climate",
        "targetSection": "risk",
        "zh": {
            "tag": "06 · 自然灾害与环境筛查",
            "title": "自然灾害、气候与环保污染近距排查",
            "subtitle": "USGS 地震加速度、FEMA 洪水区、山火与 EPA 超级基金筛查",
            "heroStatLabel": format!(
                "灾害安全评分（FEMA {flood_zone} 区{}）",
                pga.map(|g| format!(" · 地震 PGA {g:.2}g")).unwrap_or_default()
            ),
            "criticalPoints": [
                format!(
                    "洪水与抗震：FEMA {flood_zone} 区{}；USGS 地震设计峰值加速度 {}{}。",
                    if flood_sfha { "（高风险泛滥区，需买洪水险）" } else { "（非高风险泛滥区）" },
                    pga.map(|g| format!("{g:.2}g")).unwrap_or_else(|| "待查".to_string()),
                    nearest_fault.as_ref().zip(nearest_fault_mi).map(|(f, mi)| format!("，最近断层 {f} 距 {mi:.2} 英里")).unwrap_or_default()
                ),
                format!(
                    "山火与交通噪声：{}{}。",
                    wildfire_desc.as_deref().unwrap_or("请核对州级山火分级图"),
                    noise_lbl.as_ref().map(|n| format!("；交通噪声 {n}")).unwrap_or_default()
                ),
                format!(
                    "环境健康（{}）：{}{}{}。",
                    env_score.map(|s| format!("{s}/100 分")).unwrap_or_else(|| "已筛查".to_string()),
                    pm25.map(|p| format!("近 30 天 PM2.5 均值 {p:.1} µg/m³")).unwrap_or_else(|| "空气质量已核查".to_string()),
                    npl_total.map(|t| format!("；5 英里内超级基金 NPL {t} 处（1 英里内 {} 处）", npl_1mi.unwrap_or(0))).unwrap_or_default(),
                    water_sys.as_ref().map(|w| format!("；供水：{w}")).unwrap_or_default()
                )
            ],
            "watchout": if pga.map(|g| g >= 0.4).unwrap_or(false) {
                "地震动强度较高：验房请重点检查基础锚栓固定、矮墙抗剪面板及燃气地震自动切断阀。".to_string()
            } else {
                "托管期间请对照法定自然灾害披露文件（NHD）及环保局监管清单逐项确认。".to_string()
            },
            "speakerNotes": "第五章交互地图支持实时叠加 FEMA 洪水层、地质断层及 EPA 污染点位。"
        },
        "es": {
            "tag": "06 · Riesgos y Ambiente",
            "title": "Riesgos Naturales, Clima y Revisión Ambiental",
            "subtitle": "Sismicidad USGS, inundación FEMA, incendios, aire y sitios EPA",
            "heroStatLabel": format!(
                "Seguridad ante Riesgos (Zona FEMA {flood_zone}{})",
                pga.map(|g| format!(" · PGA {g:.2}g")).unwrap_or_default()
            ),
            "criticalPoints": [
                format!(
                    "Inundación FEMA Zona {flood_zone}{}; aceleración sísmica PGA {}{}.",
                    if flood_sfha { " (SFHA)" } else { " (fuera de SFHA)" },
                    pga.map(|g| format!("{g:.2}g")).unwrap_or_else(|| "N/D".to_string()),
                    nearest_fault.as_ref().zip(nearest_fault_mi).map(|(f, mi)| format!(", falla {f} a {mi:.2} mi")).unwrap_or_default()
                ),
                format!(
                    "Incendio y ruido: {}{}.",
                    wildfire_desc.as_deref().unwrap_or("Verifique mapa oficial de incendios"),
                    noise_lbl.as_ref().map(|n| format!("; ruido vial {n}")).unwrap_or_default()
                ),
                format!(
                    "Revisión ambiental ({}): {}{}.",
                    env_score.map(|s| format!("{s}/100")).unwrap_or_else(|| "revisado".to_string()),
                    pm25.map(|p| format!("PM2.5 30d {p:.1} µg/m³")).unwrap_or_else(|| "Calidad de aire revisada".to_string()),
                    npl_total.map(|t| format!("; {t} sitios Superfund NPL en 5 mi ({} en 1 mi)", npl_1mi.unwrap_or(0))).unwrap_or_default()
                )
            ],
            "watchout": if pga.map(|g| g >= 0.4).unwrap_or(false) {
                "Zona de alta sismicidad: inspeccione pernos de cimentación y refuerzo de muros bajos.".to_string()
            } else {
                "Revise el informe oficial de riesgos naturales (NHD) durante el periodo de contingencia.".to_string()
            },
            "speakerNotes": "El Capítulo 5 incluye mapas interactivos de inundación FEMA, fallas sísmicas y sitios EPA."
        }
    });

    // ===================== SLIDE 7: schools_lifestyle =====================
    let assigned_schools: Vec<(String, Option<i64>, Option<String>, Option<f64>)> = facts
        .and_then(|f| f.pointer("/schools/assigned"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let name = s.get("name").and_then(|v| v.as_str())?;
                    let rating = s.get("rating").and_then(|v| v.as_i64());
                    let niche = s
                        .get("nicheGrade")
                        .and_then(|v| v.as_str())
                        .map(|x| x.to_string());
                    let dist = s.get("distanceMi").and_then(|v| v.as_f64());
                    Some((scrub_string(name), rating, niche, dist))
                })
                .collect()
        })
        .unwrap_or_default();

    let elem_dist = ptr_str(
        report,
        &[
            "/geo/schoolDistricts/unified",
            "/geo/schoolDistricts/elementary",
            "/modules/civic/data/schools/assignedDistricts/unified",
            "/modules/civic/data/schools/assignedDistricts/elementary",
        ],
    )
    .map(scrub_string);

    let walk_score = facts
        .and_then(|f| ptr_i64(f, &["/walkScore/walk"]))
        .or_else(|| civ_data.and_then(|c| ptr_i64(c, &["/scores/walkability"])));
    let epa_walk = civ_data.and_then(|c| ptr_f64(c, &["/walkability/index"]));

    let nearest_grocery = amen_data.and_then(|a| {
        let name = ptr_str(a, &["/nearest/grocery/name"])?;
        let dist = ptr_f64(a, &["/nearest/grocery/distMi"]);
        Some((scrub_string(name), dist))
    });
    let nearest_park = amen_data.and_then(|a| {
        let name = ptr_str(a, &["/nearest/park/name"])?;
        let dist = ptr_f64(a, &["/nearest/park/distMi"]);
        Some((scrub_string(name), dist))
    });
    let nearest_cafe = amen_data.and_then(|a| {
        let name = ptr_str(a, &["/nearest/cafe/name"])?;
        let dist = ptr_f64(a, &["/nearest/cafe/distMi"]);
        Some((scrub_string(name), dist))
    });
    let parks_1mi = amen_data.and_then(|a| ptr_i64(a, &["/counts/park/oneMile"]));
    let rest_1mi = amen_data.and_then(|a| ptr_i64(a, &["/counts/restaurant/oneMile"]));

    let crime_agency = civ_data
        .and_then(|c| ptr_str(c, &["/crime/agency/name"]))
        .map(scrub_string);
    let crime_violent = civ_data.and_then(|c| ptr_f64(c, &["/crime/latest/violent/ratePer100k"]));
    let crime_prop = civ_data.and_then(|c| ptr_f64(c, &["/crime/latest/property/ratePer100k"]));
    let nearest_fire_mi = amen_data.and_then(|a| ptr_f64(a, &["/nearest/emergency/distMi"]));

    let s7_hero_val = liv_score
        .map(|s| format!("{s}/100"))
        .or_else(|| walk_score.map(|w| format!("{w}/100")))
        .unwrap_or_else(|| "Verified".to_string());
    let top_sch_label = assigned_schools
        .first()
        .map(|(n, r, _, _)| match r {
            Some(rt) => format!("{n} {rt}/10"),
            None => n.clone(),
        })
        .or_else(|| elem_dist.clone())
        .unwrap_or_else(|| "Local Schools".to_string());
    let s7_hero_lbl = format!(
        "Livability{} · {top_sch_label}",
        walk_score
            .map(|w| format!(" · Walk {w}/100"))
            .unwrap_or_default()
    );

    let schools_line = if !assigned_schools.is_empty() {
        let items: Vec<String> = assigned_schools
            .iter()
            .take(3)
            .map(|(n, r, ng, d)| {
                let mut meta = Vec::new();
                if let Some(rt) = r {
                    meta.push(format!("{rt}/10"));
                }
                if let Some(g) = ng {
                    meta.push(g.clone());
                }
                if let Some(mi) = d {
                    meta.push(format!("{mi:.2} mi"));
                }
                if meta.is_empty() {
                    n.clone()
                } else {
                    format!("{n} ({})", meta.join(", "))
                }
            })
            .collect();
        format!("Assigned schools: {}.", items.join(" · "))
    } else if let Some(ref dist) = elem_dist {
        format!("School district: {dist} (confirm exact campus assignment with district registrar).")
    } else {
        "Assigned school ratings not provided in this record — check district attendance boundaries directly."
            .to_string()
    };

    let amen_bits: Vec<String> = [
        nearest_park
            .as_ref()
            .map(|(n, d)| format!("park {n}{}", d.map(|m| format!(" ({m:.2} mi)")).unwrap_or_default())),
        nearest_grocery
            .as_ref()
            .map(|(n, d)| format!("grocery {n}{}", d.map(|m| format!(" ({m:.2} mi)")).unwrap_or_default())),
        nearest_cafe
            .as_ref()
            .map(|(n, d)| format!("café {n}{}", d.map(|m| format!(" ({m:.2} mi)")).unwrap_or_default())),
    ]
    .into_iter()
    .flatten()
    .collect();
    let walk_amen_line = format!(
        "Walkability score: {}{}{}.",
        walk_score
            .map(|w| format!("{w}/100"))
            .unwrap_or_else(|| "N/A".to_string()),
        epa_walk
            .map(|e| format!(" (EPA index {e:.1}/20)"))
            .unwrap_or_default(),
        if !amen_bits.is_empty() {
            format!(
                "; nearest {}{}",
                amen_bits.join(", "),
                match (parks_1mi, rest_1mi) {
                    (Some(p), Some(r)) => format!(" ({p} parks & {r} eateries within 1 mi)"),
                    _ => String::new(),
                }
            )
        } else {
            String::new()
        }
    );

    let safety_line = match (crime_agency.as_ref(), crime_violent, crime_prop) {
        (Some(ag), Some(v), Some(p)) => format!(
            "FBI agency data ({ag}): violent {v:.1} & property {p:.1} per 100k{}." ,
            nearest_fire_mi
                .map(|m| format!("; nearest fire station {m:.2} mi"))
                .unwrap_or_default()
        ),
        _ => format!(
            "City-level FBI agency crime breakdown is not published in this report{}; verify local police logs.",
            nearest_fire_mi
                .map(|m| format!(" (nearest fire/emergency station {m:.2} mi away)"))
                .unwrap_or_default()
        ),
    };

    let slide7 = json!({
        "slideNumber": 7,
        "id": "schools_lifestyle",
        "tag": "07 · Schools & Walkability",
        "title": "Schools, Safety & Daily Walkability",
        "subtitle": format!("{top_sch_label}{}", walk_score.map(|w| format!(" · Walkability score {w}/100")).unwrap_or_default()),
        "heroStat": {
            "value": s7_hero_val,
            "label": s7_hero_lbl,
            "tone": if liv_score.unwrap_or(50) >= 65 { "good" } else { "info" }
        },
        "criticalPoints": [
            schools_line,
            walk_amen_line,
            safety_line
        ],
        "watchout": "School attendance boundaries and transfer caps can change annually — confirm seat availability directly with the school district.",
        "speakerNotes": "Enter your workplace or frequent destinations in the Neighborhood chapter to check peak-hour drive and transit times.",
        "visualKind": "schools_lifestyle",
        "targetSection": "schools",
        "zh": {
            "tag": "07 · 学区·治安与步行便利指数",
            "title": "公立学区、社区治安与日常出行便利度",
            "subtitle": format!("{top_sch_label}{}", walk_score.map(|w| format!(" · 步行便利指数 {w}/100")).unwrap_or_default()),
            "heroStatLabel": format!("社区宜居度 · {top_sch_label}"),
            "criticalPoints": [
                if !assigned_schools.is_empty() {
                    let items: Vec<String> = assigned_schools.iter().take(3).map(|(n, r, ng, _)| format!("{n}{}{}", r.map(|rt| format!(" {rt}/10")).unwrap_or_default(), ng.as_ref().map(|g| format!(" ({g})")).unwrap_or_default())).collect();
                    format!("对口公立学校：{}。", items.join(" · "))
                } else {
                    format!("所属学区：{}（入学请以学区最新划片通知为准）。", elem_dist.as_deref().unwrap_or("请向当地学区确认"))
                },
                format!(
                    "出行与生活配套：{}{}。",
                    walk_score.map(|w| format!("步行便利指数 {w}/100")).unwrap_or_else(|| "步行便利指数待查".to_string()),
                    if !amen_bits.is_empty() { format!("；最近配套：{}", amen_bits.join("、")) } else { String::new() }
                ),
                match (crime_agency.as_ref(), crime_violent, crime_prop) {
                    (Some(ag), Some(v), Some(p)) => format!("FBI 警局治安参考（{ag}）：每十万人暴力 {v:.1}、财产案件 {p:.1}。"),
                    _ => format!("本城市当前报告未收录 FBI 警局分项犯罪统计{}。", nearest_fire_mi.map(|m| format!("（最近消防站距 {m:.2} 英里）")).unwrap_or_default())
                }
            ],
            "watchout": "公立学区划片边界与学位名额每年可能微调，购房前务必直接致电对应学区办公室确认。",
            "speakerNotes": "可在“生活圈”章节添加日常通勤地址，测算早晚高峰实际通行耗时。"
        },
        "es": {
            "tag": "07 · Escuelas y Vida Diaria",
            "title": "Escuelas, Seguridad y Caminabilidad Diaria",
            "subtitle": format!("{top_sch_label}{}", walk_score.map(|w| format!(" · Índice de caminabilidad {w}/100")).unwrap_or_default()),
            "heroStatLabel": format!("Habitalidad · {top_sch_label}"),
            "criticalPoints": [
                if !assigned_schools.is_empty() {
                    let items: Vec<String> = assigned_schools.iter().take(3).map(|(n, r, ng, _)| format!("{n}{}{}", r.map(|rt| format!(" {rt}/10")).unwrap_or_default(), ng.as_ref().map(|g| format!(" ({g})")).unwrap_or_default())).collect();
                    format!("Escuelas asignadas: {}.", items.join(" · "))
                } else {
                    format!("Distrito escolar: {} (confirme límites con el distrito).", elem_dist.as_deref().unwrap_or("Consultar distrito"))
                },
                format!(
                    "Índice de caminabilidad: {}{}.",
                    walk_score.map(|w| format!("{w}/100")).unwrap_or_else(|| "N/D".to_string()),
                    if !amen_bits.is_empty() { format!("; cercanos: {}", amen_bits.join(", ")) } else { String::new() }
                ),
                match (crime_agency.as_ref(), crime_violent, crime_prop) {
                    (Some(ag), Some(v), Some(p)) => format!("Datos FBI ({ag}): tasa violenta {v:.1} y propiedad {p:.1} por 100k."),
                    _ => format!("Sin estadística criminal FBI publicada para esta ciudad en el reporte{}.", nearest_fire_mi.map(|m| format!(" (estación de bomberos a {m:.2} mi)")).unwrap_or_default())
                }
            ],
            "watchout": "Los límites escolares y cupos cambian cada año; verifique disponibilidad directamente con el distrito escolar.",
            "speakerNotes": "Agregue su lugar de trabajo en la sección de Vecindario para calcular tiempos de traslado."
        }
    });

    // ===================== SLIDE 8: action_plan =====================
    let seller_qs = pick_str_list(ai.and_then(|a| a.get("sellerQuestions")), 3);
    let neg_levers = pick_str_list(ai.and_then(|a| a.get("negotiation")), 2);
    let verify_items = pick_str_list(ai.and_then(|a| a.get("verify")), 2);

    let top_q_line = if !seller_qs.is_empty() {
        format!("Top seller questions: {}", seller_qs.join(" | "))
    } else {
        format!(
            "Top 3 seller questions: (1) Finaled permits for all remodels/ADU? (2) Installation & service dates for roof, HVAC, water heater, electrical? (3) Any sewer lateral video, drainage, or WDO history?"
        )
    };
    let inspect_focus_line = if !verify_items.is_empty() {
        format!("Inspection focus: {}", verify_items.join(" "))
    } else {
        format!(
            "Inspection focus: general + Section-1 WDO, roof certification, sewer lateral camera{}, and Preliminary Title Schedule A/B easements/liens.",
            if pga.map(|g| g >= 0.4).unwrap_or(false) { ", foundation seismic bolting" } else { "" }
        )
    };
    let offer_strategy_line = if !neg_levers.is_empty() {
        format!("Offer & credit strategy: {}", neg_levers.join(" "))
    } else {
        format!(
            "Offer & credit strategy: anchor bid within {range_str}{}, retain inspection contingencies for itemized repair credits, and bind HO-3 + hazard coverage before closing contingencies.",
            comp_med_adj
                .map(|m| format!(" (comp median {})", fmt_usd_compact(Some(m))))
                .unwrap_or_default()
        )
    };

    let slide8 = json!({
        "slideNumber": 8,
        "id": "action_plan",
        "tag": "08 · Negotiation & Checklist",
        "title": "Buyer Negotiation Levers & Due-Diligence Checklist",
        "subtitle": "Seller questions, inspection scope & offer/credit playbook",
        "heroStat": {
            "value": "3 Levers",
            "label": "Seller Questions · Inspection Focus · Credit Strategy",
            "tone": "good"
        },
        "criticalPoints": [
            top_q_line,
            inspect_focus_line,
            offer_strategy_line
        ],
        "watchout": "Upload the Seller Disclosure packet, Home Inspection PDF, and Preliminary Title Report into Chapter 6 (Docs & AI) to extract itemized repair credits and lien flags.",
        "speakerNotes": "Ask follow-up questions in the Chapter 6 AI Analyst Q&A box grounded in every verified metric of this report.",
        "visualKind": "action_plan",
        "targetSection": "inspection",
        "zh": {
            "tag": "08 · 议价抓手与行动清单",
            "title": "买家谈判筹码与交割前尽调行动清单",
            "subtitle": "三大卖家必问问题 · 验房重点 · 出价与修缮抵扣策略",
            "heroStatLabel": "三大实战抓手：提问 · 验房 · 议价抵扣",
            "criticalPoints": [
                "向卖家必问三题：(1) 历次改扩建/ADU 是否均有市政终验许可？(2) 屋顶、暖通、热水器与主配电箱更换年份？(3) 排污主管是否内窥镜检测，过往有无渗水或白蚁史？",
                "验房重点：全屋结构、WDO 白蚁一类虫害、屋顶寿命、下水主管内窥镜、地基抗震锚固及产权初审（Schedule A/B）留置权/地役权。",
                format!("出价与抵扣策略：紧扣 {range_str} 估值区间制定出价上限，保留验房保护期并按专业报价申请修缮信用抵扣（Credit），同时提前锁定房屋与灾害保险核保。")
            ],
            "watchout": "收到卖家披露文件（TDS/SPQ/NHD）、验房报告或产权预报告（Prelim）PDF 后，请上传至第六章“文档与 AI 审阅”一键生成维修抵扣清单。",
            "speakerNotes": "可在第六章 AI 问答框随时追问细节，所有回答均严格基于本报告已核验数据。"
        },
        "es": {
            "tag": "08 · Negociación y Checklist",
            "title": "Palancas de Negociación y Lista de Diligencia Debida",
            "subtitle": "Preguntas al vendedor, enfoque de inspección y estrategia de oferta",
            "heroStatLabel": "3 Palancas: Preguntas · Inspección · Créditos",
            "criticalPoints": [
                "3 preguntas clave al vendedor: permisos finales de remodelaciones/ADU, fechas de techo/HVAC/panel eléctrico e historial de alcantarillado lateral y filtraciones.",
                "Enfoque de inspección: revisión general, termitas/WDO Sección 1, techo, cámara en drenaje, pernos sísmicos y anexo A/B del informe preliminar de título.",
                format!("Estrategia de oferta: ancle su propuesta en el rango {range_str}, conserve contingencias para negociar créditos de reparación y asegure póliza firme antes del cierre.")
            ],
            "watchout": "Suba las declaraciones del vendedor, reporte de inspección y título preliminar al Capítulo 6 (Docs e IA) para extraer créditos de reparación.",
            "speakerNotes": "Use el Analista IA del Capítulo 6 para consultar cualquier cifra o cláusula antes de presentar oferta."
        }
    });

    let mut deck = json!({
        "generatedAt": generated_at,
        "deckTitle": address,
        "deckSubtitle": "8-Slide Grounded Buyer Walkthrough Briefing",
        "synthesisMode": "instant",
        "slides": [
            slide1,
            slide2,
            slide3,
            slide4,
            slide5,
            slide6,
            slide7,
            slide8
        ]
    });
    scrub_walkthrough_deck_value(&mut deck);
    deck
}

pub(crate) fn is_valid_walkthrough_deck(deck: &Value) -> bool {
    let Some(slides) = deck.get("slides").and_then(|v| v.as_array()) else {
        return false;
    };
    if slides.len() != 8 {
        return false;
    }
    for (idx, sl) in slides.iter().enumerate() {
        let Some(id) = sl.get("id").and_then(|v| v.as_str()) else {
            return false;
        };
        if id != WALKTHROUGH_SLIDE_IDS[idx] {
            return false;
        }
        let has_pts = sl
            .get("criticalPoints")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let has_zh = sl
            .pointer("/zh/title")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        let has_es = sl
            .pointer("/es/title")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_pts || !has_zh || !has_es {
            return false;
        }
    }
    true
}

/// Idempotently populates `report["walkthroughSlides"]` using the instant deterministic deck
/// whenever missing or incomplete, and scrubs any existing deck for forbidden metadata substrings.
pub fn ensure_walkthrough_slides_on_report(report: &mut Value) {
    let valid_existing = report
        .get("walkthroughSlides")
        .map(is_valid_walkthrough_deck)
        .unwrap_or(false);
    if valid_existing {
        if let Some(ws) = report.get_mut("walkthroughSlides") {
            scrub_walkthrough_deck_value(ws);
        }
        return;
    }
    let deck = build_deterministic_walkthrough_slides(report);
    if let Some(obj) = report.as_object_mut() {
        obj.insert("walkthroughSlides".to_string(), deck);
    }
}

// ------------------------- LLM Walkthrough Synthesis -------------------------

const SLIDES_INSTRUCTIONS: &str = r#"You are a Open Estate Residential Real-Estate Walkthrough Advisor sharpening an 8-slide Home Walkthrough Briefing strictly grounded in PROPERTY_DIGEST_JSON.

STRICT GROUNDING & BREVITY RULES:
1. ZERO FABRICATION: Every dollar figure, square footage, lot measurement, year built, tax/insurance amount, seismic PGA, flood zone, school score, and distance MUST come directly from PROPERTY_DIGEST_JSON. Never invent numbers.
2. EXACT 8 SLIDE SLOTS ("s1".."s8"):
   - s1: Executive Snapshot & Deal Verdict
   - s2: Home Architecture, Staging & Lot Reconciliation
   - s3: Valuation Benchmark & Comparable Sales
   - s4: True Monthly Cost & Insurance Reality Check
   - s5: Lot Potential, ADU & Vintage CapEx Triage
   - s6: Natural Hazards, Climate & Environmental Screen
   - s7: Schools, Safety & Daily Walkability
   - s8: Buyer Negotiation Levers & Due-Diligence Checklist
3. ULTRA-CONCISE PIPE FORMAT (under 170 words total across the entire JSON):
   - "deckSubtitle": <= 7 words executive briefing subtitle.
   - Format EVERY "s1".."s8" string as 3 short pipe-separated clauses (<= 20 words total per slide):
     "Headline (<=5 words) | Key Watchout (<=7 words) | Buyer Advisor Takeaway (<=8 words)"
4. FAIR HOUSING & CLEAN METADATA: Objective property, financial, hazard, environmental, school, and amenity facts only — never demographic characteristics, stereotypes, internal AI model names, routing, or retired brand names.
5. NO COMPETITOR PORTALS OR TRADEMARKS: Never mention other real-estate portals, brokerages, or their trademarks (Zillow, Zestimate, ZHVI, ZORI, Redfin, Walk Score, Realtor.com, Homes.com, Trulia, Movoto, Compass, Opendoor, LoopNet, Crexi, HouseCanary). Use generic labels only: "Third-party estimate" (never call a third-party figure Open Estate's "True Estimate"), "ZIP home-value index", "ZIP rent index", and "Walkability score"."#;

fn localized_slide_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "tag",
            "title",
            "subtitle",
            "heroStatLabel",
            "criticalPoints",
            "watchout",
            "speakerNotes"
        ],
        "properties": {
            "tag": { "type": "string" },
            "title": { "type": "string" },
            "subtitle": { "type": "string" },
            "heroStatLabel": { "type": "string" },
            "criticalPoints": {
                "type": "array",
                "items": { "type": "string" }
            },
            "watchout": { "type": "string" },
            "speakerNotes": { "type": "string" }
        }
    })
}

/// Strict JSON Schema for the LLM-synthesized 8-slide walkthrough presentation.
pub fn walkthrough_slides_schema() -> Value {
    let slide_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "slideNumber",
            "id",
            "tag",
            "title",
            "subtitle",
            "heroStat",
            "criticalPoints",
            "watchout",
            "speakerNotes",
            "visualKind",
            "targetSection",
            "zh",
            "es"
        ],
        "properties": {
            "slideNumber": { "type": "integer" },
            "id": {
                "type": "string",
                "enum": [
                    "verdict",
                    "property_lot",
                    "zestimate_comps",
                    "cost_insurance",
                    "remodel_upside",
                    "risk_climate",
                    "schools_lifestyle",
                    "action_plan"
                ]
            },
            "tag": { "type": "string" },
            "title": { "type": "string" },
            "subtitle": { "type": "string" },
            "heroStat": {
                "type": "object",
                "additionalProperties": false,
                "required": ["value", "label", "tone"],
                "properties": {
                    "value": { "type": "string" },
                    "label": { "type": "string" },
                    "tone": { "type": "string", "enum": ["good", "info", "warn"] }
                }
            },
            "criticalPoints": {
                "type": "array",
                "items": { "type": "string" }
            },
            "watchout": { "type": "string" },
            "speakerNotes": { "type": "string" },
            "visualKind": {
                "type": "string",
                "enum": [
                    "verdict",
                    "property_lot",
                    "zestimate_comps",
                    "cost_insurance",
                    "remodel_upside",
                    "risk_climate",
                    "schools_lifestyle",
                    "action_plan"
                ]
            },
            "targetSection": { "type": "string" },
            "zh": localized_slide_schema(),
            "es": localized_slide_schema()
        }
    });

    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["deckSubtitle", "slides"],
        "properties": {
            "deckSubtitle": { "type": "string" },
            "slides": {
                "type": "array",
                "items": slide_schema
            }
        }
    })
}

/// Ultra-flat 9-string Structured Outputs schema (~200 output tokens) for sub-8s streaming,
/// merged onto `baseline` by [`normalize_llm_slides_into_deck`].
fn fast_slides_llm_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["deckSubtitle", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8"],
        "properties": {
            "deckSubtitle": { "type": "string" },
            "s1": { "type": "string" },
            "s2": { "type": "string" },
            "s3": { "type": "string" },
            "s4": { "type": "string" },
            "s5": { "type": "string" },
            "s6": { "type": "string" },
            "s7": { "type": "string" },
            "s8": { "type": "string" }
        }
    })
}

fn validate_llm_slides_payload(val: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    if val.get("s1").is_some() {
        for idx in 1..=8 {
            let key = format!("s{idx}");
            let ok = val
                .get(&key)
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !ok {
                errs.push(format!("missing or empty {key}"));
            }
        }
        return errs;
    }
    let Some(arr) = val.get("slides").and_then(|v| v.as_array()) else {
        errs.push("missing s1..s8 or slides".to_string());
        return errs;
    };
    if arr.len() != 8 {
        errs.push(format!("expected 8 slides, got {}", arr.len()));
        return errs;
    }
    for (idx, exp_id) in WALKTHROUGH_SLIDE_IDS.iter().enumerate() {
        let found = arr.iter().any(|s| {
            s.get("id").and_then(|v| v.as_str()) == Some(*exp_id)
                || s.get("slideNumber").and_then(|v| v.as_u64()) == Some((idx + 1) as u64)
        });
        if !found {
            errs.push(format!("missing slide slot {exp_id}"));
        }
    }
    errs
}

fn clip_str_val(v: Option<&Value>, max_chars: usize) -> Value {
    match v {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            let trimmed = s.trim();
            if trimmed.chars().count() <= max_chars {
                Value::String(trimmed.to_string())
            } else {
                let mut cut: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
                cut = cut.trim_end().to_string();
                cut.push('…');
                Value::String(cut)
            }
        }
        Some(other) if !other.is_null() => other.clone(),
        _ => Value::Null,
    }
}

fn build_compact_slides_digest(report: &Value, baseline: &Value) -> Value {
    let mut slots = serde_json::Map::new();
    if let Some(arr) = baseline.get("slides").and_then(|v| v.as_array()) {
        for (idx, s) in arr.iter().enumerate().take(8) {
            let pts: Vec<Value> = s
                .get("criticalPoints")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().take(2).map(|p| clip_str_val(Some(p), 90)).collect())
                .unwrap_or_default();
            let stat = format!(
                "{} ({})",
                s.pointer("/heroStat/value").and_then(|v| v.as_str()).unwrap_or(""),
                s.pointer("/heroStat/label").and_then(|v| v.as_str()).unwrap_or("")
            );
            slots.insert(
                format!("s{}", idx + 1),
                json!({
                    "stat": stat,
                    "pts": pts,
                    "watch": clip_str_val(s.get("watchout"), 90)
                }),
            );
        }
    }
    let negotiation: Vec<String> = pick_str_list(report.pointer("/ai/negotiation"), 2);
    let mut digest = json!({
        "address": report.pointer("/meta/displayAddress"),
        "score": report.pointer("/summary/overallScore"),
        "verdict": clip_str_val(report.pointer("/summary/verdict"), 160),
        "negotiation": negotiation,
        "slots": Value::Object(slots)
    });
    scrub_walkthrough_deck_value(&mut digest);
    digest
}

fn normalize_llm_slides_into_deck(llm_data: &Value, baseline: &Value) -> Value {
    let base_slides = baseline
        .get("slides")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let llm_slides = llm_data
        .get("slides")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut merged_slides = Vec::with_capacity(8);
    for (idx, spec) in SLIDE_SLOTS.iter().enumerate() {
        let base_slide = base_slides.get(idx).cloned().unwrap_or_else(|| json!({}));
        let slot_key = format!("s{}", idx + 1);
        let pipe_line = llm_data
            .get(&slot_key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let mut slide = if let Some(line) = pipe_line {
            let mut s = base_slide.clone();
            let parts: Vec<&str> = line
                .split('|')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect();
            if let Some(obj) = s.as_object_mut() {
                if parts.len() >= 3 {
                    obj.insert("title".to_string(), json!(parts[0]));
                    obj.insert("watchout".to_string(), json!(parts[1]));
                    obj.insert("speakerNotes".to_string(), json!(parts[2..].join(" — ")));
                } else if parts.len() == 2 {
                    obj.insert("title".to_string(), json!(parts[0]));
                    obj.insert("speakerNotes".to_string(), json!(parts[1]));
                } else if parts.len() == 1 {
                    obj.insert("speakerNotes".to_string(), json!(parts[0]));
                }
            }
            s
        } else {
            let found = llm_slides
                .iter()
                .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(spec.id))
                .or_else(|| {
                    llm_slides
                        .iter()
                        .find(|s| s.get("slideNumber").and_then(|v| v.as_u64()) == Some((idx + 1) as u64))
                });
            match found {
                Some(s) => s.clone(),
                None => base_slide.clone(),
            }
        };

        if let Some(obj) = slide.as_object_mut() {
            obj.insert("slideNumber".to_string(), json!(idx + 1));
            obj.insert("id".to_string(), json!(spec.id));
            obj.insert("visualKind".to_string(), json!(spec.visual_kind));
            obj.insert("targetSection".to_string(), json!(spec.target_section));

            // Backfill any missing/empty localized sub-fields from baseline
            for key in ["tag", "title", "subtitle", "watchout", "speakerNotes"] {
                let empty = obj
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true);
                if empty {
                    if let Some(bv) = base_slide.get(key) {
                        obj.insert(key.to_string(), bv.clone());
                    }
                }
            }
            if !obj
                .get("criticalPoints")
                .and_then(|v| v.as_array())
                .map(|a| a.len() >= 3)
                .unwrap_or(false)
            {
                if let Some(bp) = base_slide.get("criticalPoints") {
                    obj.insert("criticalPoints".to_string(), bp.clone());
                }
            }
            if !obj
                .get("heroStat")
                .and_then(|hs| hs.get("value"))
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                if let Some(bh) = base_slide.get("heroStat") {
                    obj.insert("heroStat".to_string(), bh.clone());
                }
            }
            for lang in ["zh", "es"] {
                if !obj.get(lang).map(|v| v.is_object()).unwrap_or(false) {
                    if let Some(bl) = base_slide.get(lang) {
                        obj.insert(lang.to_string(), bl.clone());
                    }
                } else if let (Some(l_obj), Some(b_obj)) = (
                    obj.get_mut(lang).and_then(Value::as_object_mut),
                    base_slide.get(lang).and_then(Value::as_object),
                ) {
                    for k in ["tag", "title", "subtitle", "heroStatLabel", "watchout", "speakerNotes"]
                    {
                        let empty = l_obj
                            .get(k)
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().is_empty())
                            .unwrap_or(true);
                        if empty {
                            if let Some(bv) = b_obj.get(k) {
                                l_obj.insert(k.to_string(), bv.clone());
                            }
                        }
                    }
                    if !l_obj
                        .get("criticalPoints")
                        .and_then(|v| v.as_array())
                        .map(|a| !a.is_empty())
                        .unwrap_or(false)
                    {
                        if let Some(bp) = b_obj.get("criticalPoints") {
                            l_obj.insert("criticalPoints".to_string(), bp.clone());
                        }
                    }
                }
            }
            if let Some(zh_val) = obj.get_mut("zh") {
                localize_compass_in_cjk_value(zh_val, false);
            }
        }
        merged_slides.push(slide);
    }

    let deck_title = baseline
        .get("deckTitle")
        .and_then(|v| v.as_str())
        .unwrap_or("Property Walkthrough")
        .to_string();
    let deck_subtitle = llm_data
        .get("deckSubtitle")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| "8-Slide AI-Enhanced Buyer Walkthrough Briefing".to_string());

    let mut out = json!({
        "generatedAt": now_iso(),
        "deckTitle": deck_title,
        "deckSubtitle": deck_subtitle,
        "synthesisMode": "ai_enhanced",
        "slides": merged_slides
    });
    scrub_walkthrough_deck_value(&mut out);
    out
}

/// Calls Open Estate's configured OpenAI Responses LLM client over a compact digest of `report`
/// using explicit `http`, `env`, and optional L2 `cache`, falling back cleanly to
/// [`build_deterministic_walkthrough_slides`] when credentials or network are unavailable.
pub async fn generate_llm_walkthrough_slides_with_client(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    cache: Option<&dyn LlmCache>,
    refresh: bool,
) -> Value {
    let baseline = build_deterministic_walkthrough_slides(report);
    if !env.llm_configured() {
        return baseline;
    }

    let digest = build_compact_slides_digest(report, &baseline);
    let Ok(digest_json) = serde_json::to_string(&digest) else {
        return baseline;
    };

    let prompt = format!(
        "Synthesize the 8-slide buyer walkthrough presentation strictly grounded in this property digest.\n\nPROPERTY_DIGEST_JSON:\n{digest_json}"
    );
    let instructions = crate::shared::apply_brand_to_str(
        SLIDES_INSTRUCTIONS,
        &env.brand_name(),
        &env.brand_domain(),
    );
    let mut opts = LlmCallOptions::new(
        "report_synthesis",
        "open_estate_slides_brief",
        instructions,
        prompt,
        fast_slides_llm_schema(),
    );
    opts.models = resolve_interactive_model_specs(env);

    let per_model_ms = env
        .get("SLIDES_LLM_TIMEOUT_MS")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(16_500);
    let total_budget_ms = env
        .get("SLIDES_TOTAL_TIMEOUT_MS")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| per_model_ms.saturating_add(1_500).clamp(500, 18_000));
    opts.timeout_ms = Some(per_model_ms);
    opts.max_output_tokens = Some(8_192);
    opts.refresh = refresh;
    opts.validator = Some(Arc::new(validate_llm_slides_payload));

    match tokio::time::timeout(
        Duration::from_millis(total_budget_ms),
        responses_json_with_cache(http, env, opts, cache),
    )
    .await
    {
        Ok(Ok(res)) => {
            let normalized = normalize_llm_slides_into_deck(&res.data, &baseline);
            if is_valid_walkthrough_deck(&normalized) {
                normalized
            } else {
                baseline
            }
        }
        Ok(Err(err)) => {
            tracing::warn!(
                error = %scrub_string(&err.to_string()),
                "walkthrough slides LLM synthesis failed; returning deterministic deck"
            );
            baseline
        }
        Err(_) => {
            tracing::warn!(
                total_budget_ms,
                "walkthrough slides LLM synthesis timed out within interactive budget; returning deterministic deck"
            );
            baseline
        }
    }
}

/// Synthesizes an 8-slide home walkthrough deck using explicit process environment variables
/// (`OPENAI_BASE_URL` + `OPENAI_API_KEY`, without implicitly reading ambient `.env` files from
/// `current_dir()`), falling back cleanly and fast to [`build_deterministic_walkthrough_slides`]
/// whenever LLM credentials or upstream API are unavailable.
pub async fn generate_llm_walkthrough_slides(report: &Value) -> Value {
    let env = EnvConfig::from_process_and_dotenv(None);
    if !env.llm_configured() {
        return build_deterministic_walkthrough_slides(report);
    }
    let http = HttpClient::from_env(&env, None);
    generate_llm_walkthrough_slides_with_client(&http, &env, report, None, false).await
}

/// Reusable helper for `POST /api/report/:slug/slides` handlers with explicit `HttpClient` and `EnvConfig`.
///
/// Updates `report["walkthroughSlides"]` via [`generate_llm_walkthrough_slides_with_client`] and returns
/// `{ "ok": true, "walkthroughSlides": ... }` with all internal metadata scrubbed.
pub async fn generate_slides_for_report_json_with_client(
    http: &HttpClient,
    env: &EnvConfig,
    mut report: Value,
) -> Value {
    let mut deck =
        generate_llm_walkthrough_slides_with_client(http, env, &report, None, false).await;
    scrub_walkthrough_deck_value(&mut deck);
    if let Some(obj) = report.as_object_mut() {
        obj.insert("walkthroughSlides".to_string(), deck.clone());
    }
    json!({
        "ok": true,
        "walkthroughSlides": deck
    })
}

/// Reusable helper for `POST /api/report/:slug/slides` handlers.
///
/// Updates `report["walkthroughSlides"]` via [`generate_llm_walkthrough_slides`] and returns
/// `{ "ok": true, "walkthroughSlides": ... }` with all internal metadata scrubbed.
///
/// ```rust,ignore
/// // Example 5-line Axum route handler in `src/web/server.rs`:
/// async fn handle_report_slides(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
///     let Some(mut rec) = get_cached_or_disk_report(&state, &slug).await else { return handle_not_found().await };
///     let out = open_estate::ai::slides::generate_slides_for_report_json(rec.report.clone()).await;
///     rec.report["walkthroughSlides"] = out["walkthroughSlides"].clone();
///     let _ = state.db.put_cached_report(&rec.slug, &rec.report, None).await;
///     json_response(StatusCode::OK, out)
/// }
/// ```
pub async fn generate_slides_for_report_json(report: Value) -> Value {
    let env = EnvConfig::from_process_and_dotenv(None);
    let http = HttpClient::from_env(&env, None);
    generate_slides_for_report_json_with_client(&http, &env, report).await
}
