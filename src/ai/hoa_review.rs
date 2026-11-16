// HOA / Condo / Strata Financial, Governance & Warrantability Analyzer (`POST /api/hoa-review`).
// Competes directly with Eli Report and Rexera / InspectHOA:
// 1. Ingests Reserve Study, Annual HOA Budget / Balance Sheet, CC&Rs / Bylaws / Rules,
//    Resale Certificate / Condo Questionnaire, and Board Meeting Minutes via
//    `extract_and_ocr_uploads_async` + pre-LLM `redact_title_pii` PII scrubbing.
// 2. Deterministic regex + financial extraction (offline/keyless-ready!) AND OpenAI Responses API
//    structured synthesis (`hoa_review_schema`) covering:
//    - Financial & Reserve Health: Monthly dues, Operating Budget, Reserve Allocation %,
//      Current Reserve Balance, Ideal/Fully-Funded Balance, Percent Funded (`%`) with actuarial
//      risk tier (`<30% Critical Underfunded`, `30–69% Fair / Watch`, `>=70% Strong`), per-unit
//      reserve deficit, >60-day delinquency rate (`%`), and active/proposed Special Assessments.
//    - Structural & Statutory Mandates: CA SB 326 / SB 721 Exterior Elevated Elements (balcony/
//      deck/walkway waterproofing & dry-rot), FL Milestone Inspection & Structural Integrity
//      Reserve Study (SIRS), roof/siding/elevator/plumbing repipe lifecycle shortfalls, and
//      active HOA / construction-defect litigation.
//    - Lifestyle, Investor & Governance Rules: Rental cap % & waitlist, Short-Term Rental
//      (Airbnb/VRBO) ban or minimum lease days (30-day / 180-day / 1-year min), pet weight/breed/
//      count limits, EV charger / solar / remodel / hard-surface flooring acoustic rules, smoking
//      ban, and Master Insurance coverage (`Walls-In` vs `Bare Walls` + 5%–15%+ deductible shock).
//    - Fannie Mae / Freddie Mac / FHA Condo Warrantability Checklist: Flags non-warrantable deal-
//      breakers (>15% delinquency, >10%–20% single-entity ownership, >35% commercial space, <10%
//      budget allocated to reserves, structural/safety litigation, critical unsafe SB 326/Milestone
//      defects, hotel/condotel traits, >5% master insurance deductible).
//    - Exact `{ docName, page, quote }` page citations on every extracted finding.

use crate::ai::doc_pipeline::{
    append_scrubbed_lines_warning, build_report_brief_or_default, clip_sanitized_text,
    init_deterministic_ai_meta, redact_pii_string_array, stamp_ai_enhancement_meta,
};
use crate::ai::llm::{
    resolve_model_specs_with_effort, responses_json_with_cache, LlmCache, LlmCallOptions,
};
use crate::ai::pdf::{extract_and_ocr_uploads_async, ExtractedDocument, UploadedDocInput};
use crate::ai::title_review::redact_title_pii;
use crate::guardrails::{
    inspect_output_guardrail, scrub_sensitive_text, wrap_untrusted_block, DocumentKind,
};
use crate::shared::{EnvConfig, HttpClient, ModuleOutput, SourceItem};
use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Instant;

/// Exact PDF page citation enabling UI viewers to jump straight to the cited page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DocPageCitation {
    pub doc_name: String,
    pub page: u32,
    pub quote: String,
}

#[derive(Debug, Clone)]
struct CitedSpan {
    doc_name: String,
    page: u32,
    line_idx: usize,
    text: String,
}

fn clean_quote(raw: &str, max_chars: usize) -> String {
    let collapsed = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let masked = redact_title_pii(&collapsed).masked_text;
    if masked.chars().count() <= max_chars {
        masked
    } else {
        let head: String = masked.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", head.trim_end())
    }
}

/// Builds a searchable index of single lines and 2-line sliding windows per PDF page so regexes
/// match both single-line table rows and wrapped two-line sentences while preserving exact page numbers.
fn build_cited_spans(docs: &[ExtractedDocument]) -> (Vec<CitedSpan>, Vec<CitedSpan>) {
    let mut single_lines = Vec::new();
    let mut two_line_windows = Vec::new();

    for doc in docs {
        for page in &doc.pages {
            let non_empty: Vec<&str> = page
                .text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !(l.starts_with("[Page ") && l.ends_with(']')))
                .collect();
            for (line_idx, &line) in non_empty.iter().enumerate() {
                single_lines.push(CitedSpan {
                    doc_name: doc.name.clone(),
                    page: page.page_number,
                    line_idx,
                    text: line.to_string(),
                });
            }
            for (line_idx, win) in non_empty.windows(2).enumerate() {
                two_line_windows.push(CitedSpan {
                    doc_name: doc.name.clone(),
                    page: page.page_number,
                    line_idx,
                    text: format!("{} {}", win[0], win[1]),
                });
            }
        }
    }
    (single_lines, two_line_windows)
}

fn find_first_capture(
    re: &Regex,
    singles: &[CitedSpan],
    windows: &[CitedSpan],
) -> Option<(Vec<String>, DocPageCitation)> {
    for span in singles.iter().chain(windows.iter()) {
        if let Some(caps) = re.captures(&span.text) {
            let mut groups = Vec::with_capacity(caps.len());
            for i in 0..caps.len() {
                groups.push(
                    caps.get(i)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default(),
                );
            }
            return Some((
                groups,
                DocPageCitation {
                    doc_name: span.doc_name.clone(),
                    page: span.page,
                    quote: clean_quote(&span.text, 220),
                },
            ));
        }
    }
    None
}

fn find_first_capture_with_detail<F>(
    re: &Regex,
    singles: &[CitedSpan],
    windows: &[CitedSpan],
    is_rich: F,
) -> Option<(Vec<String>, DocPageCitation)>
where
    F: Fn(&str) -> bool,
{
    let mut first_single: Option<(usize, (Vec<String>, DocPageCitation))> = None;
    for span in singles {
        if let Some(caps) = re.captures(&span.text) {
            let mut groups = Vec::with_capacity(caps.len());
            for i in 0..caps.len() {
                groups.push(
                    caps.get(i)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default(),
                );
            }
            let cit = DocPageCitation {
                doc_name: span.doc_name.clone(),
                page: span.page,
                quote: clean_quote(&span.text, 220),
            };
            if is_rich(&span.text) {
                return Some((groups, cit));
            }
            // Check corresponding 2-line window starting at this same line before falling back
            if let Some(win) = windows.iter().find(|w| {
                w.doc_name == span.doc_name && w.page == span.page && w.line_idx == span.line_idx
            }) {
                if is_rich(&win.text) {
                    if let Some(wcaps) = re.captures(&win.text) {
                        let mut wgroups = Vec::with_capacity(wcaps.len());
                        for i in 0..wcaps.len() {
                            wgroups.push(
                                wcaps
                                    .get(i)
                                    .map(|m| m.as_str().trim().to_string())
                                    .unwrap_or_default(),
                            );
                        }
                        return Some((
                            wgroups,
                            DocPageCitation {
                                doc_name: win.doc_name.clone(),
                                page: win.page,
                                quote: clean_quote(&win.text, 220),
                            },
                        ));
                    }
                }
            }
            if first_single.is_none() {
                first_single = Some((span.line_idx, (groups, cit)));
            }
        }
    }
    if let Some((_, pair)) = first_single {
        return Some(pair);
    }
    for span in windows {
        if let Some(caps) = re.captures(&span.text) {
            let mut groups = Vec::with_capacity(caps.len());
            for i in 0..caps.len() {
                groups.push(
                    caps.get(i)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default(),
                );
            }
            return Some((
                groups,
                DocPageCitation {
                    doc_name: span.doc_name.clone(),
                    page: span.page,
                    quote: clean_quote(&span.text, 220),
                },
            ));
        }
    }
    None
}

