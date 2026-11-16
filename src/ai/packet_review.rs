// Whole Disclosure Packet Ingestion & Cross-Document Contradiction Matrix (`POST /api/packet-review`).
// - Supports up to 20 PDFs in a single packet batch PLUS pure-Rust `.zip` archive unpacking
//   with strict zip-bomb size, entry count, non-overlapping header, path-traversal, and 40:1
//   compression-ratio guardrails (`crate::ai::pdf`).
// - Automatically classifies each uploaded document into:
//   `seller_disclosure`, `home_inspection`, `termite_wdo`, `roof`, `sewer`, `foundation`,
//   `prelim_title`, `hoa_reserves`, `nhd`, `permits`, `other`.
// - Runs Cross-Document & Public-Record Contradiction Detection comparing Seller Disclosure
//   representations ("No known leaks", "No unpermitted additions", "No drainage/settling issues",
//   "No neighborhood noise/nuisances", "No HOA special assessments", etc.) against:
//     1) Uploaded Inspection / WDO / Roof / Sewer / Foundation / HOA / Prelim findings with exact
//        `{ sourceDoc, sourcePage, quote }` vs `{ contradictingDoc, contradictingPage, quote }`.
//     2) Subject property's public-record `report` JSON (Assessor sq ft vs disclosed/listing/
//        footprint sq ft, missing building permits, FEMA SFHA flood zone, CAL FIRE / CGS
//        liquefaction/fault/landslide zones, EPA/GeoTracker proximity & noise, active liens/UCC-1).
// - Outputs unified Deal-Breaker Triage (`Critical / Major / Moderate / Informational`),
//   Total Deduplicated Repair & Liability Exposure (`low / mid / high USD`) without double-counting
//   overlapping defects cited across multiple reports, and Top Questions for Listing Agent / Seller.

use crate::ai::doc_pipeline::{
    append_scrubbed_lines_warning, build_report_brief_or_default, clip_sanitized_text,
    init_deterministic_ai_meta, redact_pii_string_array, stamp_ai_enhancement_meta,
};
use crate::ai::hoa_review::{parse_hoa_documents, DocPageCitation};
use crate::ai::llm::{responses_json_with_cache, LlmCache, LlmCallOptions};
use crate::ai::pdf::{
    extract_and_ocr_uploads_async, ExtractedDocument, UploadedDocInput, MAX_PACKET_DOCUMENTS,
};
use crate::ai::title_review::{parse_title_report_text, redact_title_pii};
use crate::guardrails::{
    inspect_output_guardrail, scrub_sensitive_text, wrap_untrusted_block, DocumentKind,
};
use crate::shared::{EnvConfig, HttpClient, ModuleOutput, SourceItem};
use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

pub const PACKET_DOC_CATEGORIES: &[&str] = &[
    "seller_disclosure",
    "home_inspection",
    "termite_wdo",
    "roof",
    "sewer",
    "foundation",
    "prelim_title",
    "hoa_reserves",
    "nhd",
    "permits",
    "other",
];

pub const TRIAGE_TIERS: &[&str] = &["Critical", "Major", "Moderate", "Informational"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCitation {
    pub source_doc: String,
    pub source_page: u32,
    pub quote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContradictingCitation {
    pub contradicting_doc: String,
    pub contradicting_page: u32,
    pub quote: String,
}

#[derive(Debug, Clone)]
struct SellerClaim {
    topic_key: &'static str,
    topic_label: &'static str,
    doc_name: String,
    page: u32,
    quote: String,
}

#[derive(Debug, Clone)]
struct RawFinding {
    dedup_key: &'static str,
    topic_key: &'static str,
    trade: &'static str,
    title: String,
    detail: String,
    severity: &'static str, // Critical | Major | Moderate | Informational
    doc_category: &'static str,
    citation: DocPageCitation,
    cost_low: i64,
    cost_high: i64,
    explicit_bid: bool,
    is_liability: bool,
}

fn severity_rank(sev: &str) -> u8 {
    match sev {
        "Critical" => 4,
        "Major" => 3,
        "Moderate" => 2,
        _ => 1,
    }
}

fn clean_excerpt(raw: &str, max_chars: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let masked = redact_title_pii(&collapsed).masked_text;
    if masked.chars().count() <= max_chars {
        masked
    } else {
        let head: String = masked.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", head.trim_end())
    }
}

/// Calibrates licensed contractor repair pricing by US metro / state from `report` JSON
/// (or applies an explicit caller `override_multiplier` clamped to `0.5..=3.0`).
pub fn calibrate_regional_cost_multiplier(
    report: Option<&Value>,
    override_multiplier: Option<f64>,
) -> (f64, String) {
    crate::registry::rates_and_costs::calibrate_regional_cost_multiplier_from_report(
        report,
        override_multiplier,
    )
}

/// Automatically classifies an uploaded PDF/text document into one of the 11 packet categories.
pub fn classify_packet_document(doc: &ExtractedDocument) -> (&'static str, f64) {
    let name_l = doc.name.to_ascii_lowercase();
    let sample_l: String = doc
        .formatted_text
        .chars()
        .take(6_000)
        .collect::<String>()
        .to_ascii_lowercase();

    let mut scores: BTreeMap<&'static str, i32> = BTreeMap::new();
    for &cat in PACKET_DOC_CATEGORIES {
        scores.insert(cat, 0);
    }
    let mut bump = |cat: &'static str, pts: i32| {
        if let Some(v) = scores.get_mut(cat) {
            *v += pts;
        }
    };

    // 1. Filename signals
    if name_l.contains("tds")
        || name_l.contains("spq")
        || name_l.contains("seller")
        || name_l.contains("disclosure") && !name_l.contains("nhd") && !name_l.contains("hazard")
    {
        bump("seller_disclosure", 8);
    }
    if name_l.contains("nhd") || name_l.contains("natural_hazard") || name_l.contains("natural-hazard") || name_l.contains("hazard_disclosure") {
        bump("nhd", 9);
    }
    if name_l.contains("wdo") || name_l.contains("termite") || name_l.contains("pest") {
        bump("termite_wdo", 9);
    }
    if name_l.contains("sewer") || name_l.contains("lateral") {
        bump("sewer", 9);
    }
    if name_l.contains("roof") {
        bump("roof", 9);
    }
    if name_l.contains("foundation") || name_l.contains("structural") || name_l.contains("geotech") || name_l.contains("chimney") {
        bump("foundation", 8);
    }
    if name_l.contains("prelim") || name_l.contains("title") || name_l.contains("deed") {
        bump("prelim_title", 9);
    }
    if name_l.contains("hoa")
        || name_l.contains("reserve")
        || name_l.contains("cc&r")
        || name_l.contains("ccr")
        || name_l.contains("bylaw")
        || name_l.contains("resale")
        || name_l.contains("minutes")
    {
        bump("hoa_reserves", 9);
    }
    if name_l.contains("permit") || name_l.contains("3r") || name_l.contains("building_record") {
        bump("permits", 9);
    }
    if name_l.contains("home_inspection")
        || name_l.contains("home-inspection")
        || name_l.contains("general_inspection")
        || name_l.contains("property_inspection")
        || (name_l.contains("inspection")
            && !name_l.contains("roof")
            && !name_l.contains("termite")
            && !name_l.contains("wdo")
            && !name_l.contains("sewer")
            && !name_l.contains("foundation"))
    {
        bump("home_inspection", 7);
    }

    // 2. Document body / header signals
    if sample_l.contains("transfer disclosure statement")
        || sample_l.contains("seller property questionnaire")
        || sample_l.contains("seller's disclosure")
        || sample_l.contains("seller disclosure")
        || sample_l.contains("are you (seller) aware of")
    {
        bump("seller_disclosure", 10);
    }
    if sample_l.contains("natural hazard disclosure statement")
        || sample_l.contains("special flood hazard area") && sample_l.contains("alquist-priolo")
        || sample_l.contains("very high fire hazard severity zone") && sample_l.contains("statutory")
    {
        bump("nhd", 10);
    }
    if sample_l.contains("wood destroying pests and organisms")
        || sample_l.contains("section 1") && (sample_l.contains("subterranean termite") || sample_l.contains("drywood termite") || sample_l.contains("fungus / dry rot") || sample_l.contains("wood destroying"))
        || sample_l.contains("structural pest control")
    {
        bump("termite_wdo", 11);
    }
    if sample_l.contains("sewer lateral")
        || sample_l.contains("camera scope")
        || sample_l.contains("root intrusion") && sample_l.contains("cleanout")
        || sample_l.contains("video pipe inspection")
    {
        bump("sewer", 10);
    }
    if (sample_l.contains("roof inspection") || sample_l.contains("roof certification") || sample_l.contains("roofing evaluation"))
        && !sample_l.contains("electrical panel")
        && !sample_l.contains("kitchen appliances")
    {
        bump("roof", 10);
    }
    if (sample_l.contains("foundation inspection")
        || sample_l.contains("floor elevation survey")
        || sample_l.contains("structural engineering evaluation"))
        && !sample_l.contains("kitchen appliances")
    {
        bump("foundation", 9);
    }
    if sample_l.contains("preliminary report")
        || sample_l.contains("schedule a") && sample_l.contains("schedule b")
        || sample_l.contains("alta commitment")
        || sample_l.contains("deed of trust") && sample_l.contains("vested in")
    {
        bump("prelim_title", 11);
    }
    if sample_l.contains("percent funded")
        || sample_l.contains("fully funded balance")
        || sample_l.contains("reserve study")
        || sample_l.contains("covenants, conditions and restrictions")
        || sample_l.contains("condominium project questionnaire")
    {
        bump("hoa_reserves", 10);
    }
    if sample_l.contains("building permit history")
        || sample_l.contains("permit #") && sample_l.contains("finaled")
        || sample_l.contains("report of residential building record")
    {
        bump("permits", 10);
    }
    if sample_l.contains("general home inspection")
        || sample_l.contains("property inspection report")
        || (sample_l.contains("electrical") && sample_l.contains("plumbing") && sample_l.contains("hvac"))
        || (sample_l.contains("roof") && sample_l.contains("water heater") && sample_l.contains("crawlspace"))
    {
        bump("home_inspection", 8);
    }

    let mut best_cat = "other";
    let mut best_score = 0i32;
    for (&cat, &sc) in &scores {
        if sc > best_score {
            best_score = sc;
            best_cat = cat;
        }
    }

    if best_score <= 0 {
        ("other", 0.50)
    } else {
        let conf = (0.55 + (best_score as f64 * 0.04)).clamp(0.55, 0.99);
        (best_cat, (conf * 100.0).round() / 100.0)
    }
}

// ------------------------- Seller Disclosure Negative Claim Detectors -------------------------

static SELLER_NO_LEAKS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:roof\s+|plumbing\s+|water\s+|moisture\s+|interior\s+)*(?:leaks?|moisture|water\s+intrusion|staining|seepage)|(?:roof\s+leaks?|water\s+intrusion|moisture(?:\s+or\s+leaks?)?|past\s+or\s+present\s+leaks?)[^\n]{0,65}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b|\bNone\s+known\b))",
    )
    .unwrap()
});
static SELLER_NO_UNPERMITTED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?unpermitted\s+(?:additions?|alterations?|work|remodels?|conversions?|modifications?)|all\s+(?:additions?|remodels?|renovations?)\s+(?:were\s+)?permitted|(?:room\s+additions?|structural\s+modifications?|without\s+necessary\s+permits)[^\n]{0,70}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b))",
    )
    .unwrap()
});
static SELLER_NO_DRAINAGE_SETTLING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:drainage|grading|flooding|settling|soil|foundation\s+movement|sliding)(?:\s+or\s+(?:settling|drainage|grading|foundation))?\s*(?:issues?|problems?|defects?)?|(?:flooding,\s*drainage|settling,\s*slippage|soil\s+problems?)[^\n]{0,70}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b))",
    )
    .unwrap()
});
static SELLER_NO_NOISE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:neighborhood\s+)?(?:noise|nuisances?|odors?|traffic\s+noise|airport\s+noise)|(?:neighborhood\s+noise\s+problems?|other\s+nuisances?)[^\n]{0,65}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b))",
    )
    .unwrap()
});
static SELLER_NO_HOA_ASSESS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:hoa\s+)?(?:special\s+assessments?|pending\s+hoa\s+litigation)|(?:hoa\s+special\s+assessments?|pending\s+special\s+assessments?)[^\n]{0,60}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b))",
    )
    .unwrap()
});
static SELLER_NO_WDO_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:termites?|wood[- ]destroying\s+(?:pests?|organisms?)|dry\s+rot)|(?:termites?|dry\s+rot|pest\s+infestation)[^\n]{0,60}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b))",
    )
    .unwrap()
});
static SELLER_NO_SEWER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:sewer|septic|drain\s+line|backups?|root\s+intrusion)\s*(?:issues?|problems?|defects?|backups?)?|(?:sewer\s+system\s+backups?|sewer\s+lateral\s+defects?)[^\n]{0,60}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b))",
    )
    .unwrap()
});
static SELLER_NO_LIENS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:liens?|lawsuits?|leased\s+solar|ucc[- ]?1|encumbrances?)|(?:lawsuits?\s+or\s+liens?|liens?\s+or\s+encumbrances?|solar\s+(?:lease|ucc[- ]?1))[^\n]{0,60}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b|\bNone\b))",
    )
    .unwrap()
});
static SELLER_NO_ENV_HAZARDS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:no\s+(?:known\s+)?(?:environmental\s+hazards?|hazardous\s+materials?|toxic\s+waste|soil\s+contamination|underground\s+storage\s+tanks?)|(?:environmental\s+hazards?|hazardous\s+substances?|contamination)[^\n]{0,65}?(?:\[\s*[xX✓]\s*\]\s*No\b|:\s*No\b))",
    )
    .unwrap()
});
static CLAIMED_SQFT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:living\s+area|gross\s+living\s+area|\bGLA\b|approx(?:\.|imate)?\s+(?:living\s+area|square\s+footage|sq\.?\s*ft\.?)|disclosed\s+(?:living\s+area|square\s+footage|sq\.?\s*ft\.?)|(?:property|home|residence)\s+(?:square\s+footage|sq\.?\s*ft\.?)|marketed\s+(?:at|as)|total\s+living\s+space|house\s+measured\s+at)\s*(?:\([A-Za-z]+\))?\s*[:=\-]?\s*([1-9],[0-9]{3}|[1-9][0-9]{2,3})\s*(?:sq\.?\s*ft\.?|sf|square\s+feet)",
    )
    .unwrap()
});

