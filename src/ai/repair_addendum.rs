// Exportable Request for Repair / Seller Credit Addendum Generator (`POST /api/repair-addendum`).
// Given a packet review, inspection review, title/HOA review, or custom defect selection +
// regional contractor cost multiplier (calibrated by state/metro from `report.json`), generates:
// 1. Structured JSON grouping selected defects into the 7 canonical trades:
//    - `Roofing`
//    - `Wood Destroying Pests (Section 1)`
//    - `Plumbing / Sewer`
//    - `Electrical`
//    - `HVAC`
//    - `Foundation / Drainage`
//    - `Title / Lien Cure`
// 2. Exact page citations, itemized regionally calibrated contractor cost ranges (`low / mid / high`),
//    and per-item + portfolio recommendations across:
//    - **Seller Credit Ask** (up to lender Interested Party Contribution cap)
//    - **Price Reduction** (for excess above IPC cap + lower property-tax basis)
//    - **Pre-Close Licensed Repair / Escrow Payoff** (for title liens, HOA assessments & lender-mandated safety/WDO items)
// 3. Polite, firm buyer-agent negotiation copy (`buyerAgentEmailScript` & `addendumClauseText`).
// 4. Clean Markdown (`markdownSchedule`) and XSS-safe, `@media print`-styled HTML (`printableHtml`).

use crate::ai::doc_pipeline::{init_deterministic_ai_meta, stamp_ai_enhancement_meta};
use crate::ai::hoa_review::DocPageCitation;
use crate::ai::llm::{
    resolve_model_specs_with_effort, responses_json_with_cache, LlmCache, LlmCallOptions,
};
use crate::ai::packet_review::calibrate_regional_cost_multiplier;
use crate::ai::title_review::redact_title_pii;
use crate::guardrails::{
    clean_untrusted_text, inspect_output_guardrail, scrub_sensitive_text, wrap_untrusted_block,
    GuardrailError,
};
use crate::shared::{EnvConfig, HttpClient, ModuleOutput, SourceItem};
use anyhow::Result;
use html_escape::encode_text;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

pub const ADDENDUM_TRADES: &[&str] = &[
    "Roofing",
    "Wood Destroying Pests (Section 1)",
    "Plumbing / Sewer",
    "Electrical",
    "HVAC",
    "Foundation / Drainage",
    "Title / Lien Cure",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairAddendumInput {
    #[serde(default)]
    pub packet_review: Option<Value>,
    #[serde(default)]
    pub inspection_review: Option<Value>,
    #[serde(default)]
    pub title_review: Option<Value>,
    #[serde(default)]
    pub hoa_review: Option<Value>,
    #[serde(default)]
    pub defects: Option<Vec<Value>>,
    #[serde(default)]
    pub selected_defect_ids: Option<Vec<String>>,
    #[serde(default)]
    pub regional_multiplier: Option<f64>,
    #[serde(default)]
    pub preferred_remedy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddendumLineItem {
    pub id: String,
    pub trade: &'static str,
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub cost_low: i64,
    pub cost_mid: i64,
    pub cost_high: i64,
    pub recommended_remedy: &'static str,
    pub citations: Vec<DocPageCitation>,
}

pub fn normalize_to_canonical_trade(raw: &str) -> &'static str {
    let low = raw.trim().to_ascii_lowercase();
    if low.contains("roof") || low.contains("gutter") || low.contains("flashing") || low.contains("shingle") {
        "Roofing"
    } else if low.contains("pest")
        || low.contains("wdo")
        || low.contains("termite")
        || low.contains("section 1")
        || low.contains("fungus")
        || low.contains("dry rot")
    {
        "Wood Destroying Pests (Section 1)"
    } else if low.contains("plumb")
        || low.contains("sewer")
        || low.contains("water heater")
        || low.contains("pipe")
        || low.contains("lateral")
    {
        "Plumbing / Sewer"
    } else if low.contains("electr") || low.contains("panel") || low.contains("wiring") || low.contains("breaker") {
        "Electrical"
    } else if low.contains("hvac")
        || low.contains("furnace")
        || low.contains("heating")
        || low.contains("cooling")
        || low.contains("air condition")
    {
        "HVAC"
    } else if low.contains("title")
        || low.contains("lien")
        || low.contains("ucc")
        || low.contains("assessment")
        || low.contains("hoa")
        || low.contains("escrow")
    {
        "Title / Lien Cure"
    } else {
        "Foundation / Drainage"
    }
}

fn recommend_item_remedy(trade: &str, severity: &str, title: &str) -> &'static str {
    let tl = title.to_ascii_lowercase();
    if trade == "Title / Lien Cure" {
        "Pre-Close Escrow Payoff & Lien Release"
    } else if trade == "Wood Destroying Pests (Section 1)"
        || tl.contains("cracked heat exchanger")
        || tl.contains("active gas")
    {
        "Pre-Close Licensed Repair / Section 1 Clearance (or Full Escrow Credit)"
    } else if severity.eq_ignore_ascii_case("Critical") || severity.eq_ignore_ascii_case("Major") {
        "Seller Closing Credit"
    } else {
        "Seller Closing Credit / Price Reduction"
    }
}