fn find_all_captures(
    re: &Regex,
    singles: &[CitedSpan],
    windows: &[CitedSpan],
) -> Vec<(Vec<String>, DocPageCitation)> {
    let mut out: Vec<(Vec<String>, DocPageCitation)> = Vec::new();
    let mut single_line_hits: std::collections::HashMap<(String, u32, usize), usize> =
        std::collections::HashMap::new();
    let mut seen_page_quotes = std::collections::HashSet::new();

    for span in singles {
        if let Some(caps) = re.captures(&span.text) {
            let dedup_sig = format!("{}:{}:{}", span.doc_name, span.page, &caps[0].to_lowercase());
            if !seen_page_quotes.insert(dedup_sig) {
                continue;
            }
            let mut groups = Vec::with_capacity(caps.len());
            for i in 0..caps.len() {
                groups.push(
                    caps.get(i)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default(),
                );
            }
            let idx = out.len();
            single_line_hits.insert((span.doc_name.clone(), span.page, span.line_idx), idx);
            out.push((
                groups,
                DocPageCitation {
                    doc_name: span.doc_name.clone(),
                    page: span.page,
                    quote: clean_quote(&span.text, 220),
                },
            ));
        }
    }

    for span in windows {
        let hit_first = single_line_hits.get(&(span.doc_name.clone(), span.page, span.line_idx));
        let hit_second = single_line_hits.contains_key(&(span.doc_name.clone(), span.page, span.line_idx + 1));
        if hit_second {
            continue;
        }
        if let Some(&existing_idx) = hit_first {
            // Only upgrade the single-line match to the 2-line window if the single line lacked `$`
            // and the combined window provides `$` or explicit vote/status context without duplicating.
            let prev_quote = &out[existing_idx].1.quote;
            let adds_money = !prev_quote.contains('$') && span.text.contains('$');
            if !adds_money {
                continue;
            }
            if let Some(caps) = re.captures(&span.text) {
                let mut groups = Vec::with_capacity(caps.len());
                for i in 0..caps.len() {
                    groups.push(
                        caps.get(i)
                            .map(|m| m.as_str().trim().to_string())
                            .unwrap_or_default(),
                    );
                }
                out[existing_idx] = (
                    groups,
                    DocPageCitation {
                        doc_name: span.doc_name.clone(),
                        page: span.page,
                        quote: clean_quote(&span.text, 220),
                    },
                );
            }
            continue;
        }
        if let Some(caps) = re.captures(&span.text) {
            let dedup_sig = format!("{}:{}:{}", span.doc_name, span.page, &caps[0].to_lowercase());
            if !seen_page_quotes.insert(dedup_sig) {
                continue;
            }
            let mut groups = Vec::with_capacity(caps.len());
            for i in 0..caps.len() {
                groups.push(
                    caps.get(i)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default(),
                );
            }
            out.push((
                groups,
                DocPageCitation {
                    doc_name: span.doc_name.clone(),
                    page: span.page,
                    quote: clean_quote(&span.text, 220),
                },
            ));
        }
    }
    out
}

fn parse_money_usd(raw: &str) -> Option<f64> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut val = cleaned.parse::<f64>().ok()?;
    let low = raw.to_ascii_lowercase();
    if low.contains("million") || low.ends_with('m') {
        val *= 1_000_000.0;
    } else if low.ends_with('k') {
        val *= 1_000.0;
    }
    if val.is_finite() && val >= 0.0 {
        Some(val)
    } else {
        None
    }
}

fn parse_pct(raw: &str) -> Option<f64> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let val = cleaned.parse::<f64>().ok()?;
    if val.is_finite() && (0.0..=100.0).contains(&val) {
        Some((val * 10.0).round() / 10.0)
    } else {
        None
    }
}

pub fn classify_reserve_risk_tier(percent_funded: Option<f64>) -> &'static str {
    match percent_funded {
        Some(p) if p < 30.0 => "<30% Critical Underfunded",
        Some(p) if p < 70.0 => "30–69% Fair / Watch",
        Some(_) => ">=70% Strong",
        None => "Unknown — Verify Reserve Study",
    }
}

fn classify_hoa_subdoc(doc: &ExtractedDocument) -> &'static str {
    let name_l = doc.name.to_ascii_lowercase();
    let text_l: String = doc.formatted_text.chars().take(4_000).collect::<String>().to_ascii_lowercase();

    if name_l.contains("reserve")
        || text_l.contains("percent funded")
        || text_l.contains("fully funded balance")
        || text_l.contains("structural integrity reserve study")
    {
        "reserve_study"
    } else if name_l.contains("minute")
        || text_l.contains("board of directors meeting")
        || text_l.contains("call to order")
        || text_l.contains("motion carried")
    {
        "board_minutes"
    } else if name_l.contains("questionnaire")
        || name_l.contains("resale")
        || name_l.contains("estoppel")
        || text_l.contains("condominium project questionnaire")
        || text_l.contains("resale certificate")
    {
        "resale_certificate_or_questionnaire"
    } else if name_l.contains("cc&r")
        || name_l.contains("ccr")
        || name_l.contains("bylaw")
        || name_l.contains("rules")
        || text_l.contains("covenants, conditions and restrictions")
        || text_l.contains("architectural control")
    {
        "ccrs_bylaws_rules"
    } else if name_l.contains("budget")
        || name_l.contains("financial")
        || name_l.contains("balance")
        || text_l.contains("annual operating budget")
        || text_l.contains("balance sheet")
    {
        "annual_budget_financials"
    } else {
        "hoa_document"
    }
}

// ------------------------- Regex Library (Compiled Once) -------------------------

static MONTHLY_DUES_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Monthly\s+(?:HOA\s+)?(?:Dues|Assessment|Fee|Common\s+Charge)s?|Regular\s+Monthly\s+Assessment|HOA\s+(?:Monthly\s+)?(?:Dues|Fee))\s*[:=\-]?\s*\$([0-9,]+(?:\.\d{1,2})?)",
    )
    .unwrap()
});
static OPERATING_BUDGET_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Annual\s+Operating\s+Budget|Total\s+Operating\s+(?:Budget|Revenue|Expenses|Income)|Operating\s+Budget)\s*[:=\-]?\s*\$([0-9,]+(?:\.\d{1,2})?(?:\s*(?:million|M|k))?)",
    )
    .unwrap()
});
static RESERVE_CONTRIB_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Annual\s+Reserve\s+(?:Contribution|Allocation|Funding|Transfer)|Reserve\s+(?:Contribution|Allocation|Transfer)\s*(?:\(Annual\))?)\s*[:=\-]?\s*\$([0-9,]+(?:\.\d{1,2})?(?:\s*(?:million|M|k))?)",
    )
    .unwrap()
});
static RESERVE_ALLOC_PCT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Reserve\s+(?:Allocation|Contribution|Funding)(?:\s+Rate)?|allocated\s+to\s+reserves|budget\s+(?:allocated\s+)?to\s+reserves)[^\n\d%]{0,30}([0-9]{1,2}(?:\.\d{1,2})?)\s*%",
    )
    .unwrap()
});
static CURRENT_RESERVE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Current\s+Reserve\s+(?:Fund\s+)?Balance|Actual\s+Reserve\s+(?:Fund\s+)?Balance|Projected\s+Starting\s+Reserve\s+Balance|Reserve\s+Fund\s+Balance|Ending\s+Reserve\s+Balance)\s*[:=\-]?\s*\$([0-9,]+(?:\.\d{1,2})?(?:\s*(?:million|M|k))?)",
    )
    .unwrap()
});
static IDEAL_RESERVE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Fully[- ]Funded\s+(?:Reserve\s+)?Balance|Ideal\s+(?:Reserve\s+)?Balance|100%\s+Funded\s+Balance)\s*[:=\-]?\s*\$([0-9,]+(?:\.\d{1,2})?(?:\s*(?:million|M|k))?)",
    )
    .unwrap()
});
static PERCENT_FUNDED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Percent(?:age)?\s+Funded\s*[:=\-]?\s*([0-9]{1,3}(?:\.\d{1,2})?)\s*%|([0-9]{1,3}(?:\.\d{1,2})?)\s*%\s+Funded\b)",
    )
    .unwrap()
});
static UNIT_COUNT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Total\s+(?:Number\s+of\s+)?(?:Residential\s+)?Units|Number\s+of\s+Units|Project\s+Size|([0-9]{1,4})[- ]unit\s+(?:condominium|condo|townhome|hoa|community|association))\s*[:=\-]?\s*([0-9]{1,4})?",
    )
    .unwrap()
});
static PER_UNIT_DEFICIT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:Per[- ]Unit\s+(?:Reserve\s+)?(?:Deficit|Shortfall)|Deficit\s+Per\s+Unit)\s*[:=\-]?\s*\$([0-9,]+(?:\.\d{1,2})?)",
    )
    .unwrap()
});
static DELINQUENCY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:(?:Delinquen(?:cy|t)(?:\s+Rate)?|60\+?\s*days?\s+(?:past\s+due|late|in\s+arrears)|(?:assessments?|units?|owners?)\s*(?:>|\bover\b|\bmore\s+than\b)?\s*60\s*\+?\s*days?\s+(?:past\s+due|late|delinquent))[^\n%]{0,65}?([0-9]{1,2}(?:\.\d{1,2})?)\s*%|([0-9]{1,2}(?:\.\d{1,2})?)\s*%\s+of\s+(?:units|owners|assessments)[^\n]{0,50}(?:delinquent|past\s+due|late|60\s*\+?\s*days))",
    )
    .unwrap()
});
static SPECIAL_ASSESSMENT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:Special\s+Assessment|Emergency\s+Assessment)\b[^\n]{0,160}",
    )
    .unwrap()
});
static NO_SPECIAL_ASSESSMENT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:no\s+(?:active|current|pending|proposed|known)\s+special\s+assessment|special\s+assessments?\s*:\s*none)\b",
    )
    .unwrap()
});
static MONEY_ANY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\$([0-9,]+(?:\.\d{1,2})?(?:\s*(?:million|M|k))?)").unwrap()
});
static PER_UNIT_MONEY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\$([0-9,]+(?:\.\d{1,2})?)\s*(?:per\s+unit|/\s*unit|each\s+unit)").unwrap()
});