// ------------------------- Defect / Finding Detectors Across Packet PDFs -------------------------

static DOLLAR_RANGE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\$([0-9,]+)\s*(?:[-–—]|to)\s*\$([0-9,]+)").unwrap()
});
static SINGLE_DOLLAR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\$([0-9,]+(?:\.\d{2})?)").unwrap()
});

fn parse_dollar_int(s: &str) -> Option<i64> {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    let v = digits.parse::<f64>().ok()?;
    if v.is_finite() && v >= 100.0 && v <= 2_000_000.0 {
        Some(v.round() as i64)
    } else {
        None
    }
}

fn extract_inline_cost_range(line: &str) -> Option<(i64, i64)> {
    if let Some(caps) = DOLLAR_RANGE_RE.captures(line) {
        if let (Some(lo), Some(hi)) = (parse_dollar_int(&caps[1]), parse_dollar_int(&caps[2])) {
            if hi >= lo {
                return Some((lo, hi));
            }
        }
    }
    if let Some(caps) = SINGLE_DOLLAR_RE.captures(line) {
        if let Some(v) = parse_dollar_int(&caps[1]) {
            let lo = ((v as f64) * 0.90).round() as i64;
            let hi = ((v as f64) * 1.15).round() as i64;
            return Some((lo, hi));
        }
    }
    None
}

struct DefectPatternRule {
    dedup_key: &'static str,
    topic_key: &'static str,
    trade: &'static str,
    title: &'static str,
    severity: &'static str,
    base_low: i64,
    base_high: i64,
    re: Regex,
}