fn format_usd(n: i64) -> String {
    let abs = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(abs.len() + 6);
    if n < 0 {
        out.push('-');
    }
    out.push('$');
    for (idx, ch) in abs.chars().enumerate() {
        if idx > 0 && (abs.len() - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn extract_citations_from_value(v: &Value, default_doc: &str) -> Vec<DocPageCitation> {
    if let Some(arr) = v.get("citations").and_then(|c| c.as_array()) {
        let parsed: Vec<DocPageCitation> = arr
            .iter()
            .filter_map(|item| {
                let doc_name = item
                    .get("docName")
                    .and_then(|x| x.as_str())
                    .unwrap_or(default_doc)
                    .to_string();
                let page = item.get("page").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
                let quote = item
                    .get("quote")
                    .and_then(|x| x.as_str())
                    .map(|s| redact_title_pii(s).masked_text)
                    .unwrap_or_default();
                Some(DocPageCitation {
                    doc_name: clean_untrusted_text(&doc_name, 120),
                    page: page.max(1),
                    quote: clean_untrusted_text(&quote, 220),
                })
            })
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    if let Some(pc) = v.get("primaryCitation").or_else(|| v.get("citation")) {
        if pc.is_object() {
            let doc_name = pc
                .get("docName")
                .and_then(|x| x.as_str())
                .unwrap_or(default_doc);
            let page = pc.get("page").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
            let quote = pc
                .get("quote")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            return vec![DocPageCitation {
                doc_name: clean_untrusted_text(doc_name, 120),
                page: page.max(1),
                quote: clean_untrusted_text(&redact_title_pii(quote).masked_text, 220),
            }];
        }
    }
    let page_ref = v
        .get("pageRef")
        .and_then(|x| x.as_str())
        .unwrap_or("Page 1");
    let page_num: u32 = page_ref
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse::<u32>()
        .unwrap_or(1)
        .max(1);
    let detail = v
        .get("detail")
        .or_else(|| v.get("excerpt"))
        .or_else(|| v.get("item"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    vec![DocPageCitation {
        doc_name: default_doc.to_string(),
        page: page_num,
        quote: clean_untrusted_text(&redact_title_pii(detail).masked_text, 220),
    }]
}

fn collect_line_items(
    input: &RepairAddendumInput,
    target_mult: f64,
) -> std::result::Result<Vec<AddendumLineItem>, GuardrailError> {
    let selected_filter: Option<HashSet<String>> = input
        .selected_defect_ids
        .as_ref()
        .filter(|ids| !ids.is_empty())
        .map(|ids| ids.iter().map(|s| s.trim().to_ascii_lowercase()).collect());

    let passes_filter = |id: &str, title: &str| -> bool {
        let Some(ref set) = selected_filter else {
            return true;
        };
        set.contains(&id.to_ascii_lowercase()) || set.contains(&title.to_ascii_lowercase())
    };

    let mut items: Vec<AddendumLineItem> = Vec::new();
    let mut seen_dedup: HashSet<String> = HashSet::new();

    // 1. Explicit `defects` array (if caller supplied custom/selected items)
    if let Some(ref custom_list) = input.defects {
        for (idx, raw) in custom_list.iter().enumerate() {
            let id = raw
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| clean_untrusted_text(s, 80))
                .unwrap_or_else(|| format!("custom-defect-{}", idx + 1));
            let title = raw
                .get("title")
                .or_else(|| raw.get("item"))
                .and_then(|v| v.as_str())
                .map(|s| clean_untrusted_text(&redact_title_pii(s).masked_text, 160))
                .unwrap_or_else(|| format!("Repair Item #{}", idx + 1));
            if !passes_filter(&id, &title) {
                continue;
            }
            let raw_trade = raw
                .get("trade")
                .or_else(|| raw.get("system"))
                .and_then(|v| v.as_str())
                .unwrap_or("Foundation / Drainage");
            let trade = normalize_to_canonical_trade(raw_trade);
            let severity = raw
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("Major")
                .to_string();
            let detail = raw
                .get("detail")
                .and_then(|v| v.as_str())
                .map(|s| clean_untrusted_text(&redact_title_pii(s).masked_text, 240))
                .unwrap_or_else(|| title.clone());
            let already_scaled = raw
                .get("alreadyRegionallyScaled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let scale = if already_scaled { 1.0 } else { target_mult };
            let lo_raw = raw
                .get("costLow")
                .or_else(|| raw.get("baseCostLow"))
                .and_then(|v| v.as_f64())
                .unwrap_or(1_500.0);
            let hi_raw = raw
                .get("costHigh")
                .or_else(|| raw.get("baseCostHigh"))
                .and_then(|v| v.as_f64())
                .unwrap_or(lo_raw * 1.5);
            let cost_low = ((lo_raw * scale).round() as i64).max(0);
            let cost_high = ((hi_raw * scale).round() as i64).max(cost_low);
            let cost_mid = (cost_low + cost_high) / 2;
            let citations = extract_citations_from_value(raw, "Inspection_Document.pdf");
            let sig = format!("{}:{}", trade, title.to_ascii_lowercase());
            if seen_dedup.insert(sig) {
                items.push(AddendumLineItem {
                    id,
                    trade,
                    recommended_remedy: recommend_item_remedy(trade, &severity, &title),
                    severity,
                    title,
                    detail,
                    cost_low,
                    cost_mid,
                    cost_high,
                    citations,
                });
            }
        }
    }

    // 2. `packetReview.deduplicatedDefects`
    if let Some(ref pr) = input.packet_review {
        let source_mult = pr
            .pointer("/totalExposureUsd/regionalMultiplier")
            .and_then(|v| v.as_f64())
            .filter(|&m| m > 0.0)
            .unwrap_or(target_mult);
        if let Some(arr) = pr.get("deduplicatedDefects").and_then(|v| v.as_array()) {
            for (idx, d) in arr.iter().enumerate() {
                let id = d
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("packet-defect-{}", idx + 1));
                let title = d
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|s| redact_title_pii(s).masked_text)
                    .unwrap_or_else(|| "Packet Defect".to_string());
                if !passes_filter(&id, &title) {
                    continue;
                }
                let raw_trade = d
                    .get("trade")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Foundation / Drainage");
                let trade = normalize_to_canonical_trade(raw_trade);
                let is_liability = d
                    .get("isLiability")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let ratio = if is_liability {
                    1.0
                } else {
                    target_mult / source_mult
                };
                let lo = d.get("costLow").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let hi = d.get("costHigh").and_then(|v| v.as_f64()).unwrap_or(lo);
                let cost_low = ((lo * ratio).round() as i64).max(0);
                let cost_high = ((hi * ratio).round() as i64).max(cost_low);
                let cost_mid = (cost_low + cost_high) / 2;
                let severity = d
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Major")
                    .to_string();
                let detail = d
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .map(|s| redact_title_pii(s).masked_text)
                    .unwrap_or_default();
                let citations = extract_citations_from_value(d, "Disclosure_Packet.pdf");
                let sig = format!("{}:{}", trade, title.to_ascii_lowercase());
                if seen_dedup.insert(sig) {
                    items.push(AddendumLineItem {
                        id,
                        trade,
                        recommended_remedy: recommend_item_remedy(trade, &severity, &title),
                        severity,
                        title,
                        detail,
                        cost_low,
                        cost_mid,
                        cost_high,
                        citations,
                    });
                }
            }
        }
    }

    // 3. `inspectionReview.findings` (from `/api/inspect`)
    if let Some(ref ir) = input.inspection_review {
        let default_doc = ir
            .pointer("/files/0/name")
            .and_then(|v| v.as_str())
            .unwrap_or("Home_Inspection.pdf");
        if let Some(arr) = ir.get("findings").and_then(|v| v.as_array()) {
            for (idx, f) in arr.iter().enumerate() {
                let id = format!("inspect-finding-{}", idx + 1);
                let title = f
                    .get("item")
                    .or_else(|| f.get("title"))
                    .and_then(|v| v.as_str())
                    .map(|s| redact_title_pii(s).masked_text)
                    .unwrap_or_else(|| format!("Inspection Finding #{}", idx + 1));
                if !passes_filter(&id, &title) {
                    continue;
                }
                let raw_sys = f
                    .get("system")
                    .or_else(|| f.get("trade"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Foundation / Drainage");
                let trade = normalize_to_canonical_trade(raw_sys);
                let sev_raw = f.get("severity").and_then(|v| v.as_str()).unwrap_or("major");
                let severity = match sev_raw.to_ascii_lowercase().as_str() {
                    "critical" => "Critical".to_string(),
                    "major" => "Major".to_string(),
                    "moderate" => "Moderate".to_string(),
                    _ => "Informational".to_string(),
                };
                let detail = f
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .map(|s| redact_title_pii(s).masked_text)
                    .unwrap_or_default();
                let cost_low = f.get("costLow").and_then(|v| v.as_i64()).unwrap_or(1_000).max(0);
                let cost_high = f
                    .get("costHigh")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(cost_low)
                    .max(cost_low);
                let cost_mid = (cost_low + cost_high) / 2;
                let citations = extract_citations_from_value(f, default_doc);
                let sig = format!("{}:{}", trade, title.to_ascii_lowercase());
                if seen_dedup.insert(sig) {
                    items.push(AddendumLineItem {
                        id,
                        trade,
                        recommended_remedy: recommend_item_remedy(trade, &severity, &title),
                        severity,
                        title,
                        detail,
                        cost_low,
                        cost_mid,
                        cost_high,
                        citations,
                    });
                }
            }
        }
    }

    // 4. `titleReview.involuntaryLiens` (from `/api/title-review`)
    if let Some(ref tr) = input.title_review {
        let default_doc = tr
            .pointer("/files/0/name")
            .and_then(|v| v.as_str())
            .unwrap_or("Prelim_Title.pdf");
        if let Some(liens) = tr.get("involuntaryLiens").and_then(|v| v.as_array()) {
            for (idx, l) in liens.iter().enumerate() {
                if l.get("status").and_then(|v| v.as_str()) != Some("active") {
                    continue;
                }
                let id = format!("title-lien-{}", idx + 1);
                let ltype = l
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Involuntary Lien");
                let title = format!("Schedule B Lien Payoff & Release: {ltype}");
                if !passes_filter(&id, &title) {
                    continue;
                }
                let amt = l.get("amount").and_then(|v| v.as_i64()).unwrap_or(10_000);
                let excerpt = l
                    .get("excerpt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Active Schedule B lien");
                let sig = format!("Title / Lien Cure:{}", title.to_ascii_lowercase());
                if seen_dedup.insert(sig) {
                    items.push(AddendumLineItem {
                        id,
                        trade: "Title / Lien Cure",
                        recommended_remedy: "Pre-Close Escrow Payoff & Lien Release",
                        severity: "Critical".to_string(),
                        title,
                        detail: redact_title_pii(excerpt).masked_text,
                        cost_low: amt,
                        cost_mid: amt,
                        cost_high: amt,
                        citations: vec![DocPageCitation {
                            doc_name: default_doc.to_string(),
                            page: 1,
                            quote: clean_untrusted_text(&redact_title_pii(excerpt).masked_text, 220),
                        }],
                    });
                }
            }
        }
    }

    // 5. `hoaReview.financialHealth.specialAssessments` (from `/api/hoa-review`)
    if let Some(ref hr) = input.hoa_review {
        if let Some(assessments) = hr
            .pointer("/financialHealth/specialAssessments")
            .and_then(|v| v.as_array())
        {
            for (idx, sa) in assessments.iter().enumerate() {
                let id = format!("hoa-assessment-{}", idx + 1);
                let purpose = sa
                    .get("purpose")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HOA Special Assessment");
                let title = format!("Seller Payoff of HOA Special Assessment: {purpose}");
                if !passes_filter(&id, &title) {
                    continue;
                }
                let amt = sa
                    .get("perUnitAmountUsd")
                    .and_then(|v| v.as_i64())
                    .or_else(|| sa.get("amountUsd").and_then(|v| v.as_i64()))
                    .unwrap_or(8_000);
                let citations = extract_citations_from_value(sa, "HOA_Reserve_Packet.pdf");
                let detail = citations
                    .first()
                    .map(|c| c.quote.clone())
                    .unwrap_or_else(|| purpose.to_string());
                let sig = format!("Title / Lien Cure:{}", title.to_ascii_lowercase());
                if seen_dedup.insert(sig) {
                    items.push(AddendumLineItem {
                        id,
                        trade: "Title / Lien Cure",
                        recommended_remedy: "Pre-Close Escrow Payoff & Lien Release",
                        severity: "Critical".to_string(),
                        title,
                        detail,
                        cost_low: amt,
                        cost_mid: amt,
                        cost_high: amt,
                        citations,
                    });
                }
            }
        }
    }

    if items.is_empty() {
        return Err(GuardrailError::new(
            "NO_DEFECTS_SELECTED",
            "Provide `packetReview`, `inspectionReview`, `titleReview`, `hoaReview`, or a non-empty `defects` list (or review the packet first) to generate a Repair & Credit Addendum.",
            400,
        ));
    }

    Ok(items)
}

fn render_markdown_schedule(
    address: &str,
    reg_label: &str,
    grouped: &[Value],
    total_low: i64,
    total_mid: i64,
    total_high: i64,
    credit_ask: i64,
    price_reduction: i64,
    escrow_cure: i64,
    clause_text: &str,
    agent_email: &str,
) -> String {
    let mut md = String::with_capacity(4096);
    md.push_str(&format!("# Request for Repair & Seller Credit Addendum Schedule\n\n"));
    md.push_str(&format!("**Subject Property:** {address}  \n"));
    md.push_str(&format!("**Regional Contractor Cost Basis:** {reg_label}  \n"));
    md.push_str(&format!(
        "**Total Estimated Exposure Range:** {} – {} (Midpoint: {})\n\n",
        format_usd(total_low),
        format_usd(total_high),
        format_usd(total_mid)
    ));

    md.push_str("## Recommended Cure Allocation\n\n");
    md.push_str(&format!(
        "- **Seller Closing Cost / Repair Credit Ask:** {}\n",
        format_usd(credit_ask)
    ));
    md.push_str(&format!(
        "- **Purchase Price Reduction Ask:** {}\n",
        format_usd(price_reduction)
    ));
    md.push_str(&format!(
        "- **Pre-Close Licensed Repair / Escrow Lien & Assessment Payoff:** {}\n\n",
        format_usd(escrow_cure)
    ));

    md.push_str("## Itemized Schedule by Trade\n\n");
    for grp in grouped {
        let trade = grp.get("trade").and_then(|v| v.as_str()).unwrap_or("Trade");
        let sub_lo = grp.get("subtotalLowUsd").and_then(|v| v.as_i64()).unwrap_or(0);
        let sub_hi = grp.get("subtotalHighUsd").and_then(|v| v.as_i64()).unwrap_or(0);
        md.push_str(&format!(
            "### {} ({} – {})\n\n",
            trade,
            format_usd(sub_lo),
            format_usd(sub_hi)
        ));
        md.push_str("| Severity | Defect / Cure Item | Citation | Regional Est. Range | Recommended Remedy |\n");
        md.push_str("|---|---|---|---|---|\n");
        if let Some(arr) = grp.get("items").and_then(|v| v.as_array()) {
            for item in arr {
                let sev = item.get("severity").and_then(|v| v.as_str()).unwrap_or("Major");
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .replace('|', "/");
                let lo = item.get("costLow").and_then(|v| v.as_i64()).unwrap_or(0);
                let hi = item.get("costHigh").and_then(|v| v.as_i64()).unwrap_or(0);
                let rem = item
                    .get("recommendedRemedy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Seller Closing Credit")
                    .replace('|', "/");
                let cit_str = item
                    .get("citations")
                    .and_then(|v| v.as_array())
                    .map(|cs| {
                        cs.iter()
                            .map(|c| {
                                format!(
                                    "{} p.{}",
                                    c.get("docName").and_then(|x| x.as_str()).unwrap_or("Doc"),
                                    c.get("page").and_then(|x| x.as_u64()).unwrap_or(1)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_else(|| "Page 1".to_string())
                    .replace('|', "/");
                md.push_str(&format!(
                    "| {} | {} | {} | {} – {} | {} |\n",
                    sev,
                    title,
                    cit_str,
                    format_usd(lo),
                    format_usd(hi),
                    rem
                ));
            }
        }
        md.push('\n');
    }

    md.push_str("## Proposed Addendum Clause\n\n");
    md.push_str(&format!("> {clause_text}\n\n"));
    md.push_str("## Buyer-Agent Transmittal Note\n\n");
    md.push_str(agent_email);
    md.push('\n');
    md
}

fn render_printable_html_schedule(
    address: &str,
    reg_label: &str,
    grouped: &[Value],
    total_low: i64,
    total_mid: i64,
    total_high: i64,
    credit_ask: i64,
    price_reduction: i64,
    escrow_cure: i64,
    clause_text: &str,
    agent_email: &str,
) -> String {
    let mut rows_html = String::with_capacity(4096);
    for grp in grouped {
        let trade = grp.get("trade").and_then(|v| v.as_str()).unwrap_or("Trade");
        let sub_lo = grp.get("subtotalLowUsd").and_then(|v| v.as_i64()).unwrap_or(0);
        let sub_hi = grp.get("subtotalHighUsd").and_then(|v| v.as_i64()).unwrap_or(0);
        rows_html.push_str(&format!(
            "<section class=\"trade-block\"><h3>{} <span class=\"sub\">({} – {})</span></h3><table><thead><tr><th>Severity</th><th>Item &amp; Finding</th><th>Document Citation</th><th>Est. Range</th><th>Remedy</th></tr></thead><tbody>",
            encode_text(trade),
            encode_text(&format_usd(sub_lo)),
            encode_text(&format_usd(sub_hi)),
        ));
        if let Some(arr) = grp.get("items").and_then(|v| v.as_array()) {
            for item in arr {
                let sev = item.get("severity").and_then(|v| v.as_str()).unwrap_or("Major");
                let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let detail = item.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                let lo = item.get("costLow").and_then(|v| v.as_i64()).unwrap_or(0);
                let hi = item.get("costHigh").and_then(|v| v.as_i64()).unwrap_or(0);
                let rem = item
                    .get("recommendedRemedy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Seller Closing Credit");
                let cits = item
                    .get("citations")
                    .and_then(|v| v.as_array())
                    .map(|cs| {
                        cs.iter()
                            .map(|c| {
                                let dn = c.get("docName").and_then(|x| x.as_str()).unwrap_or("Doc");
                                let pg = c.get("page").and_then(|x| x.as_u64()).unwrap_or(1);
                                format!("{} (p.{})", encode_text(dn), pg)
                            })
                            .collect::<Vec<_>>()
                            .join("<br>")
                    })
                    .unwrap_or_else(|| "p.1".to_string());

                rows_html.push_str(&format!(
                    "<tr><td><span class=\"badge\">{sev_e}</span></td><td><strong>{title_e}</strong><div class=\"detail\">{det_e}</div></td><td>{cits}</td><td class=\"num\">{lo_e} – {hi_e}</td><td>{rem_e}</td></tr>",
                    sev_e = encode_text(sev),
                    title_e = encode_text(title),
                    det_e = encode_text(detail),
                    cits = cits,
                    lo_e = encode_text(&format_usd(lo)),
                    hi_e = encode_text(&format_usd(hi)),
                    rem_e = encode_text(rem),
                ));
            }
        }
        rows_html.push_str("</tbody></table></section>");
    }

    format!(
        concat!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">",
            "<title>Request for Repair &amp; Seller Credit Schedule — {addr}</title>",
            "<style>",
            "body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;color:#1e293b;margin:28px auto;max-width:960px;line-height:1.45;}}",
            "h1{{font-size:22px;margin:0 0 6px;color:#0f172a;}}h2{{font-size:16px;margin:22px 0 8px;border-bottom:1px solid #cbd5e1;padding-bottom:4px;}}h3{{font-size:14px;margin:16px 0 6px;color:#0f172a;}}",
            ".meta{{color:#475569;font-size:13px;margin-bottom:16px;}}.cards{{display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin:14px 0;}}",
            ".card{{border:1px solid #cbd5e1;border-radius:6px;padding:10px;background:#f8fafc;}}.card .lbl{{font-size:11px;color:#64748b;text-transform:uppercase;}}.card .val{{font-size:16px;font-weight:700;color:#0f172a;margin-top:2px;}}",
            "table{{width:100%;border-collapse:collapse;font-size:12.5px;margin-bottom:10px;}}th,td{{border:1px solid #cbd5e1;padding:7px 8px;vertical-align:top;text-align:left;}}th{{background:#f1f5f9;font-weight:600;}}",
            ".num{{white-space:nowrap;font-weight:600;}}.detail{{color:#475569;font-size:11.5px;margin-top:2px;}}.sub{{color:#475569;font-weight:500;}}",
            ".badge{{display:inline-block;padding:2px 6px;border-radius:4px;background:#e2e8f0;font-size:11px;font-weight:600;}}",
            ".clause{{border-left:3px solid #0284c7;background:#f0f9ff;padding:10px 14px;font-size:13px;margin:8px 0;}}",
            "@media print{{body{{margin:10mm;max-width:none;}}.card{{background:#fff;}}}}",
            "</style></head><body>",
            "<h1>Request for Repair &amp; Seller Credit Addendum Schedule</h1>",
            "<div class=\"meta\"><strong>Subject Property:</strong> {addr} &nbsp;|&nbsp; <strong>Regional Pricing Basis:</strong> {reg}</div>",
            "<div class=\"cards\">",
            "<div class=\"card\"><div class=\"lbl\">Total Exposure Range</div><div class=\"val\">{t_lo} – {t_hi}</div></div>",
            "<div class=\"card\"><div class=\"lbl\">Seller Credit Ask</div><div class=\"val\">{c_ask}</div></div>",
            "<div class=\"card\"><div class=\"lbl\">Price Reduction Ask</div><div class=\"val\">{p_red}</div></div>",
            "<div class=\"card\"><div class=\"lbl\">Escrow / Pre-Close Cure</div><div class=\"val\">{e_cure}</div></div>",
            "</div>",
            "<h2>Itemized Schedule by Trade</h2>{rows}",
            "<h2>Proposed Contract Addendum Clause</h2><div class=\"clause\">{clause}</div>",
            "<h2>Buyer-Agent Negotiation Note</h2><p>{email}</p>",
            "<p class=\"meta\">Ballpark regional contractor estimates for buyer negotiation support only (mid: {t_mid}) — not a binding contractor bid or legal advice.</p>",
            "</body></html>"
        ),
        addr = encode_text(address),
        reg = encode_text(reg_label),
        t_lo = encode_text(&format_usd(total_low)),
        t_mid = encode_text(&format_usd(total_mid)),
        t_hi = encode_text(&format_usd(total_high)),
        c_ask = encode_text(&format_usd(credit_ask)),
        p_red = encode_text(&format_usd(price_reduction)),
        e_cure = encode_text(&format_usd(escrow_cure)),
        rows = rows_html,
        clause = encode_text(clause_text),
        email = encode_text(agent_email),
    )
}

/// Deterministic builder for the Repair & Credit Addendum JSON + Markdown + Printable HTML.
pub fn build_repair_addendum_deterministic(
    report: &Value,
    input: &RepairAddendumInput,
) -> std::result::Result<Value, GuardrailError> {
    let (reg_mult, reg_label) =
        calibrate_regional_cost_multiplier(Some(report), input.regional_multiplier);
    let line_items = collect_line_items(input, reg_mult)?;

    let display_address = report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .and_then(|v| v.as_str())
        .unwrap_or("Subject Residential Property");

    let property_value_usd = report
        .pointer("/modules/valuation/data/estimate/point")
        .or_else(|| report.pointer("/modules/valuation/data/ownership/price"))
        .or_else(|| report.pointer("/facts/lastSale/price"))
        .and_then(|v| v.as_f64())
        .filter(|&v| v > 50_000.0)
        .unwrap_or(750_000.0);

    // Conventional 80% LTV Interested Party Contribution (IPC) cap is typically 3% of purchase price
    // (6% at >=75%–90% LTV); use conservative 3% cap so buyers never forfeit excess closing credits!
    let lender_ipc_credit_cap_usd = ((property_value_usd * 0.03) / 100.0).round() as i64 * 100;

    let mut by_trade: BTreeMap<&'static str, Vec<&AddendumLineItem>> = BTreeMap::new();
    for item in &line_items {
        by_trade.entry(item.trade).or_default().push(item);
    }

    let mut grouped_trades = Vec::new();
    let mut total_low: i64 = 0;
    let mut total_mid: i64 = 0;
    let mut total_high: i64 = 0;
    let mut escrow_or_preclose_usd: i64 = 0;
    let mut negotiable_physical_mid_usd: i64 = 0;

    for &trade in ADDENDUM_TRADES {
        let Some(list) = by_trade.get(trade) else {
            continue;
        };
        let mut sub_lo = 0i64;
        let mut sub_mid = 0i64;
        let mut sub_hi = 0i64;
        let mut items_json = Vec::with_capacity(list.len());

        for it in list {
            sub_lo += it.cost_low;
            sub_mid += it.cost_mid;
            sub_hi += it.cost_high;
            if it.trade == "Title / Lien Cure" {
                escrow_or_preclose_usd += it.cost_mid;
            } else {
                negotiable_physical_mid_usd += it.cost_mid;
            }
            items_json.push(json!({
                "id": it.id,
                "trade": it.trade,
                "severity": it.severity,
                "title": it.title,
                "detail": it.detail,
                "costLow": it.cost_low,
                "costMid": it.cost_mid,
                "costHigh": it.cost_high,
                "recommendedRemedy": it.recommended_remedy,
                "citations": it.citations,
            }));
        }

        total_low += sub_lo;
        total_mid += sub_mid;
        total_high += sub_hi;

        grouped_trades.push(json!({
            "trade": trade,
            "itemCount": list.len(),
            "subtotalLowUsd": sub_lo,
            "subtotalMidUsd": sub_mid,
            "subtotalHighUsd": sub_hi,
            "items": items_json,
        }));
    }

    let pref = input
        .preferred_remedy
        .as_deref()
        .unwrap_or("auto")
        .to_ascii_lowercase();

    let (recommended_credit_usd, recommended_price_reduction_usd, primary_strategy) =
        if pref.contains("price") {
            (
                0i64,
                negotiable_physical_mid_usd,
                "Purchase Price Reduction + Escrow Lien/Assessment Payoff",
            )
        } else if pref.contains("repair") {
            (
                0i64,
                0i64,
                "Pre-Close Licensed Contractor Repair & Section 1 Clearance",
            )
        } else if negotiable_physical_mid_usd <= lender_ipc_credit_fetching_cap(lender_ipc_credit_cap_usd) {
            (
                negotiable_physical_mid_usd,
                0i64,
                if escrow_or_preclose_usd > 0 {
                    "Seller Closing Credit + Seller Escrow Payoff of Liens/Assessments"
                } else {
                    "Seller Closing Cost Credit (Buyer Controls Licensed Contractors Post-Close)"
                },
            )
        } else {
            let credit_part = lender_ipc_credit_cap_usd;
            let price_part = negotiable_physical_mid_usd.saturating_sub(credit_part);
            (
                credit_part,
                price_part,
                "Split Ask: Max Lender-Permitted Seller Closing Credit + Purchase Price Reduction for Excess",
            )
        };

    let effective_preclose = if pref.contains("repair") {
        total_mid
    } else {
        escrow_or_preclose_usd
    };

    let strategy_rationale = format!(
        "Midpoint documented repair & cure exposure across {} trade group(s) is {} (range {} – {}, calibrated at {}). Recommended structure allocates {} as a Seller credit toward Buyer's recurring/non-recurring closing costs (within the ~3% conventional IPC ceiling of {}), {} as a purchase price reduction (preventing unused credit forfeiture and lowering property-tax assessment basis), and {} to direct pre-close escrow payoff of title liens/HOA assessments.",
        grouped_trades.len(),
        format_usd(total_mid),
        format_usd(total_low),
        format_usd(total_high),
        reg_label,
        format_usd(recommended_credit_usd),
        format_usd(lender_ipc_credit_cap_usd),
        format_usd(recommended_price_reduction_usd),
        format_usd(effective_preclose),
    );

    let mut clause_parts = Vec::new();
    if recommended_credit_usd > 0 {
        clause_parts.push(format!(
            "Seller shall credit Buyer {} at close of escrow toward Buyer's closing costs, prepaid items, and/or lender-approved repair escrow",
            format_usd(recommended_credit_usd)
        ));
    }
    if recommended_price_reduction_usd > 0 {
        clause_parts.push(format!(
            "Purchase Price shall be reduced by {}",
            format_usd(recommended_price_reduction_usd)
        ));
    }
    if effective_preclose > 0 {
        clause_parts.push(format!(
            "Seller shall pay off and clear in full through escrow prior to close all Schedule B involuntary liens, solar UCC-1 obligations, and levied HOA special assessments (estimated at {})",
            format_usd(effective_preclose)
        ));
    }
    let addendum_clause_text = format!(
        "In full satisfaction of Buyer's physical inspection and disclosure Investigation Contingency for {display_address}, {}.",
        clause_parts.join("; and ")
    );

    let trade_list_str: String = grouped_trades
        .iter()
        .filter_map(|g| g.get("trade").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join(", ");

    let buyer_agent_email_script = format!(
        "Hi Listing Agent — Thank you for providing the disclosure packet for {display_address}. Our buyers remain enthusiastic about closing smoothly and on time. After reviewing the inspection and disclosure reports with regional contractor pricing ({reg_label}), we documented {} – {} in material health, safety, water-intrusion, and cure items across {trade_list_str} (itemized with exact PDF page citations in the attached schedule). Rather than delaying escrow with multi-trade work orders, our buyers propose resolving all physical inspection items cleanly via {} so we can move forward to contingency removal promptly. Please let us know Seller's response.",
        format_usd(total_low),
        format_usd(total_high),
        if recommended_credit_usd > 0 && recommended_price_reduction_usd > 0 {
            format!(
                "a {} closing credit plus a {} price adjustment{}",
                format_usd(recommended_credit_usd),
                format_usd(recommended_price_reduction_usd),
                if effective_preclose > 0 {
                    format!(" (with Seller clearing {} in title/HOA obligations through escrow)", format_usd(effective_preclose))
                } else {
                    String::new()
                }
            )
        } else if recommended_credit_usd > 0 {
            format!(
                "a {} Seller closing credit{}",
                format_usd(recommended_credit_usd),
                if effective_preclose > 0 {
                    format!(" plus escrow payoff of {} in title/HOA items", format_usd(effective_preclose))
                } else {
                    String::new()
                }
            )
        } else if recommended_price_reduction_usd > 0 {
            format!("a {} purchase price reduction", format_usd(recommended_price_reduction_usd))
        } else {
            format!("pre-close licensed cure/payoff of {} in documented items", format_usd(effective_preclose))
        }
    );

    let markdown_schedule = render_markdown_schedule(
        display_address,
        &reg_label,
        &grouped_trades,
        total_low,
        total_mid,
        total_high,
        recommended_credit_usd,
        recommended_price_reduction_usd,
        effective_preclose,
        &addendum_clause_text,
        &buyer_agent_email_script,
    );

    let printable_html = render_printable_html_schedule(
        display_address,
        &reg_label,
        &grouped_trades,
        total_low,
        total_mid,
        total_high,
        recommended_credit_usd,
        recommended_price_reduction_usd,
        effective_preclose,
        &addendum_clause_text,
        &buyer_agent_email_script,
    );

    Ok(json!({
        "propertyAddress": display_address,
        "regionalMultiplier": reg_mult,
        "regionalLabel": reg_label,
        "selectedDefectCount": line_items.len(),
        "tradeGroups": grouped_trades,
        "totalsUsd": {
            "low": total_low,
            "mid": total_mid,
            "high": total_high,
            "lenderIpcCreditCapApproxUsd": lender_ipc_credit_cap_usd,
        },
        "strategyRecommendation": {
            "primaryStrategy": primary_strategy,
            "sellerCreditAskUsd": recommended_credit_usd,
            "priceReductionAskUsd": recommended_price_reduction_usd,
            "preCloseLicensedRepairOrEscrowCureUsd": effective_preclose,
            "rationale": strategy_rationale,
        },
        "addendumClauseText": addendum_clause_text,
        "buyerAgentEmailScript": buyer_agent_email_script,
        "markdownSchedule": markdown_schedule,
        "printableHtml": printable_html,
        "disclaimer": "Negotiation & repair-schedule worksheet only — NOT legal advice or a state-realtor-association legal form. Consult your licensed real-estate agent, broker, or real-estate attorney when preparing formal Request for Repair (RR / RRRR) contract addenda."
    }))
}

#[inline]
fn lender_ipc_credit_fetching_cap(cap: i64) -> i64 {
    cap.max(5_000)
}

pub const REPAIR_ADDENDUM_INSTRUCTIONS: &str = r#"You are Open Estate Senior Buyer-Representation Negotiation Advisor.
Refine the polite, firm buyer-agent transmittal note (`buyerAgentEmailScript`) and formal-sounding plain-English repair/credit rationale (`strategyRationale`) strictly from ADDENDUM_SUMMARY_JSON.
Do not alter dollar figures, do not invent defects or page citations, never include personal owner/borrower names or SSNs, and never emit markdown code fences or HTML script tags."#;

pub fn repair_addendum_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["strategyRationale", "buyerAgentEmailScript"],
        "properties": {
            "strategyRationale": { "type": "string" },
            "buyerAgentEmailScript": { "type": "string" }
        }
    })
}

/// Full async Repair & Seller Credit Addendum generator (`POST /api/repair-addendum`).
pub async fn generate_repair_addendum(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    input: RepairAddendumInput,
    cache: Option<&dyn LlmCache>,
) -> Result<ModuleOutput> {
    let t0 = Instant::now();
    let mut data = build_repair_addendum_deterministic(report, &input)?;
    let mut warnings = Vec::new();

    init_deterministic_ai_meta(&mut data, None);

    if env.llm_configured() {
        let summary_for_llm = json!({
            "propertyAddress": data.get("propertyAddress"),
            "regionalLabel": data.get("regionalLabel"),
            "totalsUsd": data.get("totalsUsd"),
            "strategyRecommendation": data.get("strategyRecommendation"),
            "tradeGroups": data.get("tradeGroups"),
        });
        let prompt = format!(
            "Polish the buyer-agent negotiation script and strategy rationale for this Request for Repair / Seller Credit schedule.\n\n{}",
            wrap_untrusted_block(
                "untrusted_document_text",
                &serde_json::to_string(&summary_for_llm)?
            )
        );
        let mut opts = LlmCallOptions::new(
            "repair_addendum",
            "repair_addendum",
            REPAIR_ADDENDUM_INSTRUCTIONS,
            prompt,
            repair_addendum_schema(),
        );
        opts.models = resolve_model_specs_with_effort(env, Some("low"));
        opts.max_output_tokens = Some(2_048);
        opts.timeout_ms = Some(60_000);

        match responses_json_with_cache(http, env, opts, cache).await {
            Ok(res) => {
                let raw_out = serde_json::to_string(&res.data).unwrap_or_default();
                if inspect_output_guardrail(&raw_out).is_none() {
                    let polished_email = res
                        .data
                        .get("buyerAgentEmailScript")
                        .and_then(|v| v.as_str())
                        .map(|s| redact_title_pii(s).masked_text);
                    let polished_rat = res
                        .data
                        .get("strategyRationale")
                        .and_then(|v| v.as_str())
                        .map(|s| redact_title_pii(s).masked_text);

                    // Extract values via immutable `data` before mutating `obj`
                    let addr = data
                        .get("propertyAddress")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Subject Property")
                        .to_string();
                    let reg = data
                        .get("regionalLabel")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let grouped = data
                        .get("tradeGroups")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let lo = data
                        .pointer("/totalsUsd/low")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let mid = data
                        .pointer("/totalsUsd/mid")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let hi = data
                        .pointer("/totalsUsd/high")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let c_ask = data
                        .pointer("/strategyRecommendation/sellerCreditAskUsd")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let p_red = data
                        .pointer("/strategyRecommendation/priceReductionAskUsd")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let e_cure = data
                        .pointer("/strategyRecommendation/preCloseLicensedRepairOrEscrowCureUsd")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let clause = data
                        .get("addendumClauseText")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    if let Some(obj) = data.as_object_mut() {
                        if let Some(ref rat) = polished_rat {
                            if let Some(sr) = obj
                                .get_mut("strategyRecommendation")
                                .and_then(|v| v.as_object_mut())
                            {
                                sr.insert("rationale".to_string(), Value::String(rat.clone()));
                            }
                        }
                        if let Some(ref em) = polished_email {
                            obj.insert(
                                "buyerAgentEmailScript".to_string(),
                                Value::String(em.clone()),
                            );
                            // Re-render Markdown & printable HTML so polished negotiation copy is embedded cleanly
                            obj.insert(
                                "markdownSchedule".to_string(),
                                Value::String(render_markdown_schedule(
                                    &addr, &reg, &grouped, lo, mid, hi, c_ask, p_red, e_cure,
                                    &clause, em,
                                )),
                            );
                            obj.insert(
                                "printableHtml".to_string(),
                                Value::String(render_printable_html_schedule(
                                    &addr, &reg, &grouped, lo, mid, hi, c_ask, p_red, e_cure,
                                    &clause, em,
                                )),
                            );
                        }
                    }
                    stamp_ai_enhancement_meta(&mut data, &res, true);
                }
            }
            Err(e) => {
                warnings.push(format!(
                    "AI negotiation polish skipped (deterministic schedule complete): {}",
                    scrub_sensitive_text(&e.to_string())
                ));
            }
        }
    }

    if let Some(obj) = data.as_object_mut() {
        obj.insert("ms".to_string(), json!(t0.elapsed().as_millis() as u64));
    }

    let addendum_src_url = {
        let dom = env.brand_domain();
        if dom == "open-estate.local" {
            "https://openestate.org/repair-addendum".to_string()
        } else {
            format!("https://{dom}/repair-addendum")
        }
    };
    Ok(ModuleOutput {
        data,
        sources: vec![SourceItem::new(
            format!("{} Repair & Seller Credit Addendum Generator", env.brand_name()),
            addendum_src_url,
        )],
        warnings,
    })
}