static SB326_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:SB\s*[-]?\s*326|SB\s*[-]?\s*721|Exterior\s+Elevated\s+Elements?|Civil\s+Code\s+§?\s*5551)\b[^\n]{0,180}",
    )
    .unwrap()
});
static BALCONY_ROT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:balcon(?:y|ies)|elevated\s+walkway|elevated\s+deck)\b[^\n]{0,140}\b(?:dry\s+rot|fungus|waterproof\w*\s+fail\w*|wood\s+decay|structural\s+repair|unsafe|shoring|membrane\s+fail\w*)\b",
    )
    .unwrap()
});
static FL_MILESTONE_SIRS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:Milestone\s+Inspection|Structural\s+Integrity\s+Reserve\s+Study|\bSIRS\b|F\.?S\.?\s*§?\s*553\.899)\b[^\n]{0,180}",
    )
    .unwrap()
});
static LIFECYCLE_SHORTFALL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(roof(?:\s+system|\s+membrane|\s+replacement)?|siding|stucco|elevator(?:\s+modernization)?|plumbing\s+repipe|galvanized\s+pipe|polybutylene|cast[- ]iron\s+stack|boiler|waterproofing|parking\s+garage\s+spalling)\b[^\n]{0,150}\b(remaining\s+(?:useful\s+)?life\s*(?:of\s*)?[0-3]\s*(?:yr|year)|past\s+useful\s+life|end\s+of\s+(?:useful\s+)?life|deferred\s+maintenance|underfunded|shortfall|immediate\s+replacement|urgent\s+repipe)\b",
    )
    .unwrap()
});
static LITIGATION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:active\s+litigation|pending\s+(?:litigation|lawsuit)|construction\s+defect\s+(?:claim|lawsuit|litigation|action)|sued\s+by|plaintiff|defendant\s+association|structural\s+litigation)\b[^\n]{0,180}",
    )
    .unwrap()
});
static NO_LITIGATION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:no\s+(?:active|pending|known)\s+(?:litigation|lawsuits?)|litigation\s*:\s*(?:none|no))\b",
    )
    .unwrap()
});

static RENTAL_CAP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:rental\s+cap[^\n%]{0,45}?([0-9]{1,2}(?:\.\d)?)\s*%|no\s+more\s+than\s+([0-9]{1,2}(?:\.\d)?)\s*%\s+of\s+(?:the\s+)?units\s+may\s+be\s+(?:rented|leased)|maximum\s+(?:of\s+)?([0-9]{1,2}(?:\.\d)?)\s*%\s+(?:leased|rented|investor))",
    )
    .unwrap()
});
static RENTAL_WAITLIST_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:rental\s+wait(?:ing)?\s*list[^\n]{0,60}|([0-9]{1,3})\s+(?:units?|owners?)\s+on\s+(?:the\s+)?(?:rental\s+)?wait(?:ing)?\s*list)",
    )
    .unwrap()
});
static STR_MIN_LEASE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:minimum\s+(?:([0-9]{1,3})[- ](day|month|year)\s+)?(?:lease|rental)(?:\s+(?:term|period))?(?:\s+is|\s+of|\s*:)?\s*([0-9]{1,3})?\s*[- ]?(day|month|year)?s?|no\s+(?:lease|rental)s?\s+(?:for\s+)?(?:less|fewer)\s+than\s+([0-9]{1,3})\s*[- ]?(day|month|year)s?|([0-9]{1,3})[- ](day|month|year)\s+minimum\s+(?:lease|rental)|(?:short[- ]term\s+rentals?|airbnb|vrbo|transient\s+rentals?)[^\n]{0,90}(?:prohibited|banned|not\s+permitted|strictly\s+prohibited|(?:minimum|under|less\s+than)\s+([0-9]{1,3})\s*[- ]?(day|month|year)s?))",
    )
    .unwrap()
});
static PET_RULE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:pet\s+(?:restriction|policy|limit|rule)s?|no\s+more\s+than\s+(?:one|two|three|[1-4])\s+(?:domestic\s+)?pets?|(?:dog|pet)s?\s+(?:up\s+to|under|not\s+exceeding|max(?:imum)?(?:\s+weight)?)\s*[0-9]{1,3}\s*(?:lbs?|pounds)|aggressive\s+breeds?\s+prohibited|no\s+pets\s+allowed)[^\n]{0,140}",
    )
    .unwrap()
});
static PET_WEIGHT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)([0-9]{1,3})\s*(?:[- ]?(?:lb|lbs|pound)s?)").unwrap()
});
static PET_COUNT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(one|two|three|1|2|3|4)\s+(?:domestic\s+|household\s+)?(?:pets?|dogs?|cats?)\b").unwrap()
});
static EV_RULE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:EV\s+charg\w*|electric\s+vehicle\s+charging)[^\n]{0,150}").unwrap()
});
static SOLAR_RULE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:solar\s+(?:panel|energy|system)s?)[^\n]{0,140}").unwrap()
});
static FLOORING_RULE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:hard[- ]surface\s+floor\w*|hardwood\s+floor\w*|LVT|laminate\s+floor\w*|IIC\s*[0-9]{2}|STC\s*[0-9]{2}|acoustic\s+underlayment|80%\s+carpet)[^\n]{0,150}",
    )
    .unwrap()
});
static REMODEL_RULE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:architectural\s+(?:control|committee|review|approval)|interior\s+remodel\w*|structural\s+modification)[^\n]{0,140}",
    )
    .unwrap()
});
static SMOKING_RULE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:non[- ]smoking\s+(?:community|building|complex)|smoking\s+(?:is\s+)?(?:prohibited|banned)|smoke[- ]free\s+(?:policy|community|balconies))[^\n]{0,130}",
    )
    .unwrap()
});
static MASTER_INS_COVERAGE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(walls[- ]in|all[- ]in|all[- ]inclusive|bare[- ]walls|studs[- ]out|studs[- ]in|single[- ]entity)\b[^\n]{0,140}",
    )
    .unwrap()
});
static MASTER_DEDUCTIBLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:(?:master\s+(?:policy|insurance|hazard)?[^\n]{0,60}?deductible|(?:wind|hurricane|wildfire|water\s+damage|property|hazard|earthquake|per[- ]occurrence)\s+deductible)[^\n]{0,70}?(?:([0-9]{1,2}(?:\.\d)?)\s*%|\$([0-9,]+))|(?:([0-9]{1,2}(?:\.\d)?)\s*%|\$([0-9,]+))[^\n.;]{0,35}?(?:wind|hurricane|wildfire|water\s+damage|property|hazard|earthquake|master|per[- ]occurrence)?[^\n.;]{0,20}?deductible)",
    )
    .unwrap()
});

// Warrantability-specific detectors
static SINGLE_ENTITY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:single[- ]entity(?:\s+ownership)?|one\s+(?:investor|owner|entity)\s+owns)[^\n%]{0,60}?([0-9]{1,2}(?:\.\d)?)\s*%",
    )
    .unwrap()
});
static COMMERCIAL_SPACE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:commercial\s+(?:space|area|square\s+footage|use)|non[- ]residential\s+space)[^\n%]{0,60}?([0-9]{1,2}(?:\.\d)?)\s*%",
    )
    .unwrap()
});
static CONDOTEL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:condotel|condo[- ]hotel|front\s+desk\s+check[- ]in|mandatory\s+rental\s+pool|daily\s+maid\s+service|hotel[- ]style\s+transient)\b[^\n]{0,140}",
    )
    .unwrap()
});