static DEFECT_RULES: Lazy<Vec<DefectPatternRule>> = Lazy::new(|| {
    vec![
        DefectPatternRule {
            dedup_key: "roof_leak_or_covering",
            topic_key: "leaks_moisture",
            trade: "Roofing",
            title: "Active Roof Leak / Failed Flashing & Worn Roof Covering",
            severity: "Critical",
            base_low: 8_500,
            base_high: 16_500,
            re: Regex::new(
                r"(?i)\b(?:active\s+(?:roof\s+)?leak|roof\s+leak|leaking\s+roof|moisture\s+staining\s+(?:at|in|under)\s+(?:attic|roof|rafter|sheathing|chimney)|failed\s+(?:roof|chimney|valley)\s+flashing|end[- ]of[- ]life\s+(?:shingles|roof)|roof\s+replacement\s+recommended|deteriorated\s+composition\s+shingles|water\s+intrusion\s+(?:through|at)\s+roof)\b",
            )
            .unwrap(),
        },
        DefectPatternRule {
            dedup_key: "wdo_section1_pest_or_rot",
            topic_key: "pests_wdo",
            trade: "Wood Destroying Pests (Section 1)",
            title: "Section 1 Wood-Destroying Pests / Termites & Fungus Dry-Rot",
            severity: "Critical",
            base_low: 3_200,
            base_high: 7_800,
            re: Regex::new(
                r"(?i)\b(?:section\s*1\b[^\n]{0,90}(?:termite|fungus|dry\s+rot|beetle|infestation|damage)|subterranean\s+termites?|drywood\s+termites?|fungus\s*(?:/|and)?\s*dry[- ]rot\s+damage|wood[- ]destroying\s+(?:pests?|organisms?)\s+infestation)\b",
            )
            .unwrap(),
        },
        DefectPatternRule {
            dedup_key: "interior_plumbing_leak_moisture",
            topic_key: "leaks_moisture",
            trade: "Plumbing / Sewer",
            title: "Active Plumbing Leak / Subfloor Moisture Intrusion",
            severity: "Major",
            base_low: 1_800,
            base_high: 4_800,
            re: Regex::new(
                r"(?i)\b(?:active\s+(?:plumbing|supply|drain|shower\s+pan)\s+leak|elevated\s+moisture\s+meter|water\s+damage\s+below\s+(?:bathroom|kitchen|shower)|leaking\s+galvanized|polybutylene\s+pipe)\b",
            )
            .unwrap(),
        },
        DefectPatternRule {
            dedup_key: "sewer_lateral_defect",
            topic_key: "sewer_plumbing",
            trade: "Plumbing / Sewer",
            title: "Sewer Lateral Root Intrusion / Offset / Cracked Clay Pipe",
            severity: "Critical",
            base_low: 6_500,
            base_high: 14_000,
            re: Regex::new(
                r"(?i)\b(?:sewer\s+lateral[^\n]{0,90}(?:root\s+intrusion|offset|cracked|broken|collapsed|standing\s+water|belly|replacement)|heavy\s+root\s+intrusion\s+in\s+(?:main\s+)?sewer|trenchless\s+sewer\s+replacement)\b",
            )
            .unwrap(),
        },
        DefectPatternRule {
            dedup_key: "electrical_panel_hazard",
            topic_key: "electrical_safety",
            trade: "Electrical",
            title: "Hazardous Electrical Panel (Zinsco / FPE / Knob-and-Tube / Overheated Breakers)",
            severity: "Critical",
            base_low: 3_500,
            base_high: 7_500,
            re: Regex::new(
                r"(?i)\b(?:zinsco|federal\s+pacific|stab[- ]lok|knob[- ]and[- ]tube|ungrounded\s+cloth\s+wiring|double[- ]tapped\s+breakers?\s+with\s+scorching|panel\s+replacement\s+required)\b",
            )
            .unwrap(),
        },
        DefectPatternRule {
            dedup_key: "hvac_end_of_life",
            topic_key: "hvac_system",
            trade: "HVAC",
            title: "Defective / Cracked Heat Exchanger or End-of-Life HVAC System",
            severity: "Major",
            base_low: 6_000,
            base_high: 13_500,
            re: Regex::new(
                r"(?i)\b(?:cracked\s+heat\s+exchanger|inoperable\s+(?:furnace|air\s+conditioner|condenser|hvac)|hvac\s+(?:past|at\s+end\s+of)\s+useful\s+life|asbestos[- ]wrapped\s+duct\w*)\b",
            )
            .unwrap(),
        },
        DefectPatternRule {
            dedup_key: "foundation_settlement_or_drainage",
            topic_key: "drainage_settling",
            trade: "Foundation / Drainage",
            title: "Foundation Differential Settlement / Cracking & Crawlspace Grading",
            severity: "Critical",
            base_low: 7_500,
            base_high: 22_000,
            re: Regex::new(
                r"(?i)\b(?:differential\s+settlement|structural\s+foundation\s+crack|sloping\s+floors?\s+exceeding|standing\s+water\s+in\s+crawl\s*space|negative\s+grading\s+ponding\s+against\s+foundation|unbraced\s+cripple\s+walls?|helical\s+piers?\s+recommended)\b",
            )
            .unwrap(),
        },
        DefectPatternRule {
            dedup_key: "unpermitted_addition_or_conversion",
            topic_key: "unpermitted_additions",
            trade: "Foundation / Drainage",
            title: "Unpermitted Addition / Garage Conversion / Non-Code Construction",
            severity: "Major",
            base_low: 5_000,
            base_high: 18_000,
            re: Regex::new(
                r"(?i)\b(?:unpermitted\s+(?:addition|sunroom|enclosed\s+patio|garage\s+conversion|adu|bathroom|bedroom)|constructed\s+without\s+(?:building\s+)?permits?|no\s+finaled?\s+permit\s+on\s+file\s+for\s+(?:addition|conversion|remodel))\b",
            )
            .unwrap(),
        },
    ]
});