/// Deterministic parser over PII-scrubbed `ExtractedDocument` pages. Powers keyless/offline mode
/// and provides the factual anchor for LLM synthesis.
pub fn parse_hoa_documents(
    docs: &[ExtractedDocument],
    report: Option<&Value>,
    pii_redacted_count: usize,
) -> Value {
    let (singles, windows) = build_cited_spans(docs);
    let mut findings: Vec<Value> = Vec::new();

    let doc_inventory: Vec<Value> = docs
        .iter()
        .map(|d| {
            json!({
                "docName": d.name,
                "docCategory": classify_hoa_subdoc(d),
                "pages": d.pages.len(),
                "chars": d.chars,
                "method": d.method,
            })
        })
        .collect();

    // ---------------- 1. Financial & Reserve Health ----------------
    let monthly_dues = find_first_capture(&MONTHLY_DUES_RE, &singles, &windows)
        .and_then(|(g, c)| parse_money_usd(&g[1]).map(|v| (v, Some(c))))
        .or_else(|| {
            report
                .and_then(|r| r.pointer("/facts/hoaMonthly"))
                .and_then(|v| v.as_f64())
                .filter(|&v| v > 0.0)
                .map(|v| (v, None))
        });

    let operating_budget = find_first_capture(&OPERATING_BUDGET_RE, &singles, &windows)
        .and_then(|(g, c)| parse_money_usd(&g[1]).map(|v| (v, c)));

    let reserve_contrib = find_first_capture(&RESERVE_CONTRIB_RE, &singles, &windows)
        .and_then(|(g, c)| parse_money_usd(&g[1]).map(|v| (v, c)));

    let explicit_reserve_alloc_pct = find_first_capture(&RESERVE_ALLOC_PCT_RE, &singles, &windows)
        .and_then(|(g, c)| parse_pct(&g[1]).map(|v| (v, c)));

    let reserve_alloc_pct: Option<(f64, Option<DocPageCitation>)> =
        if let Some((pct, cit)) = explicit_reserve_alloc_pct {
            Some((pct, Some(cit)))
        } else if let (Some((rc, rc_cit)), Some((ob, _))) = (&reserve_contrib, &operating_budget) {
            if *ob > 0.0 {
                let pct = ((*rc / *ob) * 1000.0).round() / 10.0;
                Some((pct, Some(rc_cit.clone())))
            } else {
                None
            }
        } else {
            None
        };

    let current_reserve = find_first_capture(&CURRENT_RESERVE_RE, &singles, &windows)
        .and_then(|(g, c)| parse_money_usd(&g[1]).map(|v| (v, c)));

    let ideal_reserve = find_first_capture(&IDEAL_RESERVE_RE, &singles, &windows)
        .and_then(|(g, c)| parse_money_usd(&g[1]).map(|v| (v, c)));

    let explicit_pct_funded = find_first_capture(&PERCENT_FUNDED_RE, &singles, &windows).and_then(
        |(g, c)| {
            let s = if !g[1].is_empty() { &g[1] } else { &g[2] };
            parse_pct(s).map(|v| (v, c))
        },
    );

    let percent_funded_pair: Option<(f64, DocPageCitation)> =
        if let Some(pair) = explicit_pct_funded {
            Some(pair)
        } else if let (Some((cur, cur_cit)), Some((ideal, _))) = (&current_reserve, &ideal_reserve) {
            if *ideal > 0.0 {
                let computed = (((*cur / *ideal) * 1000.0).round() / 10.0).clamp(0.0, 200.0);
                Some((computed, cur_cit.clone()))
            } else {
                None
            }
        } else {
            None
        };

    let percent_funded = percent_funded_pair.as_ref().map(|(p, _)| *p);
    let reserve_risk_tier = classify_reserve_risk_tier(percent_funded);

    if let Some((pf, ref pf_cit)) = percent_funded_pair {
        let sev = if pf < 30.0 {
            "critical"
        } else if pf < 70.0 {
            "moderate"
        } else {
            "informational"
        };
        findings.push(json!({
            "category": "financial_reserves",
            "severity": sev,
            "title": format!("Reserve Fund {:.1}% Funded ({reserve_risk_tier})", pf),
            "detail": if pf < 30.0 {
                "Reserve study falls below the 30% actuarial danger threshold — elevated risk of special assessments or sharp monthly dues increases."
            } else if pf < 70.0 {
                "Reserve study is in the 30%–69% fair/watch band — monitor upcoming roof, exterior, or mechanical capital expenditures."
            } else {
                "Reserve fund is >=70% funded, indicating strong capital planning."
            },
            "citation": pf_cit,
        }));
    }

    let unit_count: Option<u32> = find_first_capture(&UNIT_COUNT_RE, &singles, &windows)
        .and_then(|(g, _)| {
            let s = if !g.get(1).map(|x| x.as_str()).unwrap_or("").is_empty() {
                &g[1]
            } else {
                g.get(2).map(|x| x.as_str()).unwrap_or("")
            };
            s.parse::<u32>().ok().filter(|&n| n > 0)
        });

    let total_reserve_deficit_usd: Option<i64> = match (&current_reserve, &ideal_reserve) {
        (Some((cur, _)), Some((ideal, _))) if *ideal > *cur => Some((*ideal - *cur).round() as i64),
        _ => None,
    };

    let explicit_per_unit_deficit = find_first_capture(&PER_UNIT_DEFICIT_RE, &singles, &windows)
        .and_then(|(g, _)| parse_money_usd(&g[1]).map(|v| v.round() as i64));

    let per_unit_reserve_deficit_usd: Option<i64> = explicit_per_unit_deficit.or_else(|| {
        match (total_reserve_deficit_usd, unit_count) {
            (Some(def), Some(u)) if u > 0 => Some((def as f64 / u as f64).round() as i64),
            _ => None,
        }
    });

    let delinquency_pair = find_first_capture(&DELINQUENCY_RE, &singles, &windows).and_then(
        |(g, c)| {
            let s = if !g[1].is_empty() { &g[1] } else { &g[2] };
            parse_pct(s).map(|v| (v, c))
        },
    );

    if let Some((delinq_pct, ref d_cit)) = delinquency_pair {
        let sev = if delinq_pct > 15.0 {
            "critical"
        } else if delinq_pct >= 8.0 {
            "major"
        } else {
            "informational"
        };
        findings.push(json!({
            "category": "financial_reserves",
            "severity": sev,
            "title": format!("HOA Assessment Delinquency Rate (>60 Days): {:.1}%", delinq_pct),
            "detail": if delinq_pct > 15.0 {
                "Exceeds the 15% Fannie Mae / Freddie Mac / FHA warrantability ceiling — conventional conforming loans will likely be denied."
            } else {
                "Assessment delinquency rate reported in HOA questionnaire/financials."
            },
            "citation": d_cit,
        }));
    }

    // Active or Proposed Special Assessments
    let mut special_assessments: Vec<Value> = Vec::new();
    for (g, cit) in find_all_captures(&SPECIAL_ASSESSMENT_RE, &singles, &windows) {
        let line = &g[0];
        if NO_SPECIAL_ASSESSMENT_RE.is_match(line) {
            continue;
        }
        let low = line.to_ascii_lowercase();
        let per_unit_amt = PER_UNIT_MONEY_RE
            .captures(line)
            .and_then(|c| c.get(1))
            .and_then(|m| parse_money_usd(m.as_str()))
            .map(|v| v.round() as i64);
        let first_amt = MONEY_ANY_RE
            .captures(line)
            .and_then(|c| c.get(1))
            .and_then(|m| parse_money_usd(m.as_str()))
            .map(|v| v.round() as i64);

        let vote_status = if low.contains("approved") || low.contains("adopted") || low.contains("levied") || low.contains("passed") {
            "Approved / Active"
        } else if low.contains("proposed") || low.contains("pending vote") || low.contains("ballot") || low.contains("considering") || low.contains("under discussion") {
            "Proposed / Pending Vote"
        } else {
            "Referenced — Verify Board Vote & Payoff Responsibility"
        };

        let purpose = if low.contains("balcon") || low.contains("sb 326") || low.contains("deck") {
            "Balcony / Exterior Elevated Elements Remediation"
        } else if low.contains("roof") {
            "Roof Replacement"
        } else if low.contains("plumb") || low.contains("repipe") || low.contains("pipe") {
            "Plumbing Repipe"
        } else if low.contains("siding") || low.contains("stucco") || low.contains("envelope") {
            "Building Envelope / Siding Remediation"
        } else if low.contains("insurance") {
            "Master Insurance Premium Shortfall"
        } else if low.contains("elevator") {
            "Elevator Modernization"
        } else {
            "Capital Repair / Reserve Shortfall"
        };

        special_assessments.push(json!({
            "amountUsd": first_amt,
            "perUnitAmountUsd": per_unit_amt,
            "purpose": purpose,
            "boardVoteStatus": vote_status,
            "citation": cit.clone(),
        }));

        findings.push(json!({
            "category": "special_assessment",
            "severity": if vote_status.starts_with("Approved") { "critical" } else { "major" },
            "title": format!("Special Assessment ({vote_status}): {purpose}"),
            "detail": cit.quote.clone(),
            "citation": cit,
        }));
    }

    // ---------------- 2. Structural & Statutory Mandates ----------------
    let sb326_hit = find_first_capture_with_detail(&SB326_RE, &singles, &windows, |txt| {
        let l = txt.to_ascii_lowercase();
        l.contains("dry rot")
            || l.contains("fungus")
            || l.contains("decay")
            || l.contains("unsafe")
            || l.contains("shoring")
            || l.contains("waterproof")
            || l.contains("repair")
            || l.contains("pending")
            || l.contains("overdue")
    })
    .or_else(|| find_first_capture(&BALCONY_ROT_RE, &singles, &windows));
    let ca_sb326_sb721 = if let Some((g, cit)) = sb326_hit {
        let low = format!("{} {}", g[0], cit.quote).to_ascii_lowercase();
        let has_defect = low.contains("dry rot")
            || low.contains("fungus")
            || low.contains("decay")
            || low.contains("unsafe")
            || low.contains("shoring")
            || low.contains("waterproof") && (low.contains("fail") || low.contains("compromised") || low.contains("remediat") || low.contains("repair") || low.contains("replace"))
            || low.contains("immediate repair");
        let status = if has_defect {
            "critical_repair_needed"
        } else if low.contains("pending") || low.contains("not yet completed") || low.contains("scheduled") || low.contains("overdue") {
            "pending_or_overdue"
        } else {
            "inspected_compliant"
        };
        let sev = match status {
            "critical_repair_needed" => "critical",
            "pending_or_overdue" => "major",
            _ => "informational",
        };
        findings.push(json!({
            "category": "structural_statutory",
            "severity": sev,
            "title": format!("CA SB 326 / SB 721 Balcony & Elevated Elements ({status})"),
            "detail": cit.quote.clone(),
            "citation": cit.clone(),
        }));
        json!({
            "detected": true,
            "status": status,
            "dryRotOrWaterproofingDefects": has_defect,
            "summary": cit.quote.clone(),
            "citation": cit,
        })
    } else {
        json!({
            "detected": false,
            "status": "not_referenced",
            "dryRotOrWaterproofingDefects": false,
            "summary": "No explicit CA SB 326 / SB 721 exterior elevated element inspection report found in uploaded packet.",
            "citation": Value::Null,
        })
    };

    let fl_hit = find_first_capture_with_detail(&FL_MILESTONE_SIRS_RE, &singles, &windows, |txt| {
        let l = txt.to_ascii_lowercase();
        l.contains("phase 2")
            || l.contains("deterioration")
            || l.contains("spalling")
            || l.contains("rebar")
            || l.contains("underfunded")
            || l.contains("pending")
            || l.contains("required")
    });
    let fl_milestone_sirs = if let Some((g, cit)) = fl_hit {
        let low = format!("{} {}", g[0], cit.quote).to_ascii_lowercase();
        let phase2 = low.contains("phase 2")
            || low.contains("substantial structural deterioration")
            || low.contains("spalling")
            || low.contains("rebar")
            || low.contains("underfunded sirs");
        let status = if phase2 {
            "phase2_structural_distress"
        } else if low.contains("pending") || low.contains("required") || low.contains("due") {
            "required_or_pending"
        } else {
            "compliant"
        };
        findings.push(json!({
            "category": "structural_statutory",
            "severity": if phase2 { "critical" } else { "moderate" },
            "title": format!("FL Milestone Inspection & SIRS ({status})"),
            "detail": cit.quote.clone(),
            "citation": cit.clone(),
        }));
        json!({
            "detected": true,
            "status": status,
            "summary": cit.quote.clone(),
            "citation": cit,
        })
    } else {
        json!({
            "detected": false,
            "status": "not_referenced",
            "summary": "No Florida Milestone Inspection or Structural Integrity Reserve Study (SIRS) reference detected.",
            "citation": Value::Null,
        })
    };

    let mut lifecycle_shortfalls: Vec<Value> = Vec::new();
    for (g, cit) in find_all_captures(&LIFECYCLE_SHORTFALL_RE, &singles, &windows) {
        let comp = g.get(1).cloned().unwrap_or_else(|| "Building Component".to_string());
        let est_cost = MONEY_ANY_RE
            .captures(&cit.quote)
            .and_then(|c| c.get(1))
            .and_then(|m| parse_money_usd(m.as_str()))
            .map(|v| v.round() as i64);
        lifecycle_shortfalls.push(json!({
            "component": comp,
            "estimatedReplacementCostUsd": est_cost,
            "summary": cit.quote.clone(),
            "citation": cit.clone(),
        }));
        findings.push(json!({
            "category": "lifecycle_shortfall",
            "severity": "major",
            "title": format!("Component Lifecycle / Reserve Shortfall: {}", g.get(1).map(|s| s.as_str()).unwrap_or("Major System")),
            "detail": cit.quote.clone(),
            "citation": cit,
        }));
    }

    let mut litigation_items: Vec<Value> = Vec::new();
    for (g, cit) in find_all_captures(&LITIGATION_RE, &singles, &windows) {
        if NO_LITIGATION_RE.is_match(&g[0]) || NO_LITIGATION_RE.is_match(&cit.quote) {
            continue;
        }
        let low = cit.quote.to_ascii_lowercase();
        let is_structural = low.contains("construction defect")
            || low.contains("structural")
            || low.contains("water intrusion")
            || low.contains("balcon")
            || low.contains("builder")
            || low.contains("developer")
            || low.contains("safety");
        let claimed = MONEY_ANY_RE
            .captures(&cit.quote)
            .and_then(|c| c.get(1))
            .and_then(|m| parse_money_usd(m.as_str()))
            .map(|v| v.round() as i64);
        litigation_items.push(json!({
            "isStructuralOrSafety": is_structural,
            "claimedAmountUsd": claimed,
            "summary": cit.quote.clone(),
            "citation": cit.clone(),
        }));
        findings.push(json!({
            "category": "litigation",
            "severity": if is_structural { "critical" } else { "major" },
            "title": if is_structural {
                "Active HOA Construction Defect / Structural Litigation".to_string()
            } else {
                "Active HOA Litigation".to_string()
            },
            "detail": cit.quote.clone(),
            "citation": cit,
        }));
    }

    // ---------------- 3. Lifestyle, Investor & Governance Rules ----------------
    let rental_cap_hit = find_first_capture(&RENTAL_CAP_RE, &singles, &windows);
    let waitlist_hit = find_first_capture(&RENTAL_WAITLIST_RE, &singles, &windows);
    let rental_cap_obj = if let Some((g, cit)) = rental_cap_hit {
        let cap_str = [&g[1], &g[2], &g[3]]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default();
        let cap_pct = parse_pct(&cap_str);
        let wl_count = waitlist_hit
            .as_ref()
            .and_then(|(wg, _)| wg.get(1))
            .and_then(|s| s.parse::<u32>().ok());
        findings.push(json!({
            "category": "governance_rental",
            "severity": "moderate",
            "title": format!(
                "HOA Rental Cap: {}{}",
                cap_pct.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "Restricted".to_string()),
                if wl_count.is_some() || waitlist_hit.is_some() { " (Waitlist Active)" } else { "" }
            ),
            "detail": cit.quote.clone(),
            "citation": cit.clone(),
        }));
        json!({
            "hasRentalCap": true,
            "capPercent": cap_pct,
            "hasWaitlist": waitlist_hit.is_some(),
            "waitlistCount": wl_count,
            "summary": cit.quote.clone(),
            "citation": cit,
        })
    } else {
        json!({
            "hasRentalCap": false,
            "capPercent": Value::Null,
            "hasWaitlist": waitlist_hit.is_some(),
            "waitlistCount": Value::Null,
            "summary": "No explicit percentage rental cap detected in uploaded CC&Rs/rules.",
            "citation": waitlist_hit.map(|(_, c)| c),
        })
    };

    let str_hit = find_first_capture(&STR_MIN_LEASE_RE, &singles, &windows);
    let str_obj = if let Some((g, cit)) = str_hit {
        let pairs = [
            (g.get(1), g.get(2)),
            (g.get(3), g.get(4)),
            (g.get(5), g.get(6)),
            (g.get(7), g.get(8)),
            (g.get(9), g.get(10)),
        ];
        let mut min_days: Option<u32> = None;
        for (num_opt, unit_opt) in pairs {
            if let (Some(n_str), Some(u_str)) = (num_opt, unit_opt) {
                if let Ok(n) = n_str.parse::<u32>() {
                    let u_low = u_str.to_ascii_lowercase();
                    let days = if u_low.starts_with("year") {
                        n * 365
                    } else if u_low.starts_with("month") {
                        n * 30
                    } else {
                        n
                    };
                    min_days = Some(days);
                    break;
                }
            }
        }
        let low = cit.quote.to_ascii_lowercase();
        let str_banned = min_days.map(|d| d >= 30).unwrap_or(false)
            || low.contains("airbnb")
            || low.contains("vrbo")
            || low.contains("short-term rental")
            || low.contains("short term rental");
        if min_days.is_none() && str_banned && low.contains("30") {
            min_days = Some(30);
        }
        findings.push(json!({
            "category": "governance_str",
            "severity": "informational",
            "title": match min_days {
                Some(d) => format!("Short-Term Rentals Restricted ({d}-Day Minimum Lease)"),
                None => "Short-Term Rentals (Airbnb/VRBO) Prohibited".to_string(),
            },
            "detail": cit.quote.clone(),
            "citation": cit.clone(),
        }));
        json!({
            "strProhibited": str_banned,
            "minimumLeaseDays": min_days,
            "summary": cit.quote.clone(),
            "citation": cit,
        })
    } else {
        json!({
            "strProhibited": false,
            "minimumLeaseDays": Value::Null,
            "summary": "No explicit short-term rental or minimum-lease day clause detected.",
            "citation": Value::Null,
        })
    };

    let pet_hit = find_first_capture(&PET_RULE_RE, &singles, &windows);
    let pet_obj = if let Some((_, cit)) = pet_hit {
        let low = cit.quote.to_ascii_lowercase();
        let weight_lbs = PET_WEIGHT_RE
            .captures(&cit.quote)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        let max_pets = PET_COUNT_RE.captures(&cit.quote).and_then(|c| {
            match c.get(1)?.as_str().to_ascii_lowercase().as_str() {
                "one" | "1" => Some(1u32),
                "two" | "2" => Some(2u32),
                "three" | "3" => Some(3u32),
                "4" => Some(4u32),
                _ => None,
            }
        });
        let breed_restricted = low.contains("breed") || low.contains("pit bull") || low.contains("rottweiler") || low.contains("aggressive");
        json!({
            "restricted": true,
            "maxPetCount": max_pets,
            "maxWeightLbs": weight_lbs,
            "breedRestrictions": breed_restricted,
            "summary": cit.quote.clone(),
            "citation": cit,
        })
    } else {
        json!({
            "restricted": false,
            "maxPetCount": Value::Null,
            "maxWeightLbs": Value::Null,
            "breedRestrictions": false,
            "summary": "No explicit pet weight/count restriction detected.",
            "citation": Value::Null,
        })
    };

    let ev_hit = find_first_capture(&EV_RULE_RE, &singles, &windows);
    let solar_hit = find_first_capture(&SOLAR_RULE_RE, &singles, &windows);
    let flooring_hit = find_first_capture(&FLOORING_RULE_RE, &singles, &windows);
    let remodel_hit = find_first_capture(&REMODEL_RULE_RE, &singles, &windows);
    let smoking_hit = find_first_capture(&SMOKING_RULE_RE, &singles, &windows);

    let cov_hit = find_first_capture(&MASTER_INS_COVERAGE_RE, &singles, &windows);
    let ded_hit = find_first_capture(&MASTER_DEDUCTIBLE_RE, &singles, &windows);
    let coverage_type = cov_hit
        .as_ref()
        .map(|(g, _)| {
            let k = g[1].to_ascii_lowercase();
            if k.contains("bare") || k.contains("studs") {
                "Bare Walls (Studs-Out) — Buyer HO-6 Must Cover Interior Finishes"
            } else if k.contains("walls") || k.contains("all") {
                "Walls-In (All-In)"
            } else {
                "Single Entity (Original Specifications)"
            }
        })
        .unwrap_or("Unspecified — Verify Master Hazard Policy EOI");

    let ded_pct = ded_hit
        .as_ref()
        .and_then(|(g, _)| {
            g.get(1)
                .filter(|s| !s.is_empty())
                .or_else(|| g.get(3).filter(|s| !s.is_empty()))
        })
        .and_then(|s| parse_pct(s));
    let ded_usd = ded_hit
        .as_ref()
        .and_then(|(g, _)| {
            g.get(2)
                .filter(|s| !s.is_empty())
                .or_else(|| g.get(4).filter(|s| !s.is_empty()))
        })
        .and_then(|s| parse_money_usd(s))
        .map(|v| v.round() as i64);

    let deductible_shock = ded_pct.map(|p| p >= 5.0).unwrap_or(false)
        || ded_usd.map(|d| d >= 50_000).unwrap_or(false);
    let ins_citation = ded_hit
        .as_ref()
        .map(|(_, c)| c.clone())
        .or_else(|| cov_hit.as_ref().map(|(_, c)| c.clone()));

    if deductible_shock {
        if let Some(ref ic) = ins_citation {
            findings.push(json!({
                "category": "master_insurance",
                "severity": if ded_pct.unwrap_or(0.0) > 5.0 { "critical" } else { "major" },
                "title": format!(
                    "Master Insurance Deductible Shock ({})",
                    ded_pct
                        .map(|p| format!("{p:.1}% deductible"))
                        .or_else(|| ded_usd.map(|d| format!("${d} deductible")))
                        .unwrap_or_else(|| "High Deductible".to_string())
                ),
                "detail": "High master policy wildfire/wind/water deductible exposes owners to loss-assessment charges (verify HO-6 Loss Assessment coverage) and can violate Fannie Mae/Freddie Mac 5% max deductible rules.",
                "citation": ic,
            }));
        }
    }

    // ---------------- 4. Fannie Mae / Freddie Mac / FHA Warrantability Checklist ----------------
    let mut warrantability_checklist: Vec<Value> = Vec::new();
    let mut deal_breakers = 0usize;
    let mut watch_flags = 0usize;

    // Check 4A: Delinquency > 15%
    if let Some((delinq, ref cit)) = delinquency_pair {
        let non_warrantable = delinq > 15.0;
        if non_warrantable {
            deal_breakers += 1;
        }
        warrantability_checklist.push(json!({
            "code": "delinquency_60_days",
            "label": "Assessment Delinquency (>60 Days Past Due <= 15%)",
            "status": if non_warrantable { "non_warrantable" } else { "pass" },
            "threshold": "<= 15% of units",
            "observed": format!("{delinq:.1}%"),
            "impact": if non_warrantable {
                "Exceeds Fannie Mae, Freddie Mac & FHA 15% delinquency limit — forces expensive Non-QM or portfolio financing."
            } else {
                "Within conventional agency delinquency tolerance."
            },
            "citation": cit,
        }));
    }

    // Check 4B: Reserve Budget Allocation >= 10%
    if let Some((alloc_pct, ref cit_opt)) = reserve_alloc_pct {
        let non_warrantable = alloc_pct < 10.0;
        if non_warrantable {
            deal_breakers += 1;
            if let Some(ref cit) = cit_opt {
                findings.push(json!({
                    "category": "warrantability",
                    "severity": "critical",
                    "title": format!("Non-Warrantable Reserve Allocation ({alloc_pct:.1}% < 10% Budget Minimum)"),
                    "detail": "Fannie Mae / Freddie Mac Full Review requires at least 10% of the annual operating budget allocated to reserves.",
                    "citation": cit,
                }));
            }
        }
        warrantability_checklist.push(json!({
            "code": "reserve_budget_allocation",
            "label": "Annual Budget Allocated to Reserves (>= 10%)",
            "status": if non_warrantable { "non_warrantable" } else { "pass" },
            "threshold": ">= 10% of operating budget",
            "observed": format!("{alloc_pct:.1}%"),
            "impact": if non_warrantable {
                "Less than 10% of annual operating budget goes to reserves — fails conventional Full Review condo budget requirement unless waived by lender reserve study exception."
            } else {
                "Meets standard >=10% agency reserve funding line item."
            },
            "citation": cit_opt,
        }));
    }

    // Check 4C: Single-Entity Ownership > 10%–20%
    if let Some((g, cit)) = find_first_capture(&SINGLE_ENTITY_RE, &singles, &windows) {
        if let Some(own_pct) = parse_pct(&g[1]) {
            let status = if own_pct > 20.0 {
                deal_breakers += 1;
                findings.push(json!({
                    "category": "warrantability",
                    "severity": "critical",
                    "title": format!("Single-Entity Ownership Concentration ({own_pct:.1}% > 20% Limit)"),
                    "detail": "Single investor/entity owns >20% of units — triggers Fannie Mae / Freddie Mac non-warrantable condo denial.",
                    "citation": cit.clone(),
                }));
                "non_warrantable"
            } else if own_pct > 10.0 {
                watch_flags += 1;
                findings.push(json!({
                    "category": "warrantability",
                    "severity": "major",
                    "title": format!("Elevated Single-Entity Ownership ({own_pct:.1}% > 10% FHA Threshold)"),
                    "detail": "Single-entity concentration exceeds 10% (FHA threshold) and approaches the 20% conventional ceiling.",
                    "citation": cit.clone(),
                }));
                "watch"
            } else {
                "pass"
            };
            warrantability_checklist.push(json!({
                "code": "single_entity_ownership",
                "label": "Single-Entity / Investor Concentration (<= 10%–20%)",
                "status": status,
                "threshold": "<= 20% (21+ units) / <= 2 units (5–20 units) / <= 10% FHA",
                "observed": format!("{own_pct:.1}%"),
                "impact": "High single-investor concentration restricts Fannie Mae, Freddie Mac, and FHA condo eligibility.",
                "citation": cit,
            }));
        }
    }

    // Check 4D: Commercial Space > 35%
    if let Some((g, cit)) = find_first_capture(&COMMERCIAL_SPACE_RE, &singles, &windows) {
        if let Some(comm_pct) = parse_pct(&g[1]) {
            let non_warrantable = comm_pct > 35.0;
            if non_warrantable {
                deal_breakers += 1;
                findings.push(json!({
                    "category": "warrantability",
                    "severity": "critical",
                    "title": format!("Excessive Commercial Floor Area ({comm_pct:.1}% > 35% Limit)"),
                    "detail": "Commercial/non-residential floor space >35% violates Fannie Mae, Freddie Mac & FHA condo limits.",
                    "citation": cit.clone(),
                }));
            }
            warrantability_checklist.push(json!({
                "code": "commercial_space_share",
                "label": "Commercial / Mixed-Use Floor Area (<= 35%)",
                "status": if non_warrantable { "non_warrantable" } else { "pass" },
                "threshold": "<= 35% commercial area",
                "observed": format!("{comm_pct:.1}%"),
                "impact": if non_warrantable {
                    "Commercial space >35% makes the project ineligible for Fannie Mae / Freddie Mac / FHA residential condo financing."
                } else {
                    "Mixed-use commercial area is within the 35% agency limit."
                },
                "citation": cit,
            }));
        }
    }

    // Check 4E: Structural / Safety Litigation
    if let Some(struct_lit) = litigation_items
        .iter()
        .find(|l| l.get("isStructuralOrSafety").and_then(|v| v.as_bool()) == Some(true))
    {
        deal_breakers += 1;
        warrantability_checklist.push(json!({
            "code": "structural_or_safety_litigation",
            "label": "No Structural, Safety, or Construction-Defect Litigation",
            "status": "non_warrantable",
            "threshold": "Zero active structural/habitability lawsuits",
            "observed": "Active construction-defect / structural litigation",
            "impact": "Fannie Mae & Freddie Mac prohibit lending in condo projects with unresolved structural, safety, or habitability litigation.",
            "citation": struct_lit.get("citation").cloned().unwrap_or(Value::Null),
        }));
    }

    // Check 4F: Critical Deferred Maintenance (Fannie Mae LL-2021-14 / SB 326 / FL Milestone)
    let sb326_critical = ca_sb326_sb721
        .get("status")
        .and_then(|v| v.as_str())
        == Some("critical_repair_needed");
    let fl_critical = fl_milestone_sirs
        .get("status")
        .and_then(|v| v.as_str())
        == Some("phase2_structural_distress");
    if sb326_critical || fl_critical {
        deal_breakers += 1;
        let cit = if sb326_critical {
            ca_sb326_sb721.get("citation").cloned().unwrap_or(Value::Null)
        } else {
            fl_milestone_sirs.get("citation").cloned().unwrap_or(Value::Null)
        };
        warrantability_checklist.push(json!({
            "code": "critical_deferred_maintenance",
            "label": "No Critical Structural / Elevated-Element Deferred Maintenance",
            "status": "non_warrantable",
            "threshold": "No unresolved unsafe SB 326 / SB 721 or FL Milestone Phase 2 defects",
            "observed": if sb326_critical {
                "CA SB 326 / SB 721 balcony/elevated-element dry-rot or waterproofing remediation required"
            } else {
                "FL Milestone Phase 2 / SIRS structural distress reported"
            },
            "impact": "Post-Surfside Fannie Mae & Freddie Mac critical-repair rules block agency loans until unsafe structural or waterproofing defects are remediated and funded.",
            "citation": cit,
        }));
    }

    // Check 4G: Hotel / Condotel Operations
    if let Some((_, cit)) = find_first_capture(&CONDOTEL_RE, &singles, &windows) {
        deal_breakers += 1;
        findings.push(json!({
            "category": "warrantability",
            "severity": "critical",
            "title": "Condotel / Hotel-Style Transient Use Detected (Non-Warrantable)",
            "detail": "Projects with condotel / mandatory rental-pool / front-desk hotel traits are ineligible for conventional agency loans.",
            "citation": cit.clone(),
        }));
        warrantability_checklist.push(json!({
            "code": "condotel_transient_use",
            "label": "No Hotel / Condotel / Mandatory Rental-Pool Features",
            "status": "non_warrantable",
            "threshold": "Residential governance only (no front-desk hotel or rental pool)",
            "observed": "Condotel / transient hospitality indicator detected",
            "impact": "Condotels are automatically non-warrantable and require 20%–30% down Non-QM financing.",
            "citation": cit,
        }));
    }

    // Check 4H: Master Insurance Deductible > 5%
    if let Some(dp) = ded_pct {
        if dp > 5.0 {
            deal_breakers += 1;
            warrantability_checklist.push(json!({
                "code": "master_insurance_deductible",
                "label": "Master Property Insurance Deductible <= 5%",
                "status": "non_warrantable",
                "threshold": "<= 5% maximum master hazard/wind/wildfire deductible",
                "observed": format!("{dp:.1}% master deductible"),
                "impact": "Master policies with >5% property/wildfire/wind deductibles fail Fannie Mae & Freddie Mac master insurance guidelines.",
                "citation": ins_citation.clone(),
            }));
        }
    }

    let warrantability_status = if deal_breakers > 0 {
        "Non-Warrantable (Non-QM Likely)"
    } else if watch_flags > 0 || percent_funded.map(|p| p < 30.0).unwrap_or(false) {
        "Watch / Lender Full-Review Required"
    } else {
        "Warrantable (No Agency Deal-Breakers Detected)"
    };

    let mut buyer_questions = Vec::new();
    if percent_funded.map(|p| p < 50.0).unwrap_or(true) {
        buyer_questions.push("Can the HOA manager provide the most recent full Reserve Study funding plan and confirm whether dues increases or special assessments are scheduled over the next 36 months?".to_string());
    }
    if ca_sb326_sb721.get("detected").and_then(|v| v.as_bool()) == Some(true) {
        buyer_questions.push("Has the licensed structural engineer/architect issued a final CA SB 326 compliance report, and how will balcony/elevated-element repairs be funded (reserves vs. seller-paid special assessment at close)?".to_string());
    }
    if deal_breakers > 0 {
        buyer_questions.push("Has any unit in this complex closed with conventional Fannie Mae / Freddie Mac financing in the past 90 days, or is the project currently on the Fannie Mae Condo Project Manager (CPM) unavailable list?".to_string());
    }
    if deductible_shock {
        buyer_questions.push("What per-unit Loss Assessment limit (HO-6 Coverage D) is required to cover the association's master policy deductible if assessed back to unit owners?".to_string());
    }
    if buyer_questions.len() < 3 {
        buyer_questions.push("Are there any unrecorded board-approved special assessments, insurance non-renewal notices, or threatened disputes discussed in executive-session minutes?".to_string());
    }

    let headline = if deal_breakers > 0 {
        format!(
            "High-risk HOA profile: {} Fannie/Freddie warrantability deal-breaker(s){}",
            deal_breakers,
            percent_funded
                .map(|p| format!(" and {p:.1}% funded reserves ({reserve_risk_tier})."))
                .unwrap_or_else(|| ".".to_string())
        )
    } else if let Some(pf) = percent_funded {
        format!(
            "HOA reserves are {pf:.1}% funded ({reserve_risk_tier}) with {} special assessment flag(s).",
            special_assessments.len()
        )
    } else {
        "HOA financial, governance, and warrantability screen completed across uploaded packet.".to_string()
    };

    json!({
        "headline": headline,
        "piiRedactedCount": pii_redacted_count,
        "documentInventory": doc_inventory,
        "financialHealth": {
            "monthlyDuesUsd": monthly_dues.as_ref().map(|(v, _)| (*v * 100.0).round() / 100.0),
            "monthlyDuesCitation": monthly_dues.and_then(|(_, c)| c),
            "operatingBudgetUsd": operating_budget.as_ref().map(|(v, _)| v.round() as i64),
            "operatingBudgetCitation": operating_budget.map(|(_, c)| c),
            "annualReserveContributionUsd": reserve_contrib.as_ref().map(|(v, _)| v.round() as i64),
            "reserveAllocationPctOfBudget": reserve_alloc_pct.as_ref().map(|(p, _)| *p),
            "reserveAllocationCitation": reserve_alloc_pct.and_then(|(_, c)| c),
            "currentReserveBalanceUsd": current_reserve.as_ref().map(|(v, _)| v.round() as i64),
            "currentReserveCitation": current_reserve.map(|(_, c)| c),
            "idealFullyFundedBalanceUsd": ideal_reserve.as_ref().map(|(v, _)| v.round() as i64),
            "idealReserveCitation": ideal_reserve.map(|(_, c)| c),
            "percentFunded": percent_funded,
            "percentFundedCitation": percent_funded_pair.map(|(_, c)| c),
            "reserveRiskTier": reserve_risk_tier,
            "unitCount": unit_count,
            "totalReserveDeficitUsd": total_reserve_deficit_usd,
            "perUnitReserveDeficitUsd": per_unit_reserve_deficit_usd,
            "delinquencyRatePct": delinquency_pair.as_ref().map(|(p, _)| *p),
            "delinquencyCitation": delinquency_pair.map(|(_, c)| c),
            "specialAssessments": special_assessments,
        },
        "structuralAndStatutoryMandates": {
            "caSb326Sb721": ca_sb326_sb721,
            "flMilestoneAndSirs": fl_milestone_sirs,
            "lifecycleShortfalls": lifecycle_shortfalls,
            "litigation": litigation_items,
        },
        "governanceAndLifestyleRules": {
            "rentalCap": rental_cap_obj,
            "shortTermRental": str_obj,
            "petRestrictions": pet_obj,
            "architecturalAndAcousticRules": {
                "evChargerPolicy": ev_hit.as_ref().map(|(_, c)| c.quote.clone()),
                "evChargerCitation": ev_hit.map(|(_, c)| c),
                "solarPolicy": solar_hit.as_ref().map(|(_, c)| c.quote.clone()),
                "solarCitation": solar_hit.map(|(_, c)| c),
                "hardSurfaceFlooringRule": flooring_hit.as_ref().map(|(_, c)| c.quote.clone()),
                "flooringCitation": flooring_hit.map(|(_, c)| c),
                "remodelApprovalPolicy": remodel_hit.as_ref().map(|(_, c)| c.quote.clone()),
                "remodelCitation": remodel_hit.map(|(_, c)| c),
            },
            "smokingPolicy": {
                "prohibited": smoking_hit.is_some(),
                "summary": smoking_hit
                    .as_ref()
                    .map(|(_, c)| c.quote.clone())
                    .unwrap_or_else(|| "No explicit community-wide smoking ban detected.".to_string()),
                "citation": smoking_hit.map(|(_, c)| c),
            },
            "masterInsurance": {
                "coverageType": coverage_type,
                "deductiblePercent": ded_pct,
                "deductibleUsd": ded_usd,
                "deductibleShockWarning": deductible_shock,
                "citation": ins_citation,
            }
        },
        "warrantability": {
            "status": warrantability_status,
            "isWarrantable": deal_breakers == 0,
            "nonQmLoanWarning": deal_breakers > 0,
            "dealBreakerCount": deal_breakers,
            "watchFlagCount": watch_flags,
            "checklist": warrantability_checklist,
        },
        "findings": findings,
        "buyerQuestions": buyer_questions,
        "negotiationScript": if deal_breakers > 0 || percent_funded.map(|p| p < 30.0).unwrap_or(false) {
            "Request that Seller pay 100% of any active, levied, or board-disclosed special assessments in full through escrow at closing, and retain loan-contingency protection until the lender's Condo Project Review (CPM / Full Review) approves the HOA.".to_string()
        } else {
            "Confirm current HOA account good standing, zero unpaid unit fines/assessments, and receipt of full Reserve Study, Master Insurance Evidence of Insurance (EOI), and past 12 months of Board Minutes prior to contingency removal.".to_string()
        },
        "disclaimer": "Factual HOA / condo document analysis only — NOT legal, accounting, actuarial, or mortgage underwriting advice. Owner/member personal names, SSNs, and account numbers are automatically redacted. Verify all CC&R covenants, reserve projections, and agency condo project eligibility with your HOA attorney, CPA, and mortgage underwriter."
    })
}

pub const HOA_REVIEW_INSTRUCTIONS: &str = r#"You are Open Estate Senior HOA / Condo Reserve Study, Governance & Agency Warrantability Analyst.
Review the PII-scrubbed HOA Reserve Study, Annual Budget, CC&Rs/Bylaws/Rules, Resale Certificate/Condo Questionnaire, and Board Minutes inside <untrusted_document_text>.

STRICT RULES:
1. GROUNDED PAGE CITATIONS ONLY: Every item in `additionalFindings` MUST cite an exact `docName`, integer `page` (`[Page N]`), and verbatim `quote` from `<untrusted_document_text>`. Never invent facts, numbers, or page numbers.
2. FINANCIAL, STRUCTURAL & WARRANTABILITY RIGOR: Highlight reserve funding shortfalls (`<30%` critical, `30–69%` fair, `>=70%` strong), CA SB 326/721 balcony waterproofing/dry-rot, FL Milestone/SIRS, special assessments, rental caps, Airbnb/STR minimum lease days, master insurance `Walls-In` vs `Bare Walls` deductible shock, and Fannie Mae / Freddie Mac / FHA non-warrantable triggers (>15% delinquency, <10% reserve budget allocation, >10%–20% single-entity ownership, >35% commercial space, structural litigation, condotel traits).
3. PRIVACY & FAIR HOUSING: Never output personal owner/member names, SSNs, emails, or account numbers, and never reference demographics or protected classes. Do not output markdown code fences or HTML script tags."#;