fn extract_seller_claims(docs: &[(&ExtractedDocument, &'static str)]) -> Vec<SellerClaim> {
    let mut claims = Vec::new();
    let patterns: &[(&str, &str, &Lazy<Regex>)] = &[
        ("leaks_moisture", "Moisture / Roof & Plumbing Leaks", &SELLER_NO_LEAKS_RE),
        ("unpermitted_additions", "Permits & Room Additions", &SELLER_NO_UNPERMITTED_RE),
        ("drainage_settling", "Drainage, Grading & Foundation Settling", &SELLER_NO_DRAINAGE_SETTLING_RE),
        ("noise_nuisances", "Neighborhood Noise & Nuisances", &SELLER_NO_NOISE_RE),
        ("env_contamination", "Environmental Hazards & Contamination", &SELLER_NO_ENV_HAZARDS_RE),
        ("hoa_assessments", "HOA Special Assessments & Litigation", &SELLER_NO_HOA_ASSESS_RE),
        ("pests_wdo", "Termites / Wood-Destroying Organisms", &SELLER_NO_WDO_RE),
        ("sewer_plumbing", "Sewer Lateral & Drain Line", &SELLER_NO_SEWER_RE),
        ("liens_solar", "Title Liens & Solar UCC-1 Encumbrances", &SELLER_NO_LIENS_RE),
    ];

    for &(doc, cat) in docs {
        let low_head: String = doc.formatted_text.chars().take(3_000).collect::<String>().to_ascii_lowercase();
        let is_seller_doc = cat == "seller_disclosure"
            || low_head.contains("transfer disclosure")
            || low_head.contains("seller property questionnaire")
            || low_head.contains("seller disclosure");
        if !is_seller_doc {
            continue;
        }
        for page in &doc.pages {
            for line in page.text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                for &(t_key, t_label, re) in patterns {
                    if re.is_match(trimmed)
                        && !claims.iter().any(|c: &SellerClaim| c.topic_key == t_key)
                    {
                        claims.push(SellerClaim {
                            topic_key: t_key,
                            topic_label: t_label,
                            doc_name: doc.name.clone(),
                            page: page.page_number,
                            quote: clean_excerpt(trimmed, 220),
                        });
                    }
                }
            }
        }
    }
    claims
}

fn find_claimed_sqft_in_packet(
    docs: &[(&ExtractedDocument, &'static str)],
) -> Option<(i64, DocPageCitation)> {
    for target_seller in [true, false] {
        for &(doc, cat) in docs {
            if matches!(cat, "permits" | "nhd" | "hoa_reserves" | "prelim_title") {
                continue;
            }
            let is_seller = cat == "seller_disclosure";
            if target_seller != is_seller {
                continue;
            }
            for page in &doc.pages {
                for line in page.text.lines() {
                    let trimmed = line.trim();
                    let low = trimmed.to_ascii_lowercase();
                    if low.contains("lot size") || low.contains("lot area") || low.contains("parcel area") {
                        continue;
                    }
                    if let Some(caps) = CLAIMED_SQFT_RE.captures(trimmed) {
                        let digits: String = caps[1].chars().filter(|c| c.is_ascii_digit()).collect();
                        if let Ok(sqft) = digits.parse::<i64>() {
                            if (300..=25_000).contains(&sqft) {
                                return Some((
                                    sqft,
                                    DocPageCitation {
                                        doc_name: doc.name.clone(),
                                        page: page.page_number,
                                        quote: clean_excerpt(trimmed, 220),
                                    },
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Deterministic Whole-Packet Analyzer & Contradiction Matrix builder.
pub fn analyze_packet_documents_deterministic(
    docs: &[ExtractedDocument],
    report: Option<&Value>,
    pii_redacted_count: usize,
    override_multiplier: Option<f64>,
) -> Value {
    let (reg_mult, reg_label) = calibrate_regional_cost_multiplier(report, override_multiplier);

    let mut classified_docs: Vec<(&ExtractedDocument, &'static str, f64)> =
        Vec::with_capacity(docs.len());
    let mut doc_classifications_json = Vec::with_capacity(docs.len());

    for doc in docs {
        let (cat, conf) = classify_packet_document(doc);
        classified_docs.push((doc, cat, conf));
        doc_classifications_json.push(json!({
            "docName": doc.name,
            "category": cat,
            "confidence": conf,
            "pages": doc.pages.len(),
            "chars": doc.chars,
            "method": doc.method,
        }));
    }

    let doc_cat_pairs: Vec<(&ExtractedDocument, &'static str)> = classified_docs
        .iter()
        .map(|&(d, c, _)| (d, c))
        .collect();

    let seller_claims = extract_seller_claims(&doc_cat_pairs);

    // Extract specialist HOA & Prelim Title sub-summaries when included in the packet
    let hoa_docs: Vec<ExtractedDocument> = classified_docs
        .iter()
        .filter(|(_, cat, _)| *cat == "hoa_reserves")
        .map(|(d, _, _)| (*d).clone())
        .collect();
    let hoa_summary = if !hoa_docs.is_empty() {
        Some(parse_hoa_documents(&hoa_docs, report, 0))
    } else {
        None
    };

    let title_docs: Vec<&ExtractedDocument> = classified_docs
        .iter()
        .filter(|(_, cat, _)| *cat == "prelim_title")
        .map(|(d, _, _)| *d)
        .collect();
    let title_summary = if !title_docs.is_empty() {
        let joined_title = title_docs
            .iter()
            .map(|d| d.formatted_text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        Some(parse_title_report_text(&joined_title, report))
    } else {
        None
    };

    // Scan non-seller-disclosure documents for defects & liabilities
    let mut raw_findings: Vec<RawFinding> = Vec::new();
    for &(doc, cat, _) in &classified_docs {
        if cat == "seller_disclosure" {
            continue;
        }
        for page in &doc.pages {
            let non_empty: Vec<&str> = page
                .text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !(l.starts_with("[Page ") && l.ends_with(']')))
                .collect();

            let mut matched_keys_on_page: HashSet<&'static str> = HashSet::new();
            for (idx, &line) in non_empty.iter().enumerate() {
                let two_line = if idx + 1 < non_empty.len() {
                    format!("{} {}", line, non_empty[idx + 1])
                } else {
                    line.to_string()
                };
                for rule in DEFECT_RULES.iter() {
                    if matched_keys_on_page.contains(rule.dedup_key) {
                        continue;
                    }
                    let matched_text = if rule.re.is_match(line) {
                        Some(line)
                    } else if rule.re.is_match(&two_line) {
                        Some(two_line.as_str())
                    } else {
                        None
                    };
                    let Some(hit_txt) = matched_text else {
                        continue;
                    };
                    if rule.dedup_key == "roof_leak_or_covering"
                        && cat == "termite_wdo"
                        && DEFECT_RULES
                            .iter()
                            .find(|r| r.dedup_key == "wdo_section1_pest_or_rot")
                            .map(|r| r.re.is_match(hit_txt))
                            .unwrap_or(false)
                    {
                        // WDO Section 1 lines mentioning roof leaks as moisture source carry WDO
                        // dry-rot/pest pricing rather than a roofing replacement bid; Seller
                        // "no leaks" contradiction is handled via `wdo_section1_pest_or_rot`.
                        continue;
                    }
                    matched_keys_on_page.insert(rule.dedup_key);

                    let (cost_low, cost_high, explicit_bid) =
                        if let Some((lo, hi)) = extract_inline_cost_range(hit_txt) {
                            (lo, hi, true)
                        } else {
                            (
                                ((rule.base_low as f64) * reg_mult).round() as i64,
                                ((rule.base_high as f64) * reg_mult).round() as i64,
                                false,
                            )
                        };

                    raw_findings.push(RawFinding {
                        dedup_key: rule.dedup_key,
                        topic_key: rule.topic_key,
                        trade: rule.trade,
                        title: rule.title.to_string(),
                        detail: clean_excerpt(hit_txt, 220),
                        severity: rule.severity,
                        doc_category: cat,
                        citation: DocPageCitation {
                            doc_name: doc.name.clone(),
                            page: page.page_number,
                            quote: clean_excerpt(hit_txt, 220),
                        },
                        cost_low,
                        cost_high,
                        explicit_bid,
                        is_liability: false,
                    });
                }
            }
        }
    }

    // Add Prelim Title involuntary liens / Solar UCC-1 liabilities as findings
    if let Some(ref ts) = title_summary {
        let first_title_name = title_docs
            .first()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "Prelim_Title.pdf".to_string());
        if let Some(liens) = ts.get("involuntaryLiens").and_then(|v| v.as_array()) {
            for lien in liens {
                if lien.get("status").and_then(|s| s.as_str()) != Some("active") {
                    continue;
                }
                let ltype = lien
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Involuntary Lien");
                let amt = lien.get("amount").and_then(|v| v.as_i64()).unwrap_or(15_000);
                let excerpt = lien
                    .get("excerpt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Active involuntary lien on Schedule B");
                raw_findings.push(RawFinding {
                    dedup_key: "title_involuntary_lien_cure",
                    topic_key: "liens_solar",
                    trade: "Title / Lien Cure",
                    title: format!("Active Schedule B Lien Cure: {ltype}"),
                    detail: clean_excerpt(excerpt, 220),
                    severity: "Critical",
                    doc_category: "prelim_title",
                    citation: DocPageCitation {
                        doc_name: first_title_name.clone(),
                        page: 1,
                        quote: clean_excerpt(excerpt, 220),
                    },
                    cost_low: amt,
                    cost_high: amt,
                    explicit_bid: true,
                    is_liability: true,
                });
            }
        }
        if let Some(excs) = ts.get("easementsAndExceptions").and_then(|v| v.as_array()) {
            for exc in excs {
                let etype = exc.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if etype.to_ascii_lowercase().contains("solar")
                    || etype.to_ascii_lowercase().contains("ucc")
                {
                    let excerpt = exc
                        .get("excerpt")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Solar UCC-1 fixture filing on Schedule B");
                    raw_findings.push(RawFinding {
                        dedup_key: "title_solar_ucc1_cure",
                        topic_key: "liens_solar",
                        trade: "Title / Lien Cure",
                        title: "Solar Lease / UCC-1 Financing Statement Payoff or Assumption".to_string(),
                        detail: clean_excerpt(excerpt, 220),
                        severity: "Major",
                        doc_category: "prelim_title",
                        citation: DocPageCitation {
                            doc_name: first_title_name.clone(),
                            page: 1,
                            quote: clean_excerpt(excerpt, 220),
                        },
                        cost_low: 0,
                        cost_high: 18_000,
                        explicit_bid: false,
                        is_liability: true,
                    });
                }
            }
        }
    }

    // Add HOA special assessment liabilities from uploaded HOA packet docs
    if let Some(ref hs) = hoa_summary {
        if let Some(assessments) = hs
            .pointer("/financialHealth/specialAssessments")
            .and_then(|v| v.as_array())
        {
            for sa in assessments {
                let purpose = sa
                    .get("purpose")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HOA Special Assessment");
                let vote = sa
                    .get("boardVoteStatus")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Active/Proposed");
                let amt = sa
                    .get("perUnitAmountUsd")
                    .and_then(|v| v.as_i64())
                    .or_else(|| sa.get("amountUsd").and_then(|v| v.as_i64()))
                    .unwrap_or(8_000);
                let cit: DocPageCitation = sa
                    .get("citation")
                    .and_then(|c| serde_json::from_value(c.clone()).ok())
                    .unwrap_or(DocPageCitation {
                        doc_name: "HOA_Packet.pdf".to_string(),
                        page: 1,
                        quote: purpose.to_string(),
                    });
                raw_findings.push(RawFinding {
                    dedup_key: "hoa_special_assessment_liability",
                    topic_key: "hoa_assessments",
                    trade: "Title / Lien Cure",
                    title: format!("HOA Special Assessment ({vote}): {purpose}"),
                    detail: cit.quote.clone(),
                    severity: "Critical",
                    doc_category: "hoa_reserves",
                    citation: cit,
                    cost_low: amt,
                    cost_high: amt,
                    explicit_bid: true,
                    is_liability: true,
                });
            }
        }
    }

    // Add public-record Title Liens / Solar UCC-1 filings from `report.json` when present
    if let Some(rep) = report {
        if let Some(liens) = rep
            .pointer("/facts/titleAndLiens/involuntaryLiens")
            .and_then(|v| v.as_array())
        {
            for lien in liens {
                let status = lien
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("active");
                if status.eq_ignore_ascii_case("released")
                    || status.eq_ignore_ascii_case("resolved")
                    || status.eq_ignore_ascii_case("reconveyed")
                {
                    continue;
                }
                let ltype = lien
                    .get("type")
                    .and_then(|v| v.as_str())
                    .or_else(|| lien.as_str())
                    .unwrap_or("Public Record Involuntary Lien");
                let amt = lien.get("amount").and_then(|v| v.as_i64()).unwrap_or(15_000);
                let excerpt = lien
                    .get("excerpt")
                    .or_else(|| lien.get("note"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(ltype);
                raw_findings.push(RawFinding {
                    dedup_key: "title_involuntary_lien_cure",
                    topic_key: "liens_solar",
                    trade: "Title / Lien Cure",
                    title: format!("Active Schedule B / Public Record Lien Cure: {ltype}"),
                    detail: clean_excerpt(excerpt, 220),
                    severity: "Critical",
                    doc_category: "prelim_title",
                    citation: DocPageCitation {
                        doc_name: "Public Record — County Recorder / Title & Liens (report.json)".to_string(),
                        page: 1,
                        quote: clean_excerpt(excerpt, 220),
                    },
                    cost_low: amt,
                    cost_high: amt,
                    explicit_bid: true,
                    is_liability: true,
                });
            }
        }
        let mut pub_solar_excerpts: Vec<String> = Vec::new();
        if let Some(sols) = rep
            .pointer("/facts/titleAndLiens/solarUccFilings")
            .and_then(|v| v.as_array())
        {
            for s in sols {
                let txt = s
                    .get("excerpt")
                    .or_else(|| s.get("type"))
                    .or_else(|| s.get("creditor"))
                    .and_then(|v| v.as_str())
                    .or_else(|| s.as_str())
                    .unwrap_or("Recorded Solar UCC-1 financing statement");
                pub_solar_excerpts.push(txt.to_string());
            }
        }
        if let Some(excs) = rep
            .pointer("/facts/titleAndLiens/easementsAndRestrictions")
            .and_then(|v| v.as_array())
        {
            for e in excs {
                let txt = e
                    .get("type")
                    .or_else(|| e.get("excerpt"))
                    .and_then(|v| v.as_str())
                    .or_else(|| e.as_str())
                    .unwrap_or("");
                let low = txt.to_ascii_lowercase();
                if low.contains("solar") || low.contains("ucc") {
                    pub_solar_excerpts.push(txt.to_string());
                }
            }
        }
        for txt in pub_solar_excerpts {
            raw_findings.push(RawFinding {
                dedup_key: "title_solar_ucc1_cure",
                topic_key: "liens_solar",
                trade: "Title / Lien Cure",
                title: "Solar Lease / UCC-1 Financing Statement Payoff or Assumption".to_string(),
                detail: clean_excerpt(&txt, 220),
                severity: "Major",
                doc_category: "prelim_title",
                citation: DocPageCitation {
                    doc_name: "Public Record — County Recorder / Title & Liens (report.json)".to_string(),
                    page: 1,
                    quote: clean_excerpt(&txt, 220),
                },
                cost_low: 0,
                cost_high: 18_000,
                explicit_bid: false,
                is_liability: true,
            });
        }
    }

    // ---------------- Cross-Document & Public-Record Contradiction Matrix ----------------
    let mut contradictions: Vec<Value> = Vec::new();
    let mut next_contra_id = 1usize;

    // A. Cross-document contradictions: Seller Disclosure vs Specialist/Inspection/HOA/Title PDFs
    for claim in &seller_claims {
        for f in &raw_findings {
            let matches_claim = claim.topic_key == f.topic_key
                || (claim.topic_key == "leaks_moisture"
                    && f.dedup_key == "wdo_section1_pest_or_rot"
                    && (f.citation.quote.to_ascii_lowercase().contains("moisture")
                        || f.citation.quote.to_ascii_lowercase().contains("leak")
                        || f.citation.quote.to_ascii_lowercase().contains("water")));
            if !matches_claim {
                continue;
            }
            let already = contradictions.iter().any(|c| {
                c.get("topicKey").and_then(|v| v.as_str()) == Some(claim.topic_key)
                    && c.get("contradictingDoc").and_then(|v| v.as_str())
                        == Some(f.citation.doc_name.as_str())
            });
            if already {
                continue;
            }
            let cid = format!("contra-{next_contra_id}");
            next_contra_id += 1;
            contradictions.push(json!({
                "id": cid,
                "kind": "cross_document",
                "topicKey": claim.topic_key,
                "topic": claim.topic_label,
                "severity": f.severity,
                "sourceDoc": claim.doc_name,
                "sourcePage": claim.page,
                "quote": claim.quote,
                "contradictingDoc": f.citation.doc_name,
                "contradictingPage": f.citation.page,
                "contradictingQuote": f.citation.quote,
                "source": {
                    "doc": claim.doc_name,
                    "page": claim.page,
                    "sourceDoc": claim.doc_name,
                    "sourcePage": claim.page,
                    "quote": claim.quote,
                },
                "contradicting": {
                    "doc": f.citation.doc_name,
                    "page": f.citation.page,
                    "contradictingDoc": f.citation.doc_name,
                    "contradictingPage": f.citation.page,
                    "quote": f.citation.quote,
                },
                "analysis": format!(
                    "Seller Disclosure ({}, p.{}) represents \"{}\", directly contradicted by {} ({}, p.{}) reporting: \"{}\".",
                    claim.doc_name,
                    claim.page,
                    claim.quote,
                    f.doc_category,
                    f.citation.doc_name,
                    f.citation.page,
                    f.citation.quote
                ),
            }));
        }
    }

    // B. Public-Record Contradictions against `report` JSON
    if let Some(rep) = report {
        // B1. Assessor Sq Ft vs Disclosed / Listing / Footprint Sq Ft & Missing Permits
        let assessor_sqft = rep
            .pointer("/facts/sqft")
            .and_then(|v| v.as_f64())
            .map(|v| v.round() as i64);
        let stories = rep
            .pointer("/facts/stories")
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        let fp_one_story_sqft = if stories <= 1 {
            rep.pointer("/facts/buildingRegister/footprintSqft")
                .and_then(|v| v.as_f64())
        } else {
            None
        };
        let listing_or_fp_sqft = rep
            .pointer("/facts/listingSqft")
            .or_else(|| rep.pointer("/facts/marketedSqft"))
            .or_else(|| rep.pointer("/facts/mlsListing/sqft"))
            .or_else(|| rep.pointer("/facts/footprintSqft"))
            .and_then(|v| v.as_f64())
            .or(fp_one_story_sqft)
            .map(|v| v.round() as i64);
        let packet_sqft = find_claimed_sqft_in_packet(&doc_cat_pairs);
        let permit_notes = rep
            .pointer("/facts/permitNotes")
            .or_else(|| rep.pointer("/facts/buildingRegister/permitNotes"))
            .or_else(|| rep.pointer("/facts/disclosures/permitNotes"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let sqft_note = rep
            .pointer("/facts/sqftNote")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if let Some(a_sqft) = assessor_sqft {
            let candidate_claimed: Option<(i64, String, u32, String)> =
                if let Some((p_sqft, p_cit)) = packet_sqft {
                    Some((p_sqft, p_cit.doc_name, p_cit.page, p_cit.quote))
                } else if let Some(l_sqft) = listing_or_fp_sqft {
                    let s_claim = seller_claims
                        .iter()
                        .find(|c| c.topic_key == "unpermitted_additions");
                    Some((
                        l_sqft,
                        s_claim
                            .map(|c| c.doc_name.clone())
                            .unwrap_or_else(|| "Listing / Disclosure Packet".to_string()),
                        s_claim.map(|c| c.page).unwrap_or(1),
                        s_claim
                            .map(|c| c.quote.clone())
                            .unwrap_or_else(|| format!("Marketed / footprint area: {l_sqft} sq ft")),
                    ))
                } else {
                    None
                };

            if let Some((c_sqft, src_doc, src_page, src_quote)) = candidate_claimed {
                let diff = (c_sqft - a_sqft).abs();
                let pct_diff = if a_sqft > 0 {
                    (diff as f64 / a_sqft as f64) * 100.0
                } else {
                    0.0
                };
                if diff >= 120 && pct_diff >= 6.0 {
                    let pub_quote = format!(
                        "County Assessor tax roll records {} sq ft living area vs. {} sq ft claimed ({} sq ft / {:.1}% discrepancy).{}",
                        a_sqft,
                        c_sqft,
                        diff,
                        pct_diff,
                        if !permit_notes.is_empty() {
                            format!(" Permit record: {}", clean_excerpt(permit_notes, 120))
                        } else if !sqft_note.is_empty() {
                            format!(" Note: {}", clean_excerpt(sqft_note, 120))
                        } else {
                            " Verify finaled building permits for potential unpermitted square footage.".to_string()
                        }
                    );
                    let cid = format!("contra-{next_contra_id}");
                    next_contra_id += 1;
                    contradictions.push(json!({
                        "id": cid,
                        "kind": "public_record",
                        "topicKey": "unpermitted_additions",
                        "topic": "Assessor Square Footage & Unpermitted Addition Discrepancy",
                        "severity": if diff >= 250 { "Critical" } else { "Major" },
                        "sourceDoc": src_doc,
                        "sourcePage": src_page,
                        "quote": src_quote,
                        "contradictingDoc": "Public Record — County Assessor & Permit Roll (report.json)",
                        "contradictingPage": 1,
                        "contradictingQuote": pub_quote,
                        "source": {
                            "doc": src_doc,
                            "page": src_page,
                            "sourceDoc": src_doc,
                            "sourcePage": src_page,
                            "quote": src_quote,
                        },
                        "contradicting": {
                            "doc": "Public Record — County Assessor & Permit Roll (report.json)",
                            "page": 1,
                            "contradictingDoc": "Public Record — County Assessor & Permit Roll (report.json)",
                            "contradictingPage": 1,
                            "quote": pub_quote,
                        },
                        "analysis": format!(
                            "Disclosed/marketed square footage ({} sq ft) exceeds County Assessor living area ({} sq ft) by {} sq ft ({:.1}%) — classic indicator of an unpermitted room addition, sunroom, or garage conversion that lenders/appraisers may exclude from GLA.",
                            c_sqft, a_sqft, diff, pct_diff
                        ),
                    }));
                }
            }
        }

        // B2. Missing Building Permits in `permitNotes` vs Seller "No unpermitted additions"
        let permit_low = permit_notes.to_ascii_lowercase();
        if (permit_low.contains("unpermitted")
            || permit_low.contains("no permit")
            || permit_low.contains("without permit")
            || permit_low.contains("expired permit"))
            && !contradictions
                .iter()
                .any(|c| c.get("topicKey").and_then(|v| v.as_str()) == Some("unpermitted_additions") && c.get("kind").and_then(|v| v.as_str()) == Some("public_record"))
        {
            if let Some(claim) = seller_claims
                .iter()
                .find(|c| c.topic_key == "unpermitted_additions")
            {
                let pub_quote = clean_excerpt(permit_notes, 220);
                let cid = format!("contra-{next_contra_id}");
                next_contra_id += 1;
                contradictions.push(json!({
                    "id": cid,
                    "kind": "public_record",
                    "topicKey": "unpermitted_additions",
                    "topic": "Municipal Permit History vs. Seller Permit Representation",
                    "severity": "Major",
                    "sourceDoc": claim.doc_name,
                    "sourcePage": claim.page,
                    "quote": claim.quote,
                    "contradictingDoc": "Public Record — Municipal Building Permits (report.json)",
                    "contradictingPage": 1,
                    "contradictingQuote": pub_quote,
                    "source": {
                        "doc": claim.doc_name,
                        "page": claim.page,
                        "sourceDoc": claim.doc_name,
                        "sourcePage": claim.page,
                        "quote": claim.quote,
                    },
                    "contradicting": {
                        "doc": "Public Record — Municipal Building Permits (report.json)",
                        "page": 1,
                        "contradictingDoc": "Public Record — Municipal Building Permits (report.json)",
                        "contradictingPage": 1,
                        "quote": pub_quote,
                    },
                    "analysis": "Public permit notes flag missing/unpermitted work despite Seller Disclosure stating no unpermitted additions/modifications.",
                }));
            }
        }

        // B3. FEMA SFHA Flood Zone vs Seller Drainage/Flooding Claim
        let flood_zone = rep
            .pointer("/modules/hazards/data/flood/zone")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let in_sfha = rep
            .pointer("/modules/hazards/data/flood/sfha")
            .or_else(|| rep.pointer("/modules/hazards/data/flood/inSfha"))
            .or_else(|| rep.pointer("/modules/hazards/data/flood/insuranceRequired"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| {
                let z = flood_zone.trim().to_ascii_uppercase();
                z.starts_with('A') || z.starts_with('V')
            });
        if in_sfha {
            if let Some(claim) = seller_claims
                .iter()
                .find(|c| c.topic_key == "drainage_settling")
            {
                let pub_quote = format!(
                    "FEMA NFHL maps subject parcel inside Special Flood Hazard Area (Zone {flood_zone}) — mandatory flood insurance applies."
                );
                let cid = format!("contra-{next_contra_id}");
                next_contra_id += 1;
                contradictions.push(json!({
                    "id": cid,
                    "kind": "public_record",
                    "topicKey": "drainage_settling",
                    "topic": "FEMA Special Flood Hazard Area (SFHA) vs. Disclosure",
                    "severity": "Critical",
                    "sourceDoc": claim.doc_name,
                    "sourcePage": claim.page,
                    "quote": claim.quote,
                    "contradictingDoc": "Public Record — FEMA NFHL Flood Layer (report.json)",
                    "contradictingPage": 1,
                    "contradictingQuote": pub_quote,
                    "source": {
                        "doc": claim.doc_name,
                        "page": claim.page,
                        "sourceDoc": claim.doc_name,
                        "sourcePage": claim.page,
                        "quote": claim.quote,
                    },
                    "contradicting": {
                        "doc": "Public Record — FEMA NFHL Flood Layer (report.json)",
                        "page": 1,
                        "contradictingDoc": "Public Record — FEMA NFHL Flood Layer (report.json)",
                        "contradictingPage": 1,
                        "quote": pub_quote,
                    },
                    "analysis": "Parcel lies in a FEMA 100-year Special Flood Hazard Area requiring lender-mandated flood insurance.",
                }));
            }
        }

        // B4. CAL FIRE / CGS Liquefaction, Fault, or Landslide Zones vs Seller Soil/Settling Claim
        let in_liquefaction = rep
            .pointer("/modules/hazards/data/earthquake/california/liquefaction/inZone")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || rep
                .pointer("/modules/hazards/data/caNaturalHazardDisclosure/seismicHazardZone")
                .and_then(|v| v.as_str())
                == Some("yes");
        let in_ap_fault = rep
            .pointer("/modules/hazards/data/earthquake/california/alquistPriolo/inZone")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || rep
                .pointer("/modules/hazards/data/caNaturalHazardDisclosure/earthquakeFaultZone")
                .and_then(|v| v.as_str())
                == Some("yes");
        let in_landslide = rep
            .pointer("/modules/hazards/data/earthquake/california/landslide/inZone")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let in_fhsz = rep
            .pointer("/modules/hazards/data/wildfire/california/inFhsz")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || rep
                .pointer("/modules/hazards/data/caNaturalHazardDisclosure/veryHighFireHazardZone")
                .and_then(|v| v.as_str())
                == Some("yes");
        if in_liquefaction || in_ap_fault || in_landslide || in_fhsz {
            if let Some(claim) = seller_claims
                .iter()
                .find(|c| c.topic_key == "drainage_settling" || c.topic_key == "env_contamination")
            {
                let mut zones = Vec::new();
                if in_ap_fault {
                    zones.push("Alquist-Priolo Earthquake Fault Zone");
                }
                if in_liquefaction {
                    zones.push("CGS Seismic Liquefaction Zone");
                }
                if in_landslide {
                    zones.push("CGS Earthquake-Induced Landslide Zone");
                }
                if in_fhsz {
                    zones.push("CAL FIRE Fire Hazard Severity Zone");
                }
                let pub_quote = format!(
                    "California statutory hazard mapping places parcel in: {}.",
                    zones.join(", ")
                );
                let cid = format!("contra-{next_contra_id}");
                next_contra_id += 1;
                contradictions.push(json!({
                    "id": cid,
                    "kind": "public_record",
                    "topicKey": "statutory_hazard_zones",
                    "topic": "Statutory Seismic / Wildfire Hazard Zone vs. Seller Soil/Hazard Representation",
                    "severity": "Major",
                    "sourceDoc": claim.doc_name,
                    "sourcePage": claim.page,
                    "quote": claim.quote,
                    "contradictingDoc": "Public Record — CAL FIRE / CGS Statutory Hazard Maps (report.json)",
                    "contradictingPage": 1,
                    "contradictingQuote": pub_quote,
                    "source": {
                        "doc": claim.doc_name,
                        "page": claim.page,
                        "sourceDoc": claim.doc_name,
                        "sourcePage": claim.page,
                        "quote": claim.quote,
                    },
                    "contradicting": {
                        "doc": "Public Record — CAL FIRE / CGS Statutory Hazard Maps (report.json)",
                        "page": 1,
                        "contradictingDoc": "Public Record — CAL FIRE / CGS Statutory Hazard Maps (report.json)",
                        "contradictingPage": 1,
                        "quote": pub_quote,
                    },
                    "analysis": "State hazard maps trigger mandatory CA Natural Hazard Disclosure (NHD) & AB 38 defensible-space / foundation seismic considerations.",
                }));
            }
        }

        // B5. EPA / GeoTracker Proximity & Transportation Noise vs Seller Noise/Nuisance or Env Claim
        let noise_score = rep
            .pointer("/modules/environment/data/scores/noise")
            .and_then(|v| v.as_f64());
        let road_dba = rep
            .pointer("/modules/environment/data/noise/roadDb")
            .or_else(|| rep.pointer("/modules/environment/data/noise/estLdnDb"))
            .or_else(|| rep.pointer("/modules/environment/data/noise/atProperty/all/db"))
            .or_else(|| rep.pointer("/modules/environment/data/noise/roadDba"))
            .or_else(|| rep.pointer("/modules/environment/data/noise/maxDba"))
            .and_then(|v| v.as_f64());
        let nearest_env_site = rep
            .pointer("/modules/environment/data/sites/nearest/0")
            .and_then(|v| v.as_object());
        let env_dist_m = nearest_env_site
            .and_then(|o| o.get("distanceM"))
            .and_then(|v| v.as_f64())
            .or_else(|| {
                nearest_env_site
                    .and_then(|o| o.get("distanceMi"))
                    .and_then(|v| v.as_f64())
                    .map(|mi| mi * 1609.34)
            });
        let env_close = env_dist_m.map(|m| m <= 400.0).unwrap_or(false);

        if let Some(claim) = seller_claims
            .iter()
            .find(|c| {
                (c.topic_key == "noise_nuisances"
                    && (noise_score.map(|s| s < 55.0).unwrap_or(false)
                        || road_dba.map(|d| d >= 65.0).unwrap_or(false)
                        || env_close))
                    || (c.topic_key == "env_contamination" && env_close)
            })
        {
            let pub_quote = if env_close {
                let sname = nearest_env_site
                    .and_then(|o| o.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Regulated Environmental Cleanup Site");
                let dist_m = env_dist_m.unwrap_or(0.0);
                format!("EPA / GeoTracker regulated site \"{sname}\" mapped {:.0}m from parcel.", dist_m)
            } else {
                format!(
                    "USDOT / environmental noise model indicates elevated corridor noise ({}).",
                    road_dba
                        .map(|d| format!("{d:.1} dBA"))
                        .or_else(|| noise_score.map(|s| format!("noise safety score {s:.0}/100")))
                        .unwrap_or_else(|| "elevated dBA".to_string())
                )
            };
            let cid = format!("contra-{next_contra_id}");
            next_contra_id += 1;
            contradictions.push(json!({
                "id": cid,
                "kind": "public_record",
                "topicKey": claim.topic_key,
                "topic": "Environmental / Corridor Noise & Regulated Site Proximity",
                "severity": "Moderate",
                "sourceDoc": claim.doc_name,
                "sourcePage": claim.page,
                "quote": claim.quote,
                "contradictingDoc": "Public Record — EPA / GeoTracker / USDOT Noise Layer (report.json)",
                "contradictingPage": 1,
                "contradictingQuote": pub_quote,
                "source": {
                    "doc": claim.doc_name,
                    "page": claim.page,
                    "sourceDoc": claim.doc_name,
                    "sourcePage": claim.page,
                    "quote": claim.quote,
                },
                "contradicting": {
                    "doc": "Public Record — EPA / GeoTracker / USDOT Noise Layer (report.json)",
                    "page": 1,
                    "contradictingDoc": "Public Record — EPA / GeoTracker / USDOT Noise Layer (report.json)",
                    "contradictingPage": 1,
                    "quote": pub_quote,
                },
                "analysis": "Public environmental/noise records flag nearby corridor noise or regulated site activity despite Seller Disclosure checking \"No\" on neighborhood noise/nuisances or environmental hazards.",
            }));
        }

        // B6. Public-Record Liens & Solar UCC-1 Filings (`facts/titleAndLiens`) vs Seller Claim
        let has_pub_lien_or_solar = raw_findings.iter().any(|f| {
            f.topic_key == "liens_solar"
                && f.citation.doc_name.starts_with("Public Record")
        });
        if has_pub_lien_or_solar {
            if let Some(claim) = seller_claims
                .iter()
                .find(|c| c.topic_key == "liens_solar")
            {
                let already_pub_lien = contradictions.iter().any(|c| {
                    c.get("topicKey").and_then(|v| v.as_str()) == Some("liens_solar")
                        && c.get("kind").and_then(|v| v.as_str()) == Some("public_record")
                });
                if !already_pub_lien {
                    let pub_summaries: Vec<String> = raw_findings
                        .iter()
                        .filter(|f| {
                            f.topic_key == "liens_solar"
                                && f.citation.doc_name.starts_with("Public Record")
                        })
                        .map(|f| format!("{}: {}", f.title, f.citation.quote))
                        .collect();
                    let pub_quote = clean_excerpt(&pub_summaries.join(" | "), 220);
                    let cid = format!("contra-{next_contra_id}");
                    next_contra_id += 1;
                    let _ = next_contra_id;
                    contradictions.push(json!({
                        "id": cid,
                        "kind": "public_record",
                        "topicKey": "liens_solar",
                        "topic": "Recorded Liens / Solar UCC-1 Encumbrances vs. Seller Disclosure",
                        "severity": "Critical",
                        "sourceDoc": claim.doc_name,
                        "sourcePage": claim.page,
                        "quote": claim.quote,
                        "contradictingDoc": "Public Record — County Recorder / Title & Liens (report.json)",
                        "contradictingPage": 1,
                        "contradictingQuote": pub_quote,
                        "source": {
                            "doc": claim.doc_name,
                            "page": claim.page,
                            "sourceDoc": claim.doc_name,
                            "sourcePage": claim.page,
                            "quote": claim.quote,
                        },
                        "contradicting": {
                            "doc": "Public Record — County Recorder / Title & Liens (report.json)",
                            "page": 1,
                            "contradictingDoc": "Public Record — County Recorder / Title & Liens (report.json)",
                            "contradictingPage": 1,
                            "quote": pub_quote,
                        },
                        "analysis": "Public title/lien records show an active involuntary lien or recorded solar UCC-1 financing statement despite Seller Disclosure stating no liens or leased-solar encumbrances.",
                    }));
                }
            }
        }
    }

    // ---------------- Canonical Defect Deduplication (Zero Double-Counting!) ----------------
    let raw_finding_count = raw_findings.len();
    let raw_sum_mid_usd: i64 = raw_findings
        .iter()
        .map(|f| (f.cost_low + f.cost_high) / 2)
        .sum();

    fn is_matching_trade_specialist(dedup_key: &str, doc_category: &str) -> bool {
        matches!(
            (dedup_key, doc_category),
            ("roof_leak_or_covering", "roof")
                | ("wdo_section1_pest_or_rot", "termite_wdo")
                | ("sewer_lateral_defect", "sewer")
                | ("interior_plumbing_leak_moisture", "sewer")
                | ("foundation_settlement_or_drainage", "foundation")
                | ("unpermitted_addition_or_conversion", "permits")
                | ("title_involuntary_lien_cure", "prelim_title")
                | ("title_solar_ucc1_cure", "prelim_title")
                | ("hoa_special_assessment_liability", "hoa_reserves")
        )
    }

    #[derive(Clone)]
    struct MergedDefect {
        id: String,
        dedup_key: &'static str,
        trade: &'static str,
        title: String,
        detail: String,
        severity: &'static str,
        cost_low: i64,
        cost_high: i64,
        cost_authority_rank: u8,
        is_liability: bool,
        merged_count: usize,
        citations: Vec<DocPageCitation>,
    }

    let mut merged_map: BTreeMap<&'static str, MergedDefect> = BTreeMap::new();
    for rf in raw_findings {
        let matching_specialist = is_matching_trade_specialist(rf.dedup_key, rf.doc_category);
        let authority_rank: u8 = match (matching_specialist, rf.explicit_bid) {
            (true, true) => 3,
            (false, true) => 2,
            (true, false) => 1,
            (false, false) => 0,
        };

        if let Some(existing) = merged_map.get_mut(rf.dedup_key) {
            existing.merged_count += 1;
            if severity_rank(rf.severity) > severity_rank(existing.severity) {
                existing.severity = rf.severity;
            }
            // Do NOT sum `existing.cost + rf.cost`! Prefer higher-authority trade-specialist bid
            // (e.g., licensed Roof Inspection quote supersedes Home Inspection estimate); within
            // the same authority tier, take the conservative envelope (`min(low)..max(high)`).
            if authority_rank > existing.cost_authority_rank {
                existing.cost_low = rf.cost_low;
                existing.cost_high = rf.cost_high;
                existing.cost_authority_rank = authority_rank;
            } else if authority_rank == existing.cost_authority_rank {
                existing.cost_low = existing.cost_low.min(rf.cost_low);
                existing.cost_high = existing.cost_high.max(rf.cost_high);
            }
            let dup_cit = existing.citations.iter().any(|c| {
                c.doc_name == rf.citation.doc_name
                    && c.page == rf.citation.page
                    && c.quote == rf.citation.quote
            });
            if !dup_cit {
                existing.citations.push(rf.citation);
            }
        } else {
            merged_map.insert(
                rf.dedup_key,
                MergedDefect {
                    id: format!("defect-{}", rf.dedup_key.replace('_', "-")),
                    dedup_key: rf.dedup_key,
                    trade: rf.trade,
                    title: rf.title,
                    detail: rf.detail,
                    severity: rf.severity,
                    cost_low: rf.cost_low,
                    cost_high: rf.cost_high,
                    cost_authority_rank: authority_rank,
                    is_liability: rf.is_liability,
                    merged_count: 1,
                    citations: vec![rf.citation],
                },
            );
        }
    }

    let mut merged_list: Vec<MergedDefect> = merged_map.into_values().collect();
    merged_list.sort_by(|a, b| {
        severity_rank(b.severity)
            .cmp(&severity_rank(a.severity))
            .then_with(|| ((b.cost_low + b.cost_high) / 2).cmp(&((a.cost_low + a.cost_high) / 2)))
    });

    let mut total_low: i64 = 0;
    let mut total_mid: i64 = 0;
    let mut total_high: i64 = 0;
    let mut repair_mid: i64 = 0;
    let mut liability_mid: i64 = 0;

    let deduplicated_defects_json: Vec<Value> = merged_list
        .iter()
        .map(|d| {
            let mid = (d.cost_low + d.cost_high) / 2;
            total_low += d.cost_low;
            total_mid += mid;
            total_high += d.cost_high;
            if d.is_liability {
                liability_mid += mid;
            } else {
                repair_mid += mid;
            }
            let primary_cit = d.citations.first().cloned().unwrap_or(DocPageCitation {
                doc_name: "Packet.pdf".to_string(),
                page: 1,
                quote: d.detail.clone(),
            });
            json!({
                "id": d.id,
                "dedupKey": d.dedup_key,
                "trade": d.trade,
                "severity": d.severity,
                "title": d.title,
                "detail": d.detail,
                "costLow": d.cost_low,
                "costMid": mid,
                "costHigh": d.cost_high,
                "isLiability": d.is_liability,
                "mergedCount": d.merged_count,
                "primaryCitation": primary_cit,
                "citations": d.citations,
            })
        })
        .collect();

    let duplicates_merged = raw_finding_count.saturating_sub(merged_list.len());
    let avoided_double_count_usd = raw_sum_mid_usd.saturating_sub(total_mid);

    // ---------------- Unified Deal-Breaker Triage (Critical / Major / Moderate / Informational) ----------------
    let mut triage_items: Vec<Value> = Vec::new();
    for c in &contradictions {
        triage_items.push(json!({
            "tier": c.get("severity").and_then(|v| v.as_str()).unwrap_or("Major"),
            "type": "contradiction",
            "title": format!("Contradiction — {}", c.get("topic").and_then(|v| v.as_str()).unwrap_or("Disclosure Conflict")),
            "summary": c.get("analysis").and_then(|v| v.as_str()).unwrap_or(""),
            "sourceDoc": c.get("sourceDoc"),
            "sourcePage": c.get("sourcePage"),
            "contradictingDoc": c.get("contradictingDoc"),
            "contradictingPage": c.get("contradictingPage"),
        }));
    }
    for d in &merged_list {
        let first_cit = d.citations.first();
        triage_items.push(json!({
            "tier": d.severity,
            "type": if d.is_liability { "title_or_hoa_liability" } else { "physical_defect" },
            "title": d.title,
            "trade": d.trade,
            "exposureMidUsd": (d.cost_low + d.cost_high) / 2,
            "mergedSources": d.merged_count,
            "docName": first_cit.map(|c| c.doc_name.as_str()),
            "page": first_cit.map(|c| c.page),
            "quote": first_cit.map(|c| c.quote.as_str()),
        }));
    }

    let count_tier = |t: &str| -> usize {
        triage_items
            .iter()
            .filter(|i| i.get("tier").and_then(|v| v.as_str()) == Some(t))
            .count()
    };
    let crit_count = count_tier("Critical");
    let maj_count = count_tier("Major");
    let mod_count = count_tier("Moderate");
    let info_count = count_tier("Informational");

    let overall_triage_level = if crit_count > 0 {
        "Critical"
    } else if maj_count > 0 {
        "Major"
    } else if mod_count > 0 {
        "Moderate"
    } else {
        "Informational"
    };

    // ---------------- Top Questions for Listing Agent / Seller ----------------
    let mut top_questions: Vec<String> = Vec::new();
    for c in &contradictions {
        let topic = c.get("topic").and_then(|v| v.as_str()).unwrap_or("disclosure item");
        let sdoc = c.get("sourceDoc").and_then(|v| v.as_str()).unwrap_or("Seller Disclosure");
        let spg = c.get("sourcePage").and_then(|v| v.as_u64()).unwrap_or(1);
        let cdoc = c.get("contradictingDoc").and_then(|v| v.as_str()).unwrap_or("inspection report");
        top_questions.push(format!(
            "Regarding {topic}: {sdoc} (p.{spg}) marks \"No\" / differs from {cdoc} — can Seller clarify prior repair history, provide invoices/permits, or agree to a closing repair credit?"
        ));
        if top_questions.len() >= 5 {
            break;
        }
    }
    if merged_list.iter().any(|d| d.dedup_key == "wdo_section1_pest_or_rot") {
        top_questions.push("Will Seller provide a Section 1 WDO clearance or credit the full licensed pest-control & dry-rot repair estimate at close of escrow?".to_string());
    }
    if top_questions.is_empty() {
        top_questions.push("Have any repairs, insurance claims, or contractor bids been obtained since the inspection reports in this disclosure packet were prepared?".to_string());
    }

    let headline = format!(
        "Reviewed {}-document packet: {} contradiction(s), {} deduplicated defect/liability item(s) (${}–${} estimated exposure; {} Critical).",
        docs.len(),
        contradictions.len(),
        merged_list.len(),
        total_low,
        total_high,
        crit_count
    );

    let mut out_val = json!({
        "headline": headline,
        "piiRedactedCount": pii_redacted_count,
        "packetDocumentCount": docs.len(),
        "documentClassifications": doc_classifications_json,
        "contradictions": contradictions,
        "dealBreakerTriage": {
            "overallLevel": overall_triage_level,
            "counts": {
                "Critical": crit_count,
                "Major": maj_count,
                "Moderate": mod_count,
                "Informational": info_count,
            },
            "items": triage_items,
        },
        "deduplicatedDefects": deduplicated_defects_json,
        "totalExposureUsd": {
            "low": total_low,
            "mid": total_mid,
            "high": total_high,
            "physicalRepairMidUsd": repair_mid,
            "hoaAndLienLiabilityMidUsd": liability_mid,
            "regionalMultiplier": reg_mult,
            "regionalLabel": reg_label,
            "deduplicationSummary": {
                "rawFindingCount": raw_finding_count,
                "deduplicatedDefectCount": merged_list.len(),
                "duplicatesMergedCount": duplicates_merged,
                "rawSumMidUsd": raw_sum_mid_usd,
                "deduplicatedMidUsd": total_mid,
                "avoidedDoubleCountUsd": avoided_double_count_usd,
            }
        },
        "topQuestionsForSeller": top_questions,
        "hoaSummary": hoa_summary,
        "titleSummary": title_summary,
        "disclaimer": "Factual cross-document disclosure & public-record comparison only — NOT legal advice, a structural engineering opinion, or a binding contractor bid. Overlapping defects cited in multiple reports are deduplicated by canonical system/trade key to prevent double-counting."
    });
    crate::modules::move_in_cost::enrich_packet_with_move_in_costs(
        &mut out_val,
        report.unwrap_or(&Value::Null),
    );
    out_val
}

pub const PACKET_REVIEW_INSTRUCTIONS: &str = r#"You are Open Estate Senior Multi-Document Disclosure Packet & Contradiction Analyst.
You synthesize findings across a buyer's entire residential disclosure packet (Seller TDS/SPQ, Home Inspection, Termite/WDO, Roof, Sewer, Foundation, Prelim Title, HOA Reserves, NHD, Permits) cross-checked against public property records.

STRICT RULES:
1. EXACT CITATIONS & ZERO FABRICATION: Only reference documents, page numbers (`[Page N]`), and quotes actually present in DETERMINISTIC_PACKET_ANALYSIS or <untrusted_document_text>.
2. ZERO DOUBLE-COUNTING: Never double-count the same physical defect (such as a roof leak cited in both the Home Inspection and Roof/Termite reports).
3. PRIVACY & FAIR HOUSING: Never output personal owner/borrower names, SSNs, or account numbers, and never discuss demographics or protected classes. Never emit markdown code fences or script tags."#;

pub fn packet_review_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "executiveHeadline",
            "executiveSummary",
            "negotiationStrategy",
            "topQuestionsForSeller"
        ],
        "properties": {
            "executiveHeadline": { "type": "string" },
            "executiveSummary": { "type": "string" },
            "negotiationStrategy": { "type": "string" },
            "topQuestionsForSeller": {
                "type": "array",
                "items": { "type": "string" }
            }
        }
    })
}

/// Full async Multi-PDF / `.zip` Disclosure Packet Analyzer (`POST /api/packet-review`).
pub async fn analyze_disclosure_packet(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    documents: Vec<UploadedDocInput>,
    regional_multiplier: Option<f64>,
    cache: Option<&dyn LlmCache>,
) -> Result<ModuleOutput> {
    let t0 = Instant::now();
    let batch = extract_and_ocr_uploads_async(
        http,
        env,
        &documents,
        DocumentKind::Packet,
        MAX_PACKET_DOCUMENTS,
        220_000,
    )
    .await?;

    let mut parsed = analyze_packet_documents_deterministic(
        &batch.documents,
        Some(report),
        batch.pii_redacted_count,
        regional_multiplier,
    );

    init_deterministic_ai_meta(&mut parsed, Some(batch.files));

    let mut warnings = Vec::new();
    append_scrubbed_lines_warning(&mut warnings, batch.scrubbed_lines, "disclosure packet");

    if env.llm_configured() {
        let brief = build_report_brief_or_default(report, "Subject Residential Property");
        let det_summary = json!({
            "documentClassifications": parsed.get("documentClassifications"),
            "contradictions": parsed.get("contradictions"),
            "dealBreakerTriage": parsed.get("dealBreakerTriage"),
            "totalExposureUsd": parsed.get("totalExposureUsd"),
        });
        let clipped_text = clip_sanitized_text(&batch.sanitized_text, 75_000);

        let prompt = format!(
            "Synthesize an executive buyer briefing and negotiation strategy over this multi-document disclosure packet.\n\nPROPERTY_BRIEF_JSON:\n{}\n\nDETERMINISTIC_PACKET_ANALYSIS:\n{}\n\n{}",
            serde_json::to_string(&brief)?,
            serde_json::to_string(&det_summary)?,
            wrap_untrusted_block("untrusted_document_text", &clipped_text)
        );

        let mut opts = LlmCallOptions::new(
            "packet_review",
            "packet_review",
            PACKET_REVIEW_INSTRUCTIONS,
            prompt,
            packet_review_schema(),
        )
        .with_task("packet_review")
        .with_profile("deep_analysis");
        opts.max_output_tokens = Some(4_096);
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
                        if let Some(es) = res.data.get("executiveSummary").and_then(|v| v.as_str()) {
                            obj.insert(
                                "executiveSummary".to_string(),
                                Value::String(redact_title_pii(es).masked_text),
                            );
                        }
                        if let Some(ns) = res.data.get("negotiationStrategy").and_then(|v| v.as_str()) {
                            obj.insert(
                                "negotiationStrategy".to_string(),
                                Value::String(redact_title_pii(ns).masked_text),
                            );
                        }
                        let clean_qs = redact_pii_string_array(
                            res.data.get("topQuestionsForSeller"),
                            |s| redact_title_pii(s).masked_text,
                        );
                        if !clean_qs.is_empty() {
                            obj.insert("topQuestionsForSeller".to_string(), Value::Array(clean_qs));
                        }
                    }
                    stamp_ai_enhancement_meta(&mut parsed, &res, true);
                }
            }
            Err(e) => {
                warnings.push(format!(
                    "AI packet synthesis skipped (deterministic contradiction matrix & deduplicated exposure complete): {}",
                    scrub_sensitive_text(&e.to_string())
                ));
            }
        }
    }

    if let Some(obj) = parsed.as_object_mut() {
        obj.insert("ms".to_string(), json!(t0.elapsed().as_millis() as u64));
    }

    let file_names: Vec<String> = batch.documents.iter().map(|d| d.name.clone()).collect();
    let packet_src_url = {
        let dom = env.brand_domain();
        if dom == "open-estate.local" {
            "https://openestate.org/packet-review".to_string()
        } else {
            format!("https://{dom}/packet-review")
        }
    };
    Ok(ModuleOutput {
        data: parsed,
        sources: vec![SourceItem::new(
            format!(
                "{} Multi-Document Disclosure Packet & Contradiction Engine over {} file(s) ({})",
                env.brand_name(),
                file_names.len(),
                file_names.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            ),
            packet_src_url,
        )],
        warnings,
    })
}