/// Strict OpenAI Structured Outputs JSON Schema for HOA review synthesis.
pub fn hoa_review_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "executiveHeadline",
            "executiveSummary",
            "overallRiskRating",
            "negotiationScript",
            "buyerQuestions",
            "additionalFindings"
        ],
        "properties": {
            "executiveHeadline": { "type": "string" },
            "executiveSummary": { "type": "string" },
            "overallRiskRating": {
                "type": "string",
                "enum": ["Low Risk / Well-Funded", "Moderate Watch", "High Risk / Underfunded", "Critical / Non-Warrantable"]
            },
            "negotiationScript": { "type": "string" },
            "buyerQuestions": {
                "type": "array",
                "items": { "type": "string" }
            },
            "additionalFindings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["category", "severity", "title", "detail", "citation"],
                    "properties": {
                        "category": {
                            "type": "string",
                            "enum": [
                                "financial_reserves",
                                "special_assessment",
                                "structural_statutory",
                                "lifecycle_shortfall",
                                "litigation",
                                "governance_rental",
                                "governance_str",
                                "master_insurance",
                                "warrantability"
                            ]
                        },
                        "severity": {
                            "type": "string",
                            "enum": ["critical", "major", "moderate", "informational"]
                        },
                        "title": { "type": "string" },
                        "detail": { "type": "string" },
                        "citation": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["docName", "page", "quote"],
                            "properties": {
                                "docName": { "type": "string" },
                                "page": { "type": "integer" },
                                "quote": { "type": "string" }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Full HOA / Condo / Strata analyzer combining pure-Rust PDF/ZIP extraction, pre-LLM PII scrubbing,
/// deterministic financial/statutory/warrantability extraction, and optional Responses API synthesis.
pub async fn analyze_hoa_documents(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    documents: Vec<UploadedDocInput>,
    cache: Option<&dyn LlmCache>,
) -> Result<ModuleOutput> {
    let t0 = Instant::now();
    let batch =
        extract_and_ocr_uploads_async(http, env, &documents, DocumentKind::Hoa, 12, 160_000)
            .await?;

    let mut parsed = parse_hoa_documents(
        &batch.documents,
        Some(report),
        batch.pii_redacted_count,
    );

    init_deterministic_ai_meta(&mut parsed, Some(batch.files));

    let mut warnings = Vec::new();
    append_scrubbed_lines_warning(&mut warnings, batch.scrubbed_lines, "HOA document(s)");

    if env.llm_configured() {
        let brief = build_report_brief_or_default(report, "Subject Condominium / HOA Property");
        let det_snapshot = json!({
            "financialHealth": parsed.get("financialHealth"),
            "structuralAndStatutoryMandates": parsed.get("structuralAndStatutoryMandates"),
            "warrantability": parsed.get("warrantability"),
        });
        let clipped_text = clip_sanitized_text(&batch.sanitized_text, 75_000);

        let prompt = format!(
            "Synthesize the HOA / condo due-diligence findings for this property strictly from DETERMINISTIC_HOA_EXTRACT and <untrusted_document_text>.\n\nPROPERTY_BRIEF_JSON:\n{}\n\nDETERMINISTIC_HOA_EXTRACT:\n{}\n\n{}",
            serde_json::to_string(&brief)?,
            serde_json::to_string(&det_snapshot)?,
            wrap_untrusted_block("untrusted_document_text", &clipped_text)
        );

        let mut opts = LlmCallOptions::new(
            "hoa_review",
            "hoa_review",
            HOA_REVIEW_INSTRUCTIONS,
            prompt,
            hoa_review_schema(),
        );
        opts.models = resolve_model_specs_with_effort(env, Some("medium"));
        opts.max_output_tokens = Some(6_144);
        opts.timeout_ms = Some(120_000);

        match responses_json_with_cache(http, env, opts, cache).await {
            Ok(res) => {
                let raw_out = serde_json::to_string(&res.data).unwrap_or_default();
                if inspect_output_guardrail(&raw_out).is_none() {
                    if let Some(obj) = parsed.as_object_mut() {
                        if let Some(hl) = res.data.get("executiveHeadline").and_then(|v| v.as_str()) {
                            obj.insert(
                                "headline".to_string(),
                                Value::String(redact_title_pii(hl).masked_text),
                            );
                        }
                        if let Some(sum) = res.data.get("executiveSummary").and_then(|v| v.as_str()) {
                            obj.insert(
                                "executiveSummary".to_string(),
                                Value::String(redact_title_pii(sum).masked_text),
                            );
                        }
                        if let Some(rr) = res.data.get("overallRiskRating").and_then(|v| v.as_str()) {
                            obj.insert(
                                "overallRiskRating".to_string(),
                                Value::String(rr.to_string()),
                            );
                        }
                        if let Some(ns) = res.data.get("negotiationScript").and_then(|v| v.as_str()) {
                            obj.insert(
                                "negotiationScript".to_string(),
                                Value::String(redact_title_pii(ns).masked_text),
                            );
                        }
                        let cleaned_qs = redact_pii_string_array(
                            res.data.get("buyerQuestions"),
                            |s| redact_title_pii(s).masked_text,
                        );
                        if !cleaned_qs.is_empty() {
                            obj.insert("buyerQuestions".to_string(), Value::Array(cleaned_qs));
                        }
                        if let Some(extra) = res.data.get("additionalFindings").and_then(|v| v.as_array()) {
                            if let Some(existing) = obj.get_mut("findings").and_then(|v| v.as_array_mut()) {
                                for item in extra {
                                    let title = item
                                        .get("title")
                                        .and_then(|v| v.as_str())
                                        .map(|s| redact_title_pii(s).masked_text)
                                        .unwrap_or_default();
                                    if title.is_empty() {
                                        continue;
                                    }
                                    let already = existing.iter().any(|e| {
                                        e.get("title")
                                            .and_then(|t| t.as_str())
                                            .map(|t| t.eq_ignore_ascii_case(&title))
                                            .unwrap_or(false)
                                    });
                                    if !already {
                                        let mut cleaned_item = item.clone();
                                        if let Some(io) = cleaned_item.as_object_mut() {
                                            io.insert("title".to_string(), Value::String(title));
                                            if let Some(det) = io.get("detail").and_then(|v| v.as_str()) {
                                                io.insert(
                                                    "detail".to_string(),
                                                    Value::String(redact_title_pii(det).masked_text),
                                                );
                                            }
                                            if let Some(cit_obj) =
                                                io.get_mut("citation").and_then(|v| v.as_object_mut())
                                            {
                                                if let Some(q) = cit_obj.get("quote").and_then(|v| v.as_str()) {
                                                    cit_obj.insert(
                                                        "quote".to_string(),
                                                        Value::String(redact_title_pii(q).masked_text),
                                                    );
                                                }
                                            }
                                        }
                                        existing.push(cleaned_item);
                                    }
                                }
                            }
                        }
                    }
                    stamp_ai_enhancement_meta(&mut parsed, &res, true);
                }
            }
            Err(e) => {
                warnings.push(format!(
                    "AI HOA synthesis skipped (deterministic reserve & warrantability analysis complete): {}",
                    scrub_sensitive_text(&e.to_string())
                ));
            }
        }
    }

    if let Some(obj) = parsed.as_object_mut() {
        obj.insert("ms".to_string(), json!(t0.elapsed().as_millis() as u64));
    }

    let file_names: Vec<String> = batch.documents.iter().map(|d| d.name.clone()).collect();
    Ok(ModuleOutput {
        data: parsed,
        sources: vec![SourceItem::new(
            format!(
                "{} HOA Reserve, Governance & Warrantability Analyzer over {}",
                env.brand_name(),
                file_names.join(", ")
            ),
            "https://singlefamily.fanniemae.com/originating-underwriting/condo-co-op-and-pud-eligibility",
        )],
        warnings,
    })
}
