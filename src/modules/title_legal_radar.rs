//! Title, Lien, Pre-Foreclosure & Court Docket Radar + 50-State + DC Statutory/Filed Title Rate Engine.
//!
//! Zero-mock, zero-PII legal and title due-diligence module (`titleLegalRadar`) exposing:
//! - [`build_title_legal_radar_deterministic`]: pure synchronous enrichment for cached/stored reports
//!   inside `normalize_stored_report` (never invents prices or claims "0 liens on title" without a Prelim).
//! - [`run_title_legal_radar`]: live async enrichment for `orchestrator.rs`, querying keyless open-data
//!   County Recorder / Clerk Socrata portals (NYC ACRIS, Cook County Clerk, SF Assessor-Recorder),
//!   optional BYOK `ATTOM_API_KEY` recorder endpoints, public pre-foreclosure/tax-lien feeds, and
//!   **CourtListener REST API v4** (`type=r` RECAP federal civil & bankruptcy dockets) with strict
//!   4-second timeouts, offline guards (`http.is_offline_blocked()`), and pre-storage natural-person
//!   party-name redaction.

use crate::ai::title_review::redact_title_pii;
use crate::extensions::adapter::{BoxFuture, DataExtensionAdapter};
use crate::extensions::proto::v1::{
    extension_envelope_proto::Payload, CapabilityDomain, DriverKind, ExtensionEnvelopeProto,
    ExtensionManifestProto, ExtensionQueryProto, GeoScopeProto, InsuranceQuoteProto,
    ProvenanceTier, RateLimitSpecProto, SourceAttributionProto,
};
use crate::registry::rates_and_costs::lookup_who_pays;
use crate::registry::us_geo::state_abbr_for_fips;
use crate::shared::{
    now_iso, round_n, sanitize_user_warning, scrub_competitor_brands, EnvConfig, HttpClient,
    ModuleOutput, SourceItem,
};
use anyhow::Result;
use chrono::{Datelike, NaiveDate, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use url::Url;

pub const COURTLISTENER_SEARCH_DEFAULT_URL: &str =
    "https://www.courtlistener.com/api/rest/v4/search/";

pub const TITLE_LEGAL_RADAR_DISCLAIMER: &str = "Public-record title, statutory rate-filing, pre-foreclosure, and court-docket screening only — NOT legal advice, a title abstract, or a title insurance commitment. Unindexed mechanic's liens, state court actions, easements, and payoffs require an official Preliminary Title Report (Prelim) and escrow settlement review.";

const LIVE_QUERY_TIMEOUT_MS: u64 = 4_000;

// ------------------------- Static Regexes -------------------------

static RE_QUITCLAIM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bquit\s*claim\b").expect("quitclaim regex"));

static RE_DISTRESSED_DEED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:trustee['’]?s\s+deed(?:\s+upon\s+sale)?|sheriff['’]?s\s+deed|foreclosure\s+deed|deed\s+in\s+lieu|reo\b|bank[- ]owned|tax\s+deed|treasurer['’]?s\s+deed)\b")
        .expect("distressed deed regex")
});

static RE_ACTIVE_NOD_NTS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:notice\s+of\s+default|\bnod\b|notice\s+of\s+trustee['’]?s\s+sale|\bnts\b|notice\s+of\s+foreclosure\s+sale|lis\s+pendens|pendency\s+of\s+action|pre[- ]?foreclosure)\b")
        .expect("nod nts regex")
});

static RE_TAX_DELINQUENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:tax[- ]?default(?:ed)?|delinquent\s+(?:property\s+)?tax(?:es)?|unpaid\s+prior[- ]year\s+tax(?:es)?|tax\s+lien\s+sale|irs\s+lien|federal\s+tax\s+lien|state\s+tax\s+lien|franchise\s+tax\s+board\s+lien)\b")
        .expect("tax delinq regex")
});

static RE_PACE_HERO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:pace\s+(?:assessment|lien|program|loan)|hero\s+(?:assessment|lien|program)|ygrene|renovate\s+america|property\s+assessed\s+clean\s+energy)\b")
        .expect("pace hero regex")
});

static RE_SOLAR_PRESENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:solar\s+(?:pv|photovoltaic|panel|array|roof|system|lease|ppa)|photovoltaic)\b",
    )
    .expect("solar present regex")
});

static RE_SOLAR_OWNED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:owned\s+solar|solar\s+(?:owned|paid\s+off|owned\s+free\s+and\s+clear)|fully\s+owned\s+solar)\b")
        .expect("solar owned regex")
});

static RE_SOLAR_UCC_LEASE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:ucc[- ]?1|solar\s+(?:lease|ppa|power\s+purchase\s+agreement)|leased\s+solar|sunrun|vivint\s+solar|sunnova|teslasolar\s+lease)\b")
        .expect("solar ucc regex")
});

static RE_HOA_LITIGATION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:hoa\s+litigation|association\s+litigation|pending\s+(?:hoa|association|condo|structural|construction\s+defect)\s+(?:litigation|lawsuit)|construction[- ]defect\s+(?:lawsuit|litigation|action)|sb[- ]?326\s+(?:unsafe|repair\s+order)|non[- ]?warrantable\s+condo)\b")
        .expect("hoa litigation regex")
});

static RE_NO_HOA_LITIGATION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:no\s+(?:pending|active|known)\s+(?:hoa\s+|association\s+)?litigation|litigation\s*:\s*(?:none|no))\b")
        .expect("no hoa litigation regex")
});

static RE_INSTITUTIONAL_PARTY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:Bank|N\.?A\.?|National\s+Association|Mortgage|Servicing|Trust\s+Company|Title|Escrow|Insurance|Corp(?:oration)?|Inc\.?|L\.?L\.?C\.?|L\.?P\.?|LLP|Ltd\.?|Company|Co\.|Association|Homeowners|Condominium|HOA|POA|City\s+of|Town\s+of|Village\s+of|County\s+of|State\s+of|Commonwealth\s+of|United\s+States|Department|Agency|Commission|Authority|Board|District|Federal|Fannie\s+Mae|Freddie\s+Mac|MERS|U\.?S\.?\s+Bank|Wells\s+Fargo|JPMorgan|Chase|Citibank|Bank\s+of\s+America|Wilmington\s+Savings|Deutsche\s+Bank|Nationstar|Mr\.\s+Cooper|NewRez|Freedom\s+Mortgage|Rocket\s+Mortgage|Pennymac|Ocwen|PHH|Holdings|Properties|Investments|Ventures|Partners|Builders|Construction|Development|Credit\s+Union|FSB)\b",
    )
    .expect("institutional party regex")
});

static RE_CASE_VS_SPLIT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\s+(?:v\.|vs\.|versus)\s+").expect("vs split regex"));

static RE_IN_RE_PREFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^((?:in\s+re|in\s+the\s+matter\s+of|matter\s+of|estate\s+of)\s*:?\s+)(.+)$")
        .expect("in re prefix regex")
});

// ------------------------- Core Parsing Helpers -------------------------

fn unwrap_data(v: &Value) -> &Value {
    v.get("data").filter(|d| d.is_object()).unwrap_or(v)
}

fn module_data<'a>(report: &'a Value, key: &str) -> &'a Value {
    report
        .pointer(&format!("/modules/{key}"))
        .map(unwrap_data)
        .unwrap_or(&Value::Null)
}

fn val_f64(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    if let Some(n) = val.as_f64() {
        return n.is_finite().then_some(n);
    }
    if let Some(s) = val.as_str() {
        let clean: String = s
            .trim()
            .replace([',', '$', '%'], "")
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        return clean.parse::<f64>().ok().filter(|n| n.is_finite());
    }
    None
}

fn val_i64(v: Option<&Value>) -> Option<i64> {
    val_f64(v).map(|n| n.round() as i64)
}

fn val_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn parse_flexible_date(raw: &str) -> Option<NaiveDate> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.len() >= 10 {
        if let Ok(d) = NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d") {
            return Some(d);
        }
    }
    for fmt in ["%Y-%m-%d", "%m/%d/%Y", "%Y/%m/%d", "%B %d, %Y", "%b %d, %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d);
        }
    }
    if s.len() == 7 {
        let padded = format!("{s}-01");
        if let Ok(d) = NaiveDate::parse_from_str(&padded, "%Y-%m-%d") {
            return Some(d);
        }
    }
    None
}

fn resolve_as_of_date(report: &Value) -> NaiveDate {
    for ptr in ["/meta/generatedAt", "/meta/lastRefreshedAt"] {
        if let Some(raw) = val_str(report.pointer(ptr)) {
            if let Some(d) = parse_flexible_date(raw) {
                return d;
            }
        }
    }
    Utc::now().date_naive()
}

/// Strictly resolves ONLY subject-parcel price/estimate candidates (never ZIP medians or fallback constants).
/// Returns `None` if no subject price or valuation estimate exists on the report.
fn resolve_subject_price(facts: &Value, valuation: &Value) -> Option<f64> {
    let candidates = [
        val_f64(facts.get("listPrice")),
        val_f64(facts.get("price")),
        val_f64(facts.pointer("/mlsListing/listPrice")),
        val_f64(facts.pointer("/listing/listPrice")),
        val_f64(valuation.pointer("/estimate/value")),
        val_f64(valuation.pointer("/ownership/price")),
        facts
            .get("avms")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.iter().filter_map(|a| val_f64(a.get("value"))).next()),
        val_f64(facts.pointer("/lastSale/price")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|&p| p.is_finite() && p > 0.0)
}

#[derive(Debug, Clone)]
struct TransferEntry {
    date_str: String,
    date: NaiveDate,
    price: Option<i64>,
    deed_type: String,
    note: String,
}

fn extract_sorted_transfers(facts: &Value) -> Vec<TransferEntry> {
    let mut by_date: BTreeMap<String, TransferEntry> = BTreeMap::new();
    let mut insert_item =
        |dt_raw: &str, price: Option<i64>, deed_type: &str, note: &str, event: &str| {
            let Some(date) = parse_flexible_date(dt_raw) else {
                return;
            };
            let ev_lc = event.to_ascii_lowercase();
            let is_transfer = ev_lc.is_empty()
                || ev_lc.contains("sold")
                || ev_lc.contains("sale")
                || ev_lc.contains("close")
                || ev_lc.contains("transfer")
                || ev_lc.contains("deed");
            if !is_transfer {
                return;
            }
            let norm_dt = date.format("%Y-%m-%d").to_string();
            let clean_deed = if !deed_type.trim().is_empty() {
                deed_type.trim().to_string()
            } else if !event.trim().is_empty() && !event.eq_ignore_ascii_case("sold") {
                event.trim().to_string()
            } else {
                "Recorded sale / transfer".to_string()
            };
            by_date
                .entry(norm_dt.clone())
                .and_modify(|cur| {
                    if cur.price.is_none() && price.filter(|&p| p > 0).is_some() {
                        cur.price = price.filter(|&p| p > 0);
                    }
                    if cur.deed_type == "Recorded sale / transfer"
                        && clean_deed != "Recorded sale / transfer"
                    {
                        cur.deed_type = clean_deed.clone();
                    }
                    if cur.note.is_empty() && !note.trim().is_empty() {
                        cur.note = note.trim().to_string();
                    }
                })
                .or_insert_with(|| TransferEntry {
                    date_str: norm_dt,
                    date,
                    price: price.filter(|&p| p > 0),
                    deed_type: clean_deed,
                    note: note.trim().to_string(),
                });
        };

    if let Some(arr) = facts
        .pointer("/titleAndLiens/transfers")
        .and_then(|v| v.as_array())
    {
        for tr in arr {
            insert_item(
                val_str(tr.get("date")).unwrap_or(""),
                val_i64(tr.get("price")),
                val_str(tr.get("deedType")).unwrap_or(""),
                val_str(tr.get("note")).unwrap_or(""),
                val_str(tr.get("event")).unwrap_or(""),
            );
        }
    }
    if let Some(arr) = facts.get("saleHistory").and_then(|v| v.as_array()) {
        for s in arr {
            insert_item(
                val_str(s.get("date")).unwrap_or(""),
                val_i64(s.get("price")),
                val_str(s.get("deedType")).unwrap_or(""),
                val_str(s.get("note")).unwrap_or(""),
                val_str(s.get("event")).unwrap_or(""),
            );
        }
    }
    for k in ["lastSale", "priorSale"] {
        if let Some(ls) = facts.get(k).and_then(|v| v.as_object()) {
            insert_item(
                val_str(ls.get("date")).unwrap_or(""),
                val_i64(ls.get("price")),
                val_str(ls.get("deedType")).unwrap_or(""),
                val_str(ls.get("note")).unwrap_or(""),
                "Sold",
            );
        }
    }

    let mut list: Vec<TransferEntry> = by_date.into_values().collect();
    list.sort_by(|a, b| b.date.cmp(&a.date));
    list
}

fn resolve_state_and_county(geo: &Value, facts: &Value) -> (String, String, String, String) {
    let county_fips = val_str(geo.get("countyFips")).unwrap_or("").to_string();
    let state = val_str(geo.get("state"))
        .map(|s| s.to_ascii_uppercase())
        .or_else(|| {
            val_str(geo.get("stateFips"))
                .and_then(state_abbr_for_fips)
                .map(String::from)
        })
        .or_else(|| state_abbr_for_fips(&county_fips).map(String::from))
        .or_else(|| val_str(facts.get("state")).map(|s| s.to_ascii_uppercase()))
        .unwrap_or_default();
    let county = val_str(geo.get("county")).unwrap_or("").to_string();
    let city = val_str(geo.get("city"))
        .or_else(|| val_str(geo.get("place")).map(|p| p.trim_end_matches(" city").trim()))
        .or_else(|| val_str(facts.get("city")))
        .unwrap_or("")
        .to_string();
    (state, county, county_fips, city)
}

fn resolve_street_line(report: &Value, geo: &Value, facts: &Value) -> Option<String> {
    if let Some(st) = val_str(geo.get("street")).or_else(|| val_str(facts.get("street"))) {
        if st.len() >= 5 {
            return Some(st.to_string());
        }
    }
    for candidate in [
        val_str(geo.get("displayAddress")),
        val_str(report.pointer("/meta/displayAddress")),
        val_str(geo.get("matchedAddress")),
        val_str(report.pointer("/meta/input")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(first_seg) = candidate.split(',').next() {
            let trimmed = first_seg.trim();
            if trimmed.len() >= 5 && trimmed.chars().any(|c| c.is_ascii_digit()) {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

// =================================================================================================
// 1. 50-State + DC Statutory & State-Filed Title Insurance Rate Calculator (`underwriterRateFiling`)
// =================================================================================================

#[derive(Debug, Clone)]
struct StateTitleScheduleSpec {
    state: &'static str,
    state_name: &'static str,
    rate_regime: &'static str, // "promulgated_statutory" | "state_filed_tariff" | "estimated_schedule"
    rate_schedule_basis: &'static str, // "statutory_formula" | "rating_bureau_manual" | "modeled_regional_benchmark"
    regulator: &'static str,
    citation: &'static str,
    filing_url: &'static str,
    reissue_window_years: i64,
    reissue_credit_short_pct: f64,    // <= 5 years
    reissue_credit_extended_pct: f64, // 5..=reissue_window_years
    enhanced_multiplier: f64,
    base_settlement_usd: i64,
}

fn state_title_schedule_spec(state: &str) -> StateTitleScheduleSpec {
    match state.trim().to_ascii_uppercase().as_str() {
        // --- Exact Promulgated / Statutory Rate States ---
        "TX" => StateTitleScheduleSpec {
            state: "TX",
            state_name: "Texas",
            rate_regime: "promulgated_statutory",
            rate_schedule_basis: "statutory_formula",
            regulator: "Texas Department of Insurance (TDI)",
            citation: "28 TAC § 9.1 · TDI Basic Manual of Title Insurance (Order No. 2025-9697, eff. Mar 1, 2026), Rate Rules R-1, R-5 & R-8",
            filing_url: "https://www.tdi.texas.gov/title/titleman.html",
            reissue_window_years: 7,
            reissue_credit_short_pct: 0.0, // TX R-1 owner policy is fixed; R-8 credit applies to loan policy payoffs within 7 yrs
            reissue_credit_extended_pct: 0.0,
            enhanced_multiplier: 1.10, // T-1R + T-19.1 residential restrictions/encroachments/minerals endorsement
            base_settlement_usd: 950,
        },
        "FL" => StateTitleScheduleSpec {
            state: "FL",
            state_name: "Florida",
            rate_regime: "promulgated_statutory",
            rate_schedule_basis: "statutory_formula",
            regulator: "Florida Office of Insurance Regulation (OIR)",
            citation: "Fla. Stat. § 627.782 · Fla. Admin. Code r. 69O-186.003 ($5.75/$1k to $100k; $5.00/$1k $100k–$1M; $2.50/$1k $1M–$5M)",
            filing_url: "https://www.floir.com/sections/pandc/title",
            reissue_window_years: 10,
            reissue_credit_short_pct: 35.0, // F.A.C. 69O-186.003(2) $3.30/$1k to $100k + $3.00/$1k $100k-$1M (~38-40% off original rate)
            reissue_credit_extended_pct: 25.0,
            enhanced_multiplier: 1.20, // FL OIR promulgated ALTA Homeowner's 120% rate
            base_settlement_usd: 895,
        },
        "NM" => StateTitleScheduleSpec {
            state: "NM",
            state_name: "New Mexico",
            rate_regime: "promulgated_statutory",
            rate_schedule_basis: "statutory_formula",
            regulator: "New Mexico Office of Superintendent of Insurance (OSI)",
            citation: "NMSA 1978 § 59A-30-6 · 13.14.9.18 & 13.14.9.35 NMAC Promulgated Schedule of Basic Premium Rates",
            filing_url: "https://www.osi.state.nm.us/pages/bureaus/title-bureau/resources",
            reissue_window_years: 10,
            reissue_credit_short_pct: 25.0, // 13.14.9.35 NMAC reissue discount tier
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.10,
            base_settlement_usd: 875,
        },
        "IA" => StateTitleScheduleSpec {
            state: "IA",
            state_name: "Iowa",
            rate_regime: "promulgated_statutory",
            rate_schedule_basis: "statutory_formula",
            regulator: "Iowa Finance Authority — Iowa Title Guaranty (ITG)",
            citation: "Iowa Code §§ 515.48(10) & 16.91 (Commercial title insurance prohibited; statutory Iowa Title Guaranty flat $175 <= $750k + abstract & attorney opinion)",
            filing_url: "https://www.iowatitleguaranty.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 0.0,
            reissue_credit_extended_pct: 0.0,
            enhanced_multiplier: 1.00,
            base_settlement_usd: 925, // Includes abstract continuation + examining attorney opinion + closing
        },

        // --- Competitive / CDI-Modeled CA Benchmark Curve (individual filed underwriter schedules vary) ---
        "CA" => StateTitleScheduleSpec {
            state: "CA",
            state_name: "California",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "California Department of Insurance (CDI)",
            citation: "CA Ins. Code §§ 12401.1–12401.9 · Representative CDI residential benchmark curve (actual insurer-filed schedules vary)",
            filing_url: "https://interactive.web.insurance.ca.gov/apex_extprd/f?p=150:1",
            reissue_window_years: 5,
            reissue_credit_short_pct: 20.0,
            reissue_credit_extended_pct: 10.0,
            enhanced_multiplier: 1.10,
            base_settlement_usd: 1_350,
        },

        // --- Rating-Bureau State-Filed Tariff Manual States ---
        "NY" => StateTitleScheduleSpec {
            state: "NY",
            state_name: "New York",
            rate_regime: "state_filed_tariff",
            rate_schedule_basis: "rating_bureau_manual",
            regulator: "New York State Department of Financial Services (DFS)",
            citation: "NY Ins. Law § 2305 · Title Insurance Rate Service Association (TIRSA) Rate Manual (Zone 1 / Zone 2)",
            filing_url: "https://www.tirsa.org/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 30.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.20, // TIRSA Market Value Policy Rider + ALTA Homeowner's
            base_settlement_usd: 1_550,
        },
        "PA" => StateTitleScheduleSpec {
            state: "PA",
            state_name: "Pennsylvania",
            rate_regime: "state_filed_tariff",
            rate_schedule_basis: "rating_bureau_manual",
            regulator: "Pennsylvania Insurance Department",
            citation: "40 P.S. § 910-37 · Title Insurance Rating Bureau of Pennsylvania (TIRBOP) All-Inclusive Rate Manual",
            filing_url: "https://www.pa.gov/agencies/insurance.html",
            reissue_window_years: 10,
            reissue_credit_short_pct: 10.0, // TIRBOP Reissue Rate (90% of Sale Rate within 10 years)
            reissue_credit_extended_pct: 10.0,
            enhanced_multiplier: 1.10,
            base_settlement_usd: 650, // Search & exam bundled in TIRBOP all-inclusive premium
        },
        "NJ" => StateTitleScheduleSpec {
            state: "NJ",
            state_name: "New Jersey",
            rate_regime: "state_filed_tariff",
            rate_schedule_basis: "rating_bureau_manual",
            regulator: "New Jersey Dept. of Banking and Insurance (DOBI)",
            citation: "N.J.S.A. 17:46B-42 · NJ Land Title Insurance Rating Bureau (NJLTIRB) Manual ($5.25/$1k to $100k; $4.25/$1k $100k–$500k; $2.75/$1k $500k–$2M)",
            filing_url: "https://www.nj.gov/dobi/division_insurance/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 30.0,
            reissue_credit_extended_pct: 20.0,
            enhanced_multiplier: 1.20,
            base_settlement_usd: 1_300,
        },
        "DE" => StateTitleScheduleSpec {
            state: "DE",
            state_name: "Delaware",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Delaware Department of Insurance",
            citation: "18 Del. C. § 2504 · Modeled residential benchmark (verify Delaware Title Insurance Rating Bureau DTIRB manual)",
            filing_url: "https://insurance.delaware.gov/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 20.0,
            reissue_credit_extended_pct: 10.0,
            enhanced_multiplier: 1.20,
            base_settlement_usd: 1_250,
        },
        "OH" => StateTitleScheduleSpec {
            state: "OH",
            state_name: "Ohio",
            rate_regime: "state_filed_tariff",
            rate_schedule_basis: "rating_bureau_manual",
            regulator: "Ohio Department of Insurance (ODI)",
            citation: "Ohio Rev. Code § 3935.04 · Ohio Title Insurance Rating Bureau (OTIRB) Rule PR-1 (eff. Jan 1, 2026: $5.80/$1k to $250k; $4.10/$1k $250k–$500k; $3.20/$1k $500k–$1M; $3.10/$1k $1M–$5M)",
            filing_url: "https://insurance.ohio.gov/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 30.0, // OTIRB PR-4 30% Reissue Credit within 10 years
            reissue_credit_extended_pct: 30.0,
            enhanced_multiplier: 1.15,
            base_settlement_usd: 825,
        },
        "OR" => StateTitleScheduleSpec {
            state: "OR",
            state_name: "Oregon",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Oregon Division of Financial Regulation (DFR)",
            citation: "ORS 737.320 · Modeled regional residential benchmark (confirm underwriter OTIRO tariff filing)",
            filing_url: "https://dfr.oregon.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.12,
            base_settlement_usd: 1_100,
        },
        "WA" => StateTitleScheduleSpec {
            state: "WA",
            state_name: "Washington",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Washington Office of the Insurance Commissioner (OIC)",
            citation: "RCW 48.29.140 · Modeled regional residential benchmark (confirm WAC 284-29A OIC insurer filing)",
            filing_url: "https://www.insurance.wa.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.10,
            base_settlement_usd: 1_250,
        },
        "AZ" => StateTitleScheduleSpec {
            state: "AZ",
            state_name: "Arizona",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Arizona Dept. of Insurance and Financial Institutions (DIFI)",
            citation: "A.R.S. §§ 20-376 & 6-846 · Modeled regional residential benchmark (confirm DIFI county insurer filing)",
            filing_url: "https://difi.az.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.10,
            base_settlement_usd: 995,
        },
        "NV" => StateTitleScheduleSpec {
            state: "NV",
            state_name: "Nevada",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Nevada Division of Insurance",
            citation: "NRS 692A.120 & 686A.130 · Modeled regional residential benchmark (confirm NV DOI insurer filing)",
            filing_url: "https://doi.nv.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.10,
            base_settlement_usd: 1_050,
        },
        "CO" => StateTitleScheduleSpec {
            state: "CO",
            state_name: "Colorado",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Colorado Division of Insurance (DORA)",
            citation: "C.R.S. § 10-11-118 · Modeled regional residential benchmark (confirm 3 CCR 702-8 Reg. 8-1-1 insurer schedule)",
            filing_url: "https://doi.colorado.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 30.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.10,
            base_settlement_usd: 925,
        },
        "NC" => StateTitleScheduleSpec {
            state: "NC",
            state_name: "North Carolina",
            rate_regime: "state_filed_tariff",
            rate_schedule_basis: "rating_bureau_manual",
            regulator: "North Carolina Department of Insurance (NCDOI)",
            citation: "N.C.G.S. § 58-26-1 · NC Title Insurance Rating Bureau Schedule ($2.53/$1k to $100k; $1.97/$1k $100k–$500k; $1.29/$1k $500k–$2M)",
            filing_url: "https://www.ncdoi.gov/",
            reissue_window_years: 15,
            reissue_credit_short_pct: 50.0, // NCTIRB 50% Reissue Credit up to prior policy face within 15 yrs
            reissue_credit_extended_pct: 50.0,
            enhanced_multiplier: 1.20,
            base_settlement_usd: 1_150, // Attorney closing state
        },
        "GA" => StateTitleScheduleSpec {
            state: "GA",
            state_name: "Georgia",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Georgia Office of Commissioner of Insurance and Safety Fire",
            citation: "O.C.G.A. § 33-9-21 · Modeled regional residential benchmark (confirm GA OCI insurer filing)",
            filing_url: "https://oci.georgia.gov/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 30.0,
            reissue_credit_extended_pct: 20.0,
            enhanced_multiplier: 1.15,
            base_settlement_usd: 1_150, // Mandatory closing attorney state
        },

        // --- Modeled Benchmark States (Competitive / File-and-Use Jurisdictions) ---
        "IL" => StateTitleScheduleSpec {
            state: "IL",
            state_name: "Illinois",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Illinois Dept. of Financial and Professional Regulation (IDFPR)",
            citation: "215 ILCS 155/ Title Insurance Act · Modeled regional residential benchmark (confirm underwriter disclosure schedule)",
            filing_url: "https://idfpr.illinois.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 20.0,
            reissue_credit_extended_pct: 10.0,
            enhanced_multiplier: 1.12,
            base_settlement_usd: 1_350, // Title escrow + residential real-estate attorney custom
        },
        "MA" => StateTitleScheduleSpec {
            state: "MA",
            state_name: "Massachusetts",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Massachusetts Division of Insurance",
            citation: "M.G.L. c. 175 § 114 · Modeled regional residential benchmark (confirm underwriter/attorney schedule)",
            filing_url: "https://www.mass.gov/orgs/division-of-insurance",
            reissue_window_years: 10,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.15,
            base_settlement_usd: 1_350,
        },
        "VA" => StateTitleScheduleSpec {
            state: "VA",
            state_name: "Virginia",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Virginia State Corporation Commission — Bureau of Insurance",
            citation: "Va. Code § 38.2-4608 · Modeled regional residential benchmark (confirm competitive underwriter schedule)",
            filing_url: "https://scc.virginia.gov/pages/Insurance",
            reissue_window_years: 10,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.15,
            base_settlement_usd: 995,
        },
        "DC" => StateTitleScheduleSpec {
            state: "DC",
            state_name: "District of Columbia",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "DC Dept. of Insurance, Securities and Banking (DISB)",
            citation: "D.C. Code § 31-5031.17 · Modeled regional residential benchmark (confirm DISB underwriter filing)",
            filing_url: "https://disb.dc.gov/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.15,
            base_settlement_usd: 1_195,
        },
        "IN" => StateTitleScheduleSpec {
            state: "IN",
            state_name: "Indiana",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Indiana Department of Insurance (IDOI)",
            citation: "Ind. Code § 27-7-3 · Modeled regional residential benchmark (+ $5 TIEFF Fee; confirm underwriter schedule)",
            filing_url: "https://www.in.gov/idoi/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.15,
            base_settlement_usd: 795,
        },
        "OK" => StateTitleScheduleSpec {
            state: "OK",
            state_name: "Oklahoma",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Oklahoma Insurance Department & Oklahoma Abstractors Board",
            citation: "36 O.S. § 5001 · Modeled regional residential benchmark (abstract + attorney title exam + underwriter schedule)",
            filing_url: "https://www.oid.ok.gov/",
            reissue_window_years: 5,
            reissue_credit_short_pct: 20.0,
            reissue_credit_extended_pct: 10.0,
            enhanced_multiplier: 1.12,
            base_settlement_usd: 1_150,
        },
        "VT" => StateTitleScheduleSpec {
            state: "VT",
            state_name: "Vermont",
            rate_regime: "estimated_schedule",
            rate_schedule_basis: "modeled_regional_benchmark",
            regulator: "Vermont Dept. of Financial Regulation (DFR)",
            citation: "8 V.S.A. § 4688 · Modeled regional residential benchmark (confirm DFR underwriter schedule)",
            filing_url: "https://dfr.vermont.gov/",
            reissue_window_years: 10,
            reissue_credit_short_pct: 25.0,
            reissue_credit_extended_pct: 15.0,
            enhanced_multiplier: 1.15,
            base_settlement_usd: 1_250,
        },

        // --- Remaining 27 US States (Modeled Regional Benchmark Curves; Confirm Underwriter Filing) ---
        other => {
            let (st, sname, cite, url, settle) = match other {
                "AL" => ("AL", "Alabama", "Ala. Code § 27-25-6 · Modeled residential benchmark (AL DOI)", "https://www.aldoi.gov/", 895),
                "AK" => ("AK", "Alaska", "AS 21.66.370 · Modeled residential benchmark (Alaska DOI)", "https://www.commerce.alaska.gov/web/ins/", 1_295),
                "AR" => ("AR", "Arkansas", "Ark. Code Ann. § 23-67-211 · Modeled residential benchmark (AR Insurance Dept.)", "https://insurance.arkansas.gov/", 795),
                "CT" => ("CT", "Connecticut", "Conn. Gen. Stat. § 38a-419 · Modeled residential benchmark (CT Insurance Dept.)", "https://portal.ct.gov/cid", 1_250),
                "HI" => ("HI", "Hawaii", "HRS § 431:20-120 · Modeled residential benchmark (Hawaii Insurance Div.)", "https://cca.hawaii.gov/ins/", 1_395),
                "ID" => ("ID", "Idaho", "Idaho Code § 41-2706 · Modeled residential benchmark (ID DOI)", "https://doi.idaho.gov/", 950),
                "KS" => ("KS", "Kansas", "K.S.A. 40-1137 · Modeled residential benchmark (KS Insurance Dept.)", "https://insurance.kansas.gov/", 795),
                "KY" => ("KY", "Kentucky", "KRS 304.13-051 · Modeled residential benchmark (KY DOI)", "https://insurance.ky.gov/", 895),
                "LA" => ("LA", "Louisiana", "La. R.S. 22:1464 · Modeled residential benchmark (LATISSO / LA DOI)", "https://www.ldi.la.gov/", 1_095),
                "ME" => ("ME", "Maine", "24-A M.R.S. § 2304-A · Modeled residential benchmark (Maine BOI)", "https://www.maine.gov/pfr/insurance/", 1_050),
                "MD" => ("MD", "Maryland", "Md. Code, Ins. § 11-403 · Modeled residential benchmark (MD MIA)", "https://insurance.maryland.gov/", 1_050),
                "MI" => ("MI", "Michigan", "MCL 500.2406 · Modeled residential benchmark (MI DIFS)", "https://www.michigan.gov/difs", 850),
                "MN" => ("MN", "Minnesota", "Minn. Stat. § 70A.06 · Modeled residential benchmark (MN Dept. of Commerce)", "https://mn.gov/commerce/", 895),
                "MS" => ("MS", "Mississippi", "Miss. Code Ann. § 83-15-9 · Modeled residential benchmark (MS Insurance Dept.)", "https://www.mid.ms.gov/", 895),
                "MO" => ("MO", "Missouri", "RSMo § 381.181 · Modeled residential benchmark (MO DCI)", "https://insurance.mo.gov/", 795),
                "MT" => ("MT", "Montana", "MCA § 33-25-212 · Modeled residential benchmark (MT CSI)", "https://csimt.gov/", 950),
                "NE" => ("NE", "Nebraska", "Neb. Rev. Stat. § 44-1993 · Modeled residential benchmark (NE DOI)", "https://doi.nebraska.gov/", 795),
                "NH" => ("NH", "New Hampshire", "RSA 416-A:17 · Modeled residential benchmark (NH Insurance Dept.)", "https://www.nh.gov/insurance/", 995),
                "ND" => ("ND", "North Dakota", "N.D.C.C. § 26.1-20-04 · Modeled residential benchmark (ND Insurance Dept.)", "https://www.insurance.nd.gov/", 850),
                "RI" => ("RI", "Rhode Island", "R.I. Gen. Laws § 27-2.6-16 · Modeled residential benchmark (RI DBR)", "https://dbr.ri.gov/", 1_150),
                "SC" => ("SC", "South Carolina", "S.C. Code Ann. § 38-75-980 · Modeled residential benchmark (SC DOI · Attorney Closing)", "https://doi.sc.gov/", 1_100),
                "SD" => ("SD", "South Dakota", "SDCL § 58-25-7 · Modeled residential benchmark (SD DOI)", "https://dlr.sd.gov/insurance/", 825),
                "TN" => ("TN", "Tennessee", "Tenn. Code Ann. § 56-35-111 · Modeled residential benchmark (TN TDCI)", "https://www.tn.gov/commerce/insurance.html", 875),
                "UT" => ("UT", "Utah", "Utah Code § 31A-19a-209 · Modeled residential benchmark (Utah DOI)", "https://insurance.utah.gov/", 925),
                "WV" => ("WV", "West Virginia", "W. Va. Code § 33-20-4 · Modeled residential benchmark (WV OIC)", "https://www.wvinsurance.gov/", 950),
                "WI" => ("WI", "Wisconsin", "Wis. Stat. § 625.13 · Modeled residential benchmark (WI OCI)", "https://oci.wi.gov/", 825),
                "WY" => ("WY", "Wyoming", "Wyo. Stat. § 26-23-326 · Modeled residential benchmark (WY DOI)", "https://doi.wyo.gov/", 895),
                _ => ("US", "United States (Jurisdiction Unspecified)", "Modeled US Residential Title Benchmark (confirm underwriter schedule)", "https://content.naic.org/state-insurance-departments", 995),
            };
            StateTitleScheduleSpec {
                state: st,
                state_name: sname,
                rate_regime: "estimated_schedule",
                rate_schedule_basis: "modeled_regional_benchmark",
                regulator: sname,
                citation: cite,
                filing_url: url,
                reissue_window_years: 7,
                reissue_credit_short_pct: 25.0,
                reissue_credit_extended_pct: 15.0,
                enhanced_multiplier: 1.125,
                base_settlement_usd: settle,
            }
        }
    }
}

fn calc_bracketed_owner_and_lender(
    state: &str,
    county_fips: &str,
    price: f64,
    loan: f64,
) -> (i64, i64, Vec<Value>) {
    let p = price.max(0.0);
    let mut tiers = Vec::new();
    let mut push_tier = |label: &str, taxable: f64, rate_per_k: f64, sub: f64| {
        if taxable > 0.0 {
            tiers.push(json!({
                "bracket": label,
                "coveredAmountUsd": taxable.round() as i64,
                "ratePerThousandUsd": round_n(rate_per_k, 2),
                "subtotalUsd": sub.round() as i64,
            }));
        }
    };

    match state {
        "TX" => {
            // Texas TDI Basic Manual Rate Rule R-1 (Order No. 2025-9697, 6.2% reduction eff. Mar 1, 2026)
            let t1_amt = p.min(100_000.0);
            let t1_sub = if p <= 10_000.0 {
                223.0
            } else if p <= 50_000.0 {
                223.0 + (p - 10_000.0) * 0.00605
            } else if p <= 100_000.0 {
                465.0 + (p - 50_000.0) * 0.0063
            } else {
                780.0
            };
            push_tier(
                "Up to $100,000 (TDI R-1 Base, eff. Mar 2026)",
                t1_amt,
                if t1_amt > 0.0 { (t1_sub / t1_amt) * 1_000.0 } else { 7.80 },
                t1_sub,
            );

            let t2_amt = (p.min(1_000_000.0) - 100_000.0).max(0.0);
            let t2_sub = t2_amt * 0.00494;
            push_tier("$100,001 – $1,000,000 ($4.94/$1k)", t2_amt, 4.94, t2_sub);

            let t3_amt = (p.min(5_000_000.0) - 1_000_000.0).max(0.0);
            let t3_sub = t3_amt * 0.00406;
            push_tier("$1,000,001 – $5,000,000 ($4.06/$1k)", t3_amt, 4.06, t3_sub);

            let t4_amt = (p.min(15_000_000.0) - 5_000_000.0).max(0.0);
            let t4_sub = t4_amt * 0.00335;
            push_tier("$5,000,001 – $15,000,000 ($3.35/$1k)", t4_amt, 3.35, t4_sub);

            let t5_amt = (p - 15_000_000.0).max(0.0);
            let t5_sub = t5_amt * 0.00238;
            push_tier("Over $15,000,000 ($2.38/$1k)", t5_amt, 2.38, t5_sub);

            let owner = (t1_sub + t2_sub + t3_sub + t4_sub + t5_sub).round() as i64;
            // TX R-5 Simultaneous Loan Policy ($100 base + T-19 $50 + T-30 $20 + T-36 $25)
            let lender = if loan > 0.0 { 195 } else { 0 };
            (owner, lender, tiers)
        }
        "FL" => {
            // Exact Florida OIR Statutory Formula (F.A.C. 69O-186.003)
            let t1_amt = p.min(100_000.0);
            let t1_sub = (t1_amt * 0.00575).max(100.0);
            push_tier("Up to $100,000 ($5.75/$1k)", t1_amt, 5.75, t1_sub);

            let t2_amt = (p.min(1_000_000.0) - 100_000.0).max(0.0);
            let t2_sub = t2_amt * 0.00500;
            push_tier("$100,001 – $1,000,000 ($5.00/$1k)", t2_amt, 5.00, t2_sub);

            let t3_amt = (p.min(5_000_000.0) - 1_000_000.0).max(0.0);
            let t3_sub = t3_amt * 0.00250;
            push_tier("$1,000,001 – $5,000,000 ($2.50/$1k)", t3_amt, 2.50, t3_sub);

            let t4_amt = (p.min(10_000_000.0) - 5_000_000.0).max(0.0);
            let t4_sub = t4_amt * 0.00225;
            push_tier("$5,000,001 – $10,000,000 ($2.25/$1k)", t4_amt, 2.25, t4_sub);

            let t5_amt = (p - 10_000_000.0).max(0.0);
            let t5_sub = t5_amt * 0.00200;
            push_tier("Over $10,000,000 ($2.00/$1k)", t5_amt, 2.00, t5_sub);

            let owner_raw = t1_sub + t2_sub + t3_sub + t4_sub + t5_sub;
            let owner = owner_raw.round() as i64;
            // Simultaneous loan ($25 min simultaneous + FL Form 9 10% endorsement + ALTA 8.1 $50)
            let lender = if loan > 0.0 {
                (75.0 + owner_raw * 0.10).round() as i64
            } else {
                0
            };
            (owner, lender, tiers)
        }
        "NM" => {
            // Exact New Mexico OSI Promulgated Formula (13.14.9.18 NMAC)
            let t1_amt = p.min(100_000.0);
            let t1_sub = if p <= 100_000.0 {
                (212.0 + (p - 10_000.0).max(0.0) * 0.00519).min(679.0)
            } else {
                679.0
            };
            push_tier("Up to $100,000 (13.14.9.18 NMAC)", t1_amt, 6.79, t1_sub);

            let t2_amt = (p.min(500_000.0) - 100_000.0).max(0.0);
            let t2_sub = t2_amt * 0.00474;
            push_tier("$100,001 – $500,000 ($4.74/$1k)", t2_amt, 4.74, t2_sub);

            let t3_amt = (p.min(1_000_000.0) - 500_000.0).max(0.0);
            let t3_sub = t3_amt * 0.00373;
            push_tier("$500,001 – $1,000,000 ($3.73/$1k)", t3_amt, 3.73, t3_sub);

            let t4_amt = (p.min(5_000_000.0) - 1_000_000.0).max(0.0);
            let t4_sub = t4_amt * 0.00279;
            push_tier("$1,000,001 – $5,000,000 ($2.79/$1k)", t4_amt, 2.79, t4_sub);

            let t5_amt = (p - 5_000_000.0).max(0.0);
            let t5_sub = t5_amt * 0.00202;
            push_tier("Over $5,000,000 ($2.02/$1k)", t5_amt, 2.02, t5_sub);

            let owner = (t1_sub + t2_sub + t3_sub + t4_sub + t5_sub).round() as i64;
            let lender = if loan > 0.0 { 165 } else { 0 };
            (owner, lender, tiers)
        }
        "IA" => {
            // Iowa Title Guaranty (ITG) Statutory Schedule (Iowa Code § 16.91)
            let base_amt = p.min(750_000.0);
            push_tier(
                "Up to $750,000 (ITG Flat Statutory Certificate)",
                base_amt,
                0.23,
                175.0,
            );
            let excess = (p - 750_000.0).max(0.0);
            let excess_sub = (excess / 1_000.0).ceil() * 1.00;
            push_tier(
                "Over $750,000 ($1.00/$1k ITG Excess)",
                excess,
                1.00,
                excess_sub,
            );
            let owner = (175.0 + excess_sub).round() as i64;
            let lender = if loan > 0.0 { 175 } else { 0 };
            (owner, lender, tiers)
        }
        "CA" => {
            // CA CDI Filed Residential Schedule (CLTA Standard Owner's)
            let t1_amt = p.min(300_000.0);
            let t1_sub = 575.0 + t1_amt * 0.00245;
            push_tier(
                "Up to $300,000 (CDI Base + $2.45/$1k)",
                t1_amt,
                2.45,
                t1_sub,
            );

            let t2_amt = (p.min(1_000_000.0) - 300_000.0).max(0.0);
            let t2_sub = t2_amt * 0.00195;
            push_tier("$300,001 – $1,000,000 ($1.95/$1k)", t2_amt, 1.95, t2_sub);

            let t3_amt = (p.min(3_000_000.0) - 1_000_000.0).max(0.0);
            let t3_sub = t3_amt * 0.00145;
            push_tier("$1,000,001 – $3,000,000 ($1.45/$1k)", t3_amt, 1.45, t3_sub);

            let t4_amt = (p.min(5_000_000.0) - 3_000_000.0).max(0.0);
            let t4_sub = t4_amt * 0.00115;
            push_tier("$3,000,001 – $5,000,000 ($1.15/$1k)", t4_amt, 1.15, t4_sub);

            let t5_amt = (p - 5_000_000.0).max(0.0);
            let t5_sub = t5_amt * 0.00090;
            push_tier("Over $5,000,000 ($0.90/$1k)", t5_amt, 0.90, t5_sub);

            let owner = (t1_sub + t2_sub + t3_sub + t4_sub + t5_sub).round() as i64;
            let lender = if loan > 0.0 {
                (450.0 + loan * 0.00048).round().clamp(450.0, 2_250.0) as i64
            } else {
                0
            };
            (owner, lender, tiers)
        }
        "NY" => {
            // NY TIRSA Rate Manual (Zone 2 Downstate vs Zone 1 Upstate)
            let is_zone2 = matches!(
                county_fips.trim(),
                "36005"
                    | "36047"
                    | "36061"
                    | "36081"
                    | "36085"
                    | "36059"
                    | "36103"
                    | "36119"
                    | "36087"
                    | "36079"
                    | "36071"
                    | "36027"
            ) || county_fips.trim().is_empty();
            let mult = if is_zone2 { 1.0 } else { 0.86 };

            let b1 = p.min(35_000.0);
            let s1 = 402.0 * mult;
            push_tier("Up to $35,000 (TIRSA Minimum)", b1, 11.49 * mult, s1);

            let b2 = (p.min(50_000.0) - 35_000.0).max(0.0);
            let s2 = b2 * 0.00667 * mult;
            push_tier("$35,001 – $50,000", b2, 6.67 * mult, s2);

            let b3 = (p.min(100_000.0) - 50_000.0).max(0.0);
            let s3 = b3 * 0.00543 * mult;
            push_tier("$50,001 – $100,000", b3, 5.43 * mult, s3);

            let b4 = (p.min(500_000.0) - 100_000.0).max(0.0);
            let s4 = b4 * 0.00436 * mult;
            push_tier("$100,001 – $500,000", b4, 4.36 * mult, s4);

            let b5 = (p.min(1_000_000.0) - 500_000.0).max(0.0);
            let s5 = b5 * 0.00398 * mult;
            push_tier("$500,001 – $1,000,000", b5, 3.98 * mult, s5);

            let b6 = (p.min(5_000_000.0) - 1_000_000.0).max(0.0);
            let s6 = b6 * 0.00366 * mult;
            push_tier("$1,000,001 – $5,000,000", b6, 3.66 * mult, s6);

            let b7 = (p - 5_000_000.0).max(0.0);
            let s7 = b7 * 0.00325 * mult;
            push_tier("Over $5,000,000", b7, 3.25 * mult, s7);

            let owner = (s1 + s2 + s3 + s4 + s5 + s6 + s7).round() as i64;
            // TIRSA Section 6 Simultaneous Loan Rate (~30% of loan basic rate up to owner face)
            let lender = if loan > 0.0 {
                ((owner as f64) * 0.24).round().max(325.0) as i64
            } else {
                0
            };
            (owner, lender, tiers)
        }
        "NJ" => {
            // NJLTIRB Standard Underwriting Schedule
            let b1 = p.min(100_000.0);
            let s1 = (b1 * 0.00525).max(200.0);
            push_tier("Up to $100,000 ($5.25/$1k)", b1, 5.25, s1);

            let b2 = (p.min(500_000.0) - 100_000.0).max(0.0);
            let s2 = b2 * 0.00425;
            push_tier("$100,001 – $500,000 ($4.25/$1k)", b2, 4.25, s2);

            let b3 = (p.min(2_000_000.0) - 500_000.0).max(0.0);
            let s3 = b3 * 0.00275;
            push_tier("$500,001 – $2,000,000 ($2.75/$1k)", b3, 2.75, s3);

            let b4 = (p - 2_000_000.0).max(0.0);
            let s4 = b4 * 0.00200;
            push_tier("Over $2,000,000 ($2.00/$1k)", b4, 2.00, s4);

            let owner = (s1 + s2 + s3 + s4).round() as i64;
            let lender = if loan > 0.0 { 225 } else { 0 }; // NJLTIRB simultaneous loan + endorsements
            (owner, lender, tiers)
        }
        "PA" => {
            // TIRBOP All-Inclusive Sale Rate Manual
            let b1 = p.min(30_000.0);
            let s1 = 570.0;
            push_tier("Up to $30,000 (TIRBOP Base)", b1, 19.00, s1);

            let b2 = (p.min(45_000.0) - 30_000.0).max(0.0);
            let s2 = b2 * 0.00650;
            push_tier("$30,001 – $45,000 ($6.50/$1k)", b2, 6.50, s2);

            let b3 = (p.min(100_000.0) - 45_000.0).max(0.0);
            let s3 = b3 * 0.00575;
            push_tier("$45,001 – $100,000 ($5.75/$1k)", b3, 5.75, s3);

            let b4 = (p.min(500_000.0) - 100_000.0).max(0.0);
            let s4 = b4 * 0.00500;
            push_tier("$100,001 – $500,000 ($5.00/$1k)", b4, 5.00, s4);

            let b5 = (p.min(1_000_000.0) - 500_000.0).max(0.0);
            let s5 = b5 * 0.00400;
            push_tier("$500,001 – $1,000,000 ($4.00/$1k)", b5, 4.00, s5);

            let b6 = (p.min(2_000_000.0) - 1_000_000.0).max(0.0);
            let s6 = b6 * 0.00300;
            push_tier("$1,000,001 – $2,000,000 ($3.00/$1k)", b6, 3.00, s6);

            let b7 = (p - 2_000_000.0).max(0.0);
            let s7 = b7 * 0.00200;
            push_tier("Over $2,000,000 ($2.00/$1k)", b7, 2.00, s7);

            let owner = (s1 + s2 + s3 + s4 + s5 + s6 + s7).round() as i64;
            let lender = if loan > 0.0 { 275 } else { 0 }; // Simultaneous loan + PA 100/300/900 endorsements
            (owner, lender, tiers)
        }
        "OH" => {
            // OTIRB Rate Manual Rule PR-1 (effective Jan 1, 2026)
            let b1 = p.min(250_000.0);
            let s1 = (b1 * 0.00580).max(225.0);
            push_tier("Up to $250,000 ($5.80/$1k)", b1, 5.80, s1);

            let b2 = (p.min(500_000.0) - 250_000.0).max(0.0);
            let s2 = b2 * 0.00410;
            push_tier("$250,001 – $500,000 ($4.10/$1k)", b2, 4.10, s2);

            let b3 = (p.min(1_000_000.0) - 500_000.0).max(0.0);
            let s3 = b3 * 0.00320;
            push_tier("$500,001 – $1,000,000 ($3.20/$1k)", b3, 3.20, s3);

            let b4 = (p.min(5_000_000.0) - 1_000_000.0).max(0.0);
            let s4 = b4 * 0.00310;
            push_tier("$1,000,001 – $5,000,000 ($3.10/$1k)", b4, 3.10, s4);

            let b5 = (p.min(10_000_000.0) - 5_000_000.0).max(0.0);
            let s5 = b5 * 0.00290;
            push_tier("$5,000,001 – $10,000,000 ($2.90/$1k)", b5, 2.90, s5);

            let b6 = (p - 10_000_000.0).max(0.0);
            let s6 = b6 * 0.00260;
            push_tier("Over $10,000,000 ($2.60/$1k)", b6, 2.60, s6);

            let owner = (s1 + s2 + s3 + s4 + s5 + s6).round() as i64;
            let lender = if loan > 0.0 { 200 } else { 0 }; // OTIRB simultaneous $100 + endorsements
            (owner, lender, tiers)
        }
        "NC" => {
            // NCTIRB Schedule (Notably low filed rates)
            let b1 = p.min(100_000.0);
            let s1 = (b1 * 0.00253).max(125.0);
            push_tier("Up to $100,000 ($2.53/$1k)", b1, 2.53, s1);

            let b2 = (p.min(500_000.0) - 100_000.0).max(0.0);
            let s2 = b2 * 0.00197;
            push_tier("$100,001 – $500,000 ($1.97/$1k)", b2, 1.97, s2);

            let b3 = (p.min(2_000_000.0) - 500_000.0).max(0.0);
            let s3 = b3 * 0.00129;
            push_tier("$500,001 – $2,000,000 ($1.29/$1k)", b3, 1.29, s3);

            let b4 = (p - 2_000_000.0).max(0.0);
            let s4 = b4 * 0.00099;
            push_tier("Over $2,000,000 ($0.99/$1k)", b4, 0.99, s4);

            let owner = (s1 + s2 + s3 + s4).round() as i64;
            let lender = if loan > 0.0 { 135 } else { 0 }; // Simultaneous loan $26 + ALTA 8.1/9
            (owner, lender, tiers)
        }
        _ => {
            // Regional state-filed / file-and-use major underwriter residential bracket curve
            let state_factor = match state {
                "WA" | "OR" | "AZ" | "NV" | "UT" | "ID" => 0.94,
                "CO" | "IL" | "MN" | "WI" | "MI" => 0.92,
                "MA" | "CT" | "RI" | "MD" | "DC" | "DE" | "VA" | "HI" | "AK" => 1.06,
                "GA" | "SC" | "TN" | "AL" | "LA" => 0.96,
                _ => 0.95,
            };
            let r1 = 3.80 * state_factor;
            let r2 = 2.85 * state_factor;
            let r3 = 1.95 * state_factor;
            let r4 = 1.40 * state_factor;

            let b1 = p.min(250_000.0);
            let s1 = 325.0 + b1 * (r1 / 1_000.0);
            push_tier("Up to $250,000", b1, r1, s1);

            let b2 = (p.min(1_000_000.0) - 250_000.0).max(0.0);
            let s2 = b2 * (r2 / 1_000.0);
            push_tier("$250,001 – $1,000,000", b2, r2, s2);

            let b3 = (p.min(3_000_000.0) - 1_000_000.0).max(0.0);
            let s3 = b3 * (r3 / 1_000.0);
            push_tier("$1,000,001 – $3,000,000", b3, r3, s3);

            let b4 = (p - 3_000_000.0).max(0.0);
            let s4 = b4 * (r4 / 1_000.0);
            push_tier("Over $3,000,000", b4, r4, s4);

            let owner = (s1 + s2 + s3 + s4).round().max(475.0) as i64;
            let lender = if loan > 0.0 {
                (375.0 + loan * 0.00045).round().clamp(375.0, 1_950.0) as i64
            } else {
                0
            };
            (owner, lender, tiers)
        }
    }
}

fn allocate_who_pays_share(payer: &str, total: i64) -> Option<(i64, i64)> {
    match payer {
        "buyer" | "closing_attorney" | "buyer_attorney_or_title" | "buyer_and_lender_attorneys" => {
            Some((total, 0))
        }
        "seller" => Some((0, total)),
        "split_50_50" => {
            let half = total / 2;
            Some((half, total - half))
        }
        // Ambiguous / county-variable / negotiable customs must not fabricate a 50/50 dollar split
        "varies_by_county" | "negotiable" | "buyer_or_split" | _ => None,
    }
}

/// Public state-filed / statutory title insurance rate calculator usable directly by callers and tests.
pub fn compute_state_title_rate_filing(
    geo: &Value,
    facts: &Value,
    subject_price: Option<f64>,
    as_of: NaiveDate,
) -> Value {
    let (state, _county, county_fips, _city) = resolve_state_and_county(geo, facts);
    let spec = state_title_schedule_spec(&state);
    let who = lookup_who_pays(geo);
    let transfers = extract_sorted_transfers(facts);

    let latest_transfer = transfers.first();
    let (prior_date_str, years_since_prior) = if let Some(tr) = latest_transfer {
        let days = (as_of - tr.date).num_days().max(0);
        (
            Some(tr.date_str.clone()),
            Some(round_n(days as f64 / 365.25, 2)),
        )
    } else {
        (None, None)
    };

    let (reissue_eligible, reissue_credit_pct, reissue_note) = match years_since_prior {
        Some(yrs) if spec.state == "TX" => {
            if yrs <= 7.0 {
                (
                    false,
                    0.0,
                    format!(
                        "Prior transfer recorded {:.1} yrs ago ({}). Under Texas TDI Basic Manual R-1, residential owner's policy rates are fixed without reissue discounts (TDI Rule R-8 refinance credits apply only to renewal loan policies).",
                        yrs,
                        prior_date_str.as_deref().unwrap_or("recent")
                    ),
                )
            } else {
                (
                    false,
                    0.0,
                    format!(
                        "Prior transfer recorded {:.1} yrs ago; Texas TDI Rule R-1 promulgated basic rate applies.",
                        yrs
                    ),
                )
            }
        }
        Some(yrs) if spec.state == "IA" => (
            false,
            0.0,
            format!(
                "Prior transfer recorded {:.1} yrs ago; Iowa Title Guaranty charges a flat statutory $175 certificate up to $750,000 (plus abstract continuation & attorney examination).",
                yrs
            ),
        ),
        Some(yrs) if yrs <= 5.0 && spec.reissue_credit_short_pct > 0.0 => (
            true,
            spec.reissue_credit_short_pct,
            format!(
                "Prior transfer recorded {:.1} yrs ago on {} (within {}-yr short-term/reissue window): eligible for ~{:.0}% reissue rate credit upon providing seller's prior owner's title policy.",
                yrs,
                prior_date_str.as_deref().unwrap_or("record"),
                spec.reissue_window_years,
                spec.reissue_credit_short_pct
            ),
        ),
        Some(yrs)
            if yrs <= (spec.reissue_window_years as f64)
                && spec.reissue_credit_extended_pct > 0.0 =>
        {
            (
                true,
                spec.reissue_credit_extended_pct,
                format!(
                    "Prior transfer recorded {:.1} yrs ago on {} (within {}-yr state reissue window): eligible for ~{:.0}% reissue credit up to the prior policy face amount when prior policy is supplied.",
                    yrs,
                    prior_date_str.as_deref().unwrap_or("record"),
                    spec.reissue_window_years,
                    spec.reissue_credit_extended_pct
                ),
            )
        }
        Some(yrs) => (
            false,
            0.0,
            format!(
                "Last recorded transfer was {:.1} yrs ago on {} (outside {}-yr reissue window); standard rate applies unless seller refinanced with a lender's policy more recently.",
                yrs,
                prior_date_str.as_deref().unwrap_or("record"),
                spec.reissue_window_years
            ),
        ),
        None => (
            false,
            0.0,
            format!(
                "No prior transfer date in parcel records; ask seller/escrow for any prior owner's policy issued within the past {} years to check reissue rate eligibility.",
                spec.reissue_window_years
            ),
        ),
    };

    let valid_price = subject_price.filter(|&p| p.is_finite() && p > 0.0);

    let Some(price) = valid_price else {
        return json!({
            "state": if state.is_empty() { Value::Null } else { json!(state) },
            "stateName": if state.is_empty() { Value::Null } else { json!(spec.state_name) },
            "rateRegime": spec.rate_regime,
            "rateScheduleBasis": spec.rate_schedule_basis,
            "regulator": spec.regulator,
            "citation": spec.citation,
            "filingUrl": spec.filing_url,
            "basisPriceUsd": Value::Null,
            "assumedLoanAmountUsd": Value::Null,
            "ownerStandardUsd": Value::Null,
            "ownerEnhancedUsd": Value::Null,
            "simultaneousLenderUsd": Value::Null,
            "reissueEligible": reissue_eligible,
            "reissueWindowYears": spec.reissue_window_years,
            "priorTransferDate": prior_date_str,
            "yearsSincePriorTransfer": years_since_prior,
            "reissueCreditPct": reissue_credit_pct,
            "reissueSavingsUsd": Value::Null,
            "ownerStandardAfterReissueUsd": Value::Null,
            "ownerEnhancedAfterReissueUsd": Value::Null,
            "reissueNote": reissue_note,
            "escrowSettlementUsd": Value::Null,
            "tierBreakdown": [],
            "whoPaysCustom": {
                "regionKey": who.region_key,
                "regionName": who.region_name,
                "state": who.state,
                "countyFips": who.county_fips,
                "ownersPolicyPayer": who.owners_policy_payer,
                "lendersPolicyPayer": who.lenders_policy_payer,
                "escrowFeePayer": who.escrow_fee_payer,
                "closingModel": who.closing_model,
                "customNote": who.custom_note,
                "whoPaysDeterministic": false,
                "buyerEstimatedTitleEscrowUsd": Value::Null,
                "sellerEstimatedTitleEscrowUsd": Value::Null,
            }
        });
    };

    let loan_80 = (price * 0.80).round();
    let (owner_std, sim_lender, tier_breakdown) =
        calc_bracketed_owner_and_lender(spec.state, &county_fips, price, loan_80);
    let owner_enh = ((owner_std as f64) * spec.enhanced_multiplier).round() as i64;

    // Reissue credit applies up to the prior sale price (if known and lower than current price) or full owner policy
    let reissue_cap_ratio = match latest_transfer.and_then(|t| t.price).filter(|&p| p > 0) {
        Some(prior_p) if (prior_p as f64) < price => ((prior_p as f64) / price).clamp(0.25, 1.0),
        _ => 1.0,
    };
    let reissue_savings = if reissue_eligible && reissue_credit_pct > 0.0 {
        ((owner_std as f64) * reissue_cap_ratio * (reissue_credit_pct / 100.0)).round() as i64
    } else {
        0
    };
    let owner_std_net = (owner_std - reissue_savings).max(0);
    let owner_enh_net = (owner_enh - reissue_savings).max(0);

    // Escrow / settlement scales gently above $1M for higher escrow liability tiers
    let escrow_settlement = (spec.base_settlement_usd as f64
        + ((price - 500_000.0).max(0.0) / 1_000_000.0) * 280.0)
        .round()
        .clamp(550.0, 3_600.0) as i64;

    // Allocate customary Buyer vs. Seller title + escrow dollars honestly from `lookup_who_pays`
    let owner_split = allocate_who_pays_share(who.owners_policy_payer, owner_std_net);
    let lender_split = allocate_who_pays_share(who.lenders_policy_payer, sim_lender);
    let escrow_split = allocate_who_pays_share(who.escrow_fee_payer, escrow_settlement);
    let (who_pays_deterministic, buyer_est_val, seller_est_val) =
        match (owner_split, lender_split, escrow_split) {
            (Some((ob, os)), Some((lb, ls)), Some((eb, es))) => {
                (true, json!(ob + lb + eb), json!(os + ls + es))
            }
            _ => (false, Value::Null, Value::Null),
        };

    json!({
        "state": if state.is_empty() { Value::Null } else { json!(state) },
        "stateName": if state.is_empty() { Value::Null } else { json!(spec.state_name) },
        "rateRegime": spec.rate_regime,
        "rateScheduleBasis": spec.rate_schedule_basis,
        "regulator": spec.regulator,
        "citation": spec.citation,
        "filingUrl": spec.filing_url,
        "basisPriceUsd": price.round() as i64,
        "assumedLoanAmountUsd": loan_80 as i64,
        "ownerStandardUsd": owner_std,
        "ownerEnhancedUsd": owner_enh,
        "simultaneousLenderUsd": sim_lender,
        "reissueEligible": reissue_eligible,
        "reissueWindowYears": spec.reissue_window_years,
        "priorTransferDate": prior_date_str,
        "yearsSincePriorTransfer": years_since_prior,
        "reissueCreditPct": reissue_credit_pct,
        "reissueSavingsUsd": reissue_savings,
        "ownerStandardAfterReissueUsd": owner_std_net,
        "ownerEnhancedAfterReissueUsd": owner_enh_net,
        "reissueNote": reissue_note,
        "escrowSettlementUsd": escrow_settlement,
        "tierBreakdown": tier_breakdown,
        "whoPaysCustom": {
            "regionKey": who.region_key,
            "regionName": who.region_name,
            "state": who.state,
            "countyFips": who.county_fips,
            "ownersPolicyPayer": who.owners_policy_payer,
            "lendersPolicyPayer": who.lenders_policy_payer,
            "escrowFeePayer": who.escrow_fee_payer,
            "closingModel": who.closing_model,
            "customNote": who.custom_note,
            "whoPaysDeterministic": who_pays_deterministic,
            "buyerEstimatedTitleEscrowUsd": buyer_est_val,
            "sellerEstimatedTitleEscrowUsd": seller_est_val,
        }
    })
}

// =================================================================================================
// 2. Super-Priority HOA Lien Rules & Recorded Liens / Encumbrances (`recordedLiensAndEncumbrances`)
// =================================================================================================

fn lookup_hoa_super_priority(state: &str, hoa_profile: &Value) -> Value {
    if let Some(sp) = hoa_profile
        .get("superPriorityLien")
        .filter(|v| v.is_object())
    {
        return sp.clone();
    }
    let st = state.trim().to_ascii_uppercase();
    let (is_sp, months, statute, note) = match st.as_str() {
        "NV" => (true, Some(9), "NRS 116.3116", "Nevada grants HOAs a true 9-month super-priority assessment lien ahead of first Deeds of Trust; foreclosure of the super-priority portion can extinguish a first mortgage if unpaid."),
        "CO" => (true, Some(6), "C.R.S. § 38-33.3-316", "Colorado CCIOA grants associations a 6-month super-priority lien over first mortgages."),
        "WA" => (true, Some(6), "RCW 64.34.364 / RCW 64.90.485", "Washington WUCIOA / Condo Act provides a 6-month assessment super-priority lien ahead of first mortgages."),
        "OR" => (true, None, "ORS 100.450 / ORS 94.709", "Oregon statutory assessment liens hold priority over certain encumbrances subject to lender notice rules."),
        "FL" => (true, Some(12), "Fla. Stat. §§ 718.116 & 720.3085", "Florida Condo/HOA statutes impose joint-and-several buyer liability for seller arrears; first mortgagees taking title owe lesser of 12 months dues or 1% of original mortgage."),
        "IL" => (true, Some(6), "765 ILCS 605/9(g)", "Illinois Condominium Property Act grants up to 6 months unpaid common-expense priority upon judicial foreclosure resale."),
        "MA" => (true, Some(6), "M.G.L. c. 183A § 6(c)", "Massachusetts rolling 6-month condo common-expense super-priority lien over first mortgages."),
        "NJ" => (true, Some(6), "N.J.S.A. 46:8B-21", "New Jersey grants condo/HOA associations a 6-month limited super-priority lien."),
        "PA" => (true, Some(6), "68 Pa.C.S. §§ 3315 & 5315", "Pennsylvania Uniform Condominium / Planned Community Acts provide a 6-month limited super-priority lien."),
        "CT" => (true, Some(9), "Conn. Gen. Stat. § 47-258", "Connecticut CIOA provides a 9-month common-expense super-priority lien ahead of first mortgages."),
        "MD" => (true, Some(4), "Md. Code, Real Prop. §§ 11-110 & 11B-117", "Maryland grants up to 4 months (capped at $1,200) priority for unpaid regular assessments."),
        "DC" => (true, Some(6), "D.C. Code § 42-1903.13", "D.C. Condominium Act grants a 6-month super-priority assessment lien; foreclosure can prime first mortgages."),
        "DE" | "NH" | "RI" | "VT" | "HI" | "AK" | "WV" | "MN" | "AL" | "TN" | "WY" => (
            true,
            Some(6),
            "State UCIOA / Condo Super-Priority Statute",
            "State statutory condo/HOA assessment super-priority applies (commonly 6 months of regular assessments ahead of first mortgages).",
        ),
        "CA" => (
            false,
            None,
            "CA Civ. Code §§ 5675 & 5720 (Davis-Stirling Act)",
            "California HOA Notice of Delinquent Assessment (NODA) liens are junior to prior-recorded first Deeds of Trust, though associations may foreclose once principal arrears reach $1,800 or 12+ months delinquent.",
        ),
        "TX" => (
            false,
            None,
            "TX Prop. Code § 209.009",
            "Texas POA assessment liens are generally subordinate to vendor's liens and first-lien mortgages recorded prior to delinquency, but judicial foreclosure remains possible for unpaid dues.",
        ),
        _ => (
            false,
            None,
            "State Condominium / Homeowners Association Lien Act",
            "Verify HOA/condo assessment lien priority and obtain a paid-current HOA estoppel / demand statement in escrow.",
        ),
    };
    json!({
        "isSuperPriorityState": is_sp,
        "monthsPriority": months,
        "statute": statute,
        "note": note,
    })
}

fn push_lien_flag(
    flags: &mut Vec<Value>,
    seen: &mut HashSet<String>,
    code: &str,
    severity: &str,
    category: &str,
    title: &str,
    detail: String,
    source: &str,
    recommended_action: &str,
) {
    if seen.insert(code.to_string()) {
        let clean_detail = scrub_competitor_brands(&redact_title_pii(&detail).masked_text);
        flags.push(json!({
            "code": code,
            "severity": severity,
            "category": category,
            "title": title,
            "detail": clean_detail,
            "source": source,
            "recommendedAction": recommended_action,
        }));
    }
}

#[derive(Debug, Default, Clone)]
struct LiveRecorderOutcome {
    checked: bool,
    /// True when open-data API only covers building violations or assessor roll rather than a recorded lien index.
    non_lien_proxy: bool,
    source_name: Option<String>,
    source_url: Option<String>,
    records: Vec<Value>,
}

fn build_recorded_liens_section(
    report: &Value,
    geo: &Value,
    facts: &Value,
    hoa_profile: &Value,
    live_recorder: Option<&LiveRecorderOutcome>,
) -> Value {
    let (state, county, _county_fips, _city) = resolve_state_and_county(geo, facts);
    let title_review = report
        .get("titleReview")
        .or_else(|| report.pointer("/modules/titleReview"))
        .map(unwrap_data)
        .filter(|v| v.is_object());

    let prelim_uploaded = title_review.is_some();
    let live_checked = live_recorder.map(|r| r.checked).unwrap_or(false);
    let live_non_lien_proxy = !live_checked
        && live_recorder.map(|r| r.non_lien_proxy).unwrap_or(false);

    let coverage_status = if prelim_uploaded {
        "prelim_uploaded"
    } else if live_checked {
        "live_open_recorder_checked"
    } else if live_non_lien_proxy {
        "assessor_or_violations_only"
    } else {
        "assessor_and_disclosures_only"
    };
    let requires_prelim_verification = !prelim_uploaded;

    // Recorder & Tax Collector official links
    let rec_links = facts.pointer("/titleAndLiens/recorderLinks");
    let recorder_name = val_str(rec_links.and_then(|r| r.get("recorderName")))
        .map(String::from)
        .unwrap_or_else(|| {
            if !county.is_empty() && !state.is_empty() {
                format!("{county}, {state} Clerk / Recorder")
            } else if !state.is_empty() {
                format!("{state} County Clerk / Recorder")
            } else {
                "County Clerk / Recorder".to_string()
            }
        });
    let recorder_url = val_str(rec_links.and_then(|r| r.get("recorderUrl")))
        .map(String::from)
        .or_else(|| live_recorder.and_then(|r| r.source_url.clone()));
    let tax_collector_name =
        val_str(rec_links.and_then(|r| r.get("taxCollectorName"))).map(String::from);
    let tax_collector_url =
        val_str(rec_links.and_then(|r| r.get("taxCollectorUrl"))).map(String::from);

    let mut flags: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. Uploaded Preliminary Title Report (Prelim) extraction
    let prelim_summary = if let Some(tr) = title_review {
        if let Some(arr) = tr.get("involuntaryLiens").and_then(|v| v.as_array()) {
            for (idx, lien) in arr.iter().enumerate() {
                let status = val_str(lien.get("status")).unwrap_or("active");
                if status == "active" {
                    let ltype = val_str(lien.get("type")).unwrap_or("Involuntary Lien");
                    let sev = val_str(lien.get("severity")).unwrap_or("critical");
                    let amt_note = val_i64(lien.get("amount"))
                        .map(|a| format!(" (${a})"))
                        .unwrap_or_default();
                    let dt_note = val_str(lien.get("date"))
                        .map(|d| format!(" recorded {d}"))
                        .unwrap_or_default();
                    push_lien_flag(
                        &mut flags,
                        &mut seen,
                        &format!("PRELIM_ACTIVE_LIEN_{idx}"),
                        sev,
                        "involuntary_lien",
                        ltype,
                        format!("Active Schedule B {ltype}{amt_note}{dt_note} on uploaded Preliminary Title Report."),
                        "Uploaded Preliminary Title Report (Schedule B)",
                        "Demand written payoff statement and record full release/reconveyance at or before close of escrow.",
                    );
                }
            }
        }
        if let Some(arr) = tr.get("easementsAndExceptions").and_then(|v| v.as_array()) {
            for (idx, exc) in arr.iter().enumerate() {
                let etype = val_str(exc.get("type")).unwrap_or("Schedule B Exception");
                let lc = etype.to_ascii_lowercase();
                let sev = if lc.contains("solar") || lc.contains("ucc") || lc.contains("encroach") {
                    "major"
                } else {
                    "moderate"
                };
                push_lien_flag(
                    &mut flags,
                    &mut seen,
                    &format!("PRELIM_EXCEPTION_{idx}"),
                    sev,
                    "schedule_b_exception",
                    etype,
                    format!("Schedule B exception identified on uploaded Prelim: {etype}."),
                    "Uploaded Preliminary Title Report (Schedule B)",
                    "Pull underlying recorded instrument and plot corridors/restrictions before removing title contingency.",
                );
            }
        }
        Some(json!({
            "effectiveDate": tr.get("effectiveDate").cloned().unwrap_or(Value::Null),
            "titleCompany": tr.get("titleCompany").cloned().unwrap_or(Value::Null),
            "overallStatus": tr.get("overallStatus").cloned().unwrap_or(Value::Null),
            "activeMortgages": tr.pointer("/counts/activeMortgages").cloned().unwrap_or(json!(0)),
            "activeInvoluntaryLiens": tr.pointer("/counts/activeInvoluntaryLiens").cloned().unwrap_or(json!(0)),
            "easementsAndExceptions": tr.pointer("/counts/easementsAndExceptions").cloned().unwrap_or(json!(0)),
        }))
    } else {
        None
    };

    // 2. Live Open-Data Recorder / BYOK ATTOM records
    let mut live_records: Vec<Value> = Vec::new();
    if let Some(lr) = live_recorder {
        live_records = lr.records.clone();
        for (idx, rec) in lr.records.iter().enumerate() {
            let doc_type = val_str(rec.get("documentType")).unwrap_or("Recorded Instrument");
            let dt = val_str(rec.get("recordedDate")).unwrap_or("date unindexed");
            let lc = doc_type.to_ascii_lowercase();
            if lc.contains("lien")
                || lc.contains("ucc")
                || lc.contains("lis pendens")
                || lc.contains("judgment")
                || lc.contains("default")
                || lc.contains("violation")
            {
                push_lien_flag(
                    &mut flags,
                    &mut seen,
                    &format!("LIVE_RECORDER_HIT_{idx}"),
                    "major",
                    "open_recorder_index",
                    doc_type,
                    format!("Open county/municipal recorder index lists `{doc_type}` ({dt}). Verify whether released or satisfied on official Prelim."),
                    lr.source_name.as_deref().unwrap_or("Open County Recorder API"),
                    "Order full instrument image and confirm recorded satisfaction/release in escrow.",
                );
            }
        }
    }

    // 3. Public Assessor / TitleAndLiens / Disclosures synthesis
    if let Some(arr) = facts
        .pointer("/titleAndLiens/involuntaryLiens")
        .and_then(|v| v.as_array())
    {
        for (idx, item) in arr.iter().enumerate() {
            let desc = val_str(item.get("type"))
                .or_else(|| val_str(item.get("description")))
                .or_else(|| item.as_str())
                .unwrap_or("Recorded Lien Notice");
            push_lien_flag(
                &mut flags,
                &mut seen,
                &format!("PUBLIC_LIEN_{idx}"),
                "critical",
                "public_record_lien",
                desc,
                format!("Public parcel records list involuntary lien item: {desc}."),
                "Parcel Title & Lien Summary",
                "Require formal payoff demand and escrow release prior to closing.",
            );
        }
    }

    if let Some(arr) = facts
        .pointer("/titleAndLiens/easementsAndRestrictions")
        .and_then(|v| v.as_array())
    {
        for (idx, item) in arr.iter().enumerate() {
            let desc = val_str(item.get("type"))
                .or_else(|| val_str(item.get("description")))
                .or_else(|| item.as_str())
                .unwrap_or("Easement / Covenant");
            push_lien_flag(
                &mut flags,
                &mut seen,
                &format!("PUBLIC_EASEMENT_{idx}"),
                "moderate",
                "easement_or_covenant",
                desc,
                format!("Parcel records note easement or restriction: {desc}."),
                "Parcel Title & Lien Summary",
                "Review Schedule B plotted easements before planning additions, pools, or ADUs.",
            );
        }
    }

    // Gather non-PII structured disclosure/feature text for PACE, Solar UCC-1, and Tax Delinquency checks
    let mut screening_parts: Vec<String> = Vec::new();
    if let Some(arr) = facts.get("features").and_then(|v| v.as_array()) {
        for f in arr {
            if let Some(s) = f.as_str() {
                screening_parts.push(s.to_string());
            }
        }
    }
    if let Some(s) = val_str(facts.get("renovations")) {
        screening_parts.push(s.to_string());
    }
    if let Some(s) = val_str(facts.get("taxNote")) {
        screening_parts.push(s.to_string());
    }
    if let Some(arr) = facts
        .pointer("/disclosures/statutoryChecklist")
        .and_then(|v| v.as_array())
    {
        for item in arr {
            if let Some(k) = val_str(item.get("key")) {
                screening_parts.push(k.to_string());
            }
            if let Some(d) = val_str(item.get("detail")) {
                screening_parts.push(d.to_string());
            }
        }
    }
    if let Some(arr) = facts
        .pointer("/disclosures/mlsDisclosures/disclosures")
        .and_then(|v| v.as_array())
    {
        for item in arr {
            if let Some(s) = item.as_str() {
                screening_parts.push(s.to_string());
            }
        }
    }
    let screening_blob = screening_parts.join(" | ");

    // PACE / HERO Super-Priority Assessment Check
    let pace_flag = RE_PACE_HERO.is_match(&screening_blob)
        || flags.iter().any(|f| {
            val_str(f.get("title"))
                .map(|t| RE_PACE_HERO.is_match(t))
                .unwrap_or(false)
        });
    if pace_flag {
        push_lien_flag(
            &mut flags,
            &mut seen,
            "PACE_HERO_SUPER_PRIORITY",
            "critical",
            "super_priority_tax_assessment",
            "PACE / HERO Clean-Energy Super-Priority Assessment",
            "Records reference a PACE/HERO property-tax energy assessment. Under FHFA / Fannie Mae / Freddie Mac / FHA rules, first-lien super-priority PACE assessments block standard mortgage underwriting unless paid off in full at closing.".to_string(),
            "Tax Roll & Disclosure Screening",
            "Obtain PACE program payoff statement and negotiate 100% seller payoff through escrow.",
        );
    }

    // Solar PV Ownership vs. Lease / PPA / UCC-1 Fixture Filing Check
    let prelim_has_ucc = title_review
        .and_then(|tr| tr.get("easementsAndExceptions"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|e| {
                let t = val_str(e.get("type")).unwrap_or("").to_ascii_lowercase();
                t.contains("solar") || t.contains("ucc")
            })
        })
        .unwrap_or(false);
    let prop_feat_solar = report
        .pointer("/propertyFeatures/hasSolar")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_solar = prelim_has_ucc || prop_feat_solar || RE_SOLAR_PRESENT.is_match(&screening_blob);
    let solar_status = if prelim_has_ucc {
        "ucc1_recorded_on_prelim"
    } else if has_solar {
        if RE_SOLAR_OWNED.is_match(&screening_blob) && !RE_SOLAR_UCC_LEASE.is_match(&screening_blob)
        {
            "owned_confirmed"
        } else {
            "installed_verify_ownership"
        }
    } else {
        "none_detected"
    };

    if solar_status == "ucc1_recorded_on_prelim" || solar_status == "installed_verify_ownership" {
        push_lien_flag(
            &mut flags,
            &mut seen,
            "SOLAR_UCC1_OWNERSHIP_CHECK",
            if solar_status == "ucc1_recorded_on_prelim" { "major" } else { "watch" },
            "ucc1_fixture_filing",
            "Solar PV Array — Verify Free-and-Clear Ownership vs. UCC-1 / PPA Lease",
            "Solar photovoltaic panels are installed on the property. Leased, PPA, or financed solar arrays frequently carry a recorded County UCC-1 Financing Statement fixture filing that clouds title and requires lender DTI inclusion + formal lease transfer or seller buyout.".to_string(),
            "Solar & UCC-1 Fixture Screening",
            "Request solar purchase receipt or PPA/lease contract, utility NEM interconnection letter, and check Prelim Schedule B for UCC-1 filings.",
        );
    }

    // HOA Super-Priority State Check
    let super_priority_hoa = lookup_hoa_super_priority(&state, hoa_profile);
    let hoa_monthly = val_f64(facts.get("hoaMonthly")).unwrap_or(0.0);
    let has_hoa = hoa_monthly > 0.0
        || hoa_profile
            .get("hasHoa")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    if has_hoa
        && super_priority_hoa
            .get("isSuperPriorityState")
            .and_then(|v| v.as_bool())
            == Some(true)
    {
        let statute = val_str(super_priority_hoa.get("statute")).unwrap_or("State HOA Act");
        push_lien_flag(
            &mut flags,
            &mut seen,
            "HOA_SUPER_PRIORITY_JURISDICTION",
            "watch",
            "hoa_super_priority",
            "HOA Super-Priority Assessment Lien State",
            format!("Property has HOA/condo governance in a super-priority lien state ({statute}). Unpaid seller HOA assessments or special assessments can prime mortgage liens or transfer to the buyer."),
            statute,
            "Require an official Seller-Paid HOA Resale Certificate / Estoppel Demand confirming $0.00 delinquent balance at closing.",
        );
    }

    // Trust / Probate Vesting Check (zero PII)
    if let Some(vest) = val_str(facts.pointer("/titleAndLiens/vesting")) {
        let redacted_vest = redact_title_pii(vest).masked_text;
        if redacted_vest.to_ascii_lowercase().contains("trust") {
            push_lien_flag(
                &mut flags,
                &mut seen,
                "TRUST_VESTING_CERTIFICATION",
                "info",
                "vesting_authority",
                "Trust Vesting — Trustee Authority & Disclosure Rule Check",
                format!("Parcel vesting reflects `{redacted_vest}`. Escrow will require a Trust Certification confirming active trustee signing authority (and in CA under Civ. Code § 1102.2(d), an owner-occupant trustee must still deliver a full TDS)."),
                "Assessor Vesting Profile",
                "Have seller deliver Certification of Trust early to escrow to avoid signing delays.",
            );
        }
    }

    // Honest coverage summary — NEVER claims "0 liens on title" without an uploaded Prelim!
    let coverage_summary = match coverage_status {
        "prelim_uploaded" => format!(
            "Uploaded Preliminary Title Report parsed ({} Schedule B flag(s) surfaced). Confirm updated date-down endorsement immediately prior to recording.",
            flags.len()
        ),
        "live_open_recorder_checked" => format!(
            "Queried {} ({} indexed instrument(s) returned; {} title/encumbrance watchout(s) flagged). Open clerk indexes do not substitute for a full title plant search — order an official Preliminary Title Report (Prelim) in escrow to verify unindexed mechanic's liens, judgments, and Schedule B easements.",
            live_recorder
                .and_then(|r| r.source_name.as_deref())
                .unwrap_or("open county recorder API"),
            live_records.len(),
            flags.len()
        ),
        "assessor_or_violations_only" => format!(
            "Queried {} ({} municipal violation / assessor roll record(s)), which is NOT a county grantor-grantee lien index. {} watchout(s) flagged from disclosures and assessor data; order an official Preliminary Title Report (Prelim) in escrow to verify recorded mortgages, tax liens, mechanic's liens, and judgments.",
            live_recorder
                .and_then(|r| r.source_name.as_deref())
                .unwrap_or("municipal open data"),
            live_records.len(),
            flags.len()
        ),
        _ => format!(
            "County grantor-grantee / lien index for {} is not exposed via keyless public API and no Preliminary Title Report (Prelim) has been uploaded. Public assessor & disclosure screening surfaced {} watchout(s), but CANNOT confirm absence of recorded mortgages, tax liens, mechanic's liens, or judgments — Prelim verification is required.",
            recorder_name,
            flags.len()
        ),
    };

    let buyer_guidance = if prelim_uploaded {
        "Review every Schedule B exception hyperlinked PDF with your escrow/title officer before removing title contingency, and select an ALTA Homeowner's Policy where available for post-closing forgery and encroachment protection.".to_string()
    } else {
        format!(
            "Request the seller's Preliminary Title Report (Prelim) from escrow on Day 1, verify payoff demands on all existing loans/UCC-1s/assessments, and enroll in {recorder_name}'s free recording alert after closing."
        )
    };
    let seller_guidance = "Order a pre-listing Preliminary Title Report to clear stale reconveyances, old HELOCs (which must be frozen and closed in writing, not merely paid to $0), solar UCC-1 terminations, or trust certifications before going active.".to_string();
    let agent_guidance = "Verify vesting signatory match (all trustees, spouses, or LLC managers), solar ownership vs. PPA transfer timeline (10–21 days), and municipal/HOA demand turnaround so Schedule B clearance never delays funding.".to_string();

    json!({
        "coverageStatus": coverage_status,
        "requiresPrelimVerification": requires_prelim_verification,
        "verifiedZeroLiens": false,
        "coverageSummary": coverage_summary,
        "prelimSummary": prelim_summary,
        "liveRecorderSource": live_recorder.and_then(|r| r.source_name.clone()),
        "liveRecorderRecords": live_records,
        "superPriorityHoaRule": super_priority_hoa,
        "paceAssessmentFlag": pace_flag,
        "solarOwnershipStatus": solar_status,
        "recorderPortal": {
            "name": recorder_name,
            "url": recorder_url,
            "taxCollectorName": tax_collector_name,
            "taxCollectorUrl": tax_collector_url,
        },
        "flags": flags,
        "buyerActionGuidance": buyer_guidance,
        "sellerActionGuidance": seller_guidance,
        "agentActionGuidance": agent_guidance,
    })
}

// =================================================================================================
// 3. Pre-Foreclosure, Tax-Default & Distress Radar (`foreclosureAndDistress`)
// =================================================================================================

fn statutory_foreclosure_timeline_for_state(state: &str) -> (&'static str, &'static str, String) {
    match state.trim().to_ascii_uppercase().as_str() {
        "CA" => (
            "non_judicial",
            "CA Civ. Code §§ 2924–2924k & SB 1079 (§ 2924m)",
            "Non-judicial Deed of Trust state (CA Civ. Code § 2924): minimum 90-day Notice of Default (NOD) reinstatement period + minimum 21-day Notice of Trustee's Sale (NTS) publication/posting (~111+ days statutory floor; borrower right to reinstate up to 5 business days before sale under § 2924c; SB 1079 45-day post-auction eligible owner-occupant/tenant bid window).".to_string(),
        ),
        "TX" => (
            "non_judicial",
            "TX Prop. Code § 51.002",
            "Fast-track non-judicial Deed of Trust state (TX Prop. Code § 51.002): minimum 20-day written default cure notice on homesteads + minimum 21-day Notice of Sale prior to 'First Tuesday' county courthouse auction (~41+ days; no statutory post-sale mortgage redemption, 2-yr tax-sale homestead redemption).".to_string(),
        ),
        "FL" => (
            "judicial",
            "Fla. Stat. ch. 702 & § 45.0315",
            "Mandatory judicial foreclosure state (Fla. Stat. ch. 702): lender must file circuit-court lawsuit & record Lis Pendens, obtain Final Judgment of Foreclosure, and complete Clerk auction (~180–365+ days; borrower right of redemption ends upon issuance of Clerk's Certificate of Sale under § 45.0315).".to_string(),
        ),
        "NY" => (
            "judicial",
            "NY RPAPL §§ 1301–1391 & CPLR § 3408",
            "Mandatory judicial foreclosure state (NY RPAPL § 1304): mandatory 90-day pre-foreclosure notice, Lis Pendens + Supreme Court summons, mandatory CPLR § 3408 settlement conferences, referee computation, and Judgment of Foreclosure & Sale (~12–24+ months).".to_string(),
        ),
        "IL" => (
            "judicial",
            "735 ILCS 5/15-1101 (Illinois Mortgage Foreclosure Law)",
            "Mandatory judicial foreclosure state (735 ILCS 5/15-1603): circuit-court complaint + statutory 7-month residential redemption period from service of summons (or 3 months from judgment, whichever is later) prior to judicial sale.".to_string(),
        ),
        "NJ" => (
            "judicial",
            "N.J.S.A. 2A:50-53 (NJ Fair Foreclosure Act)",
            "Mandatory judicial foreclosure state (N.J.S.A. 2A:50-56): 30-day Notice of Intention to Foreclose, Chancery Division complaint, final judgment, and Sheriff's Sale (with two statutory 30-day borrower adjournments + 10-day post-sale redemption window).".to_string(),
        ),
        "PA" => (
            "judicial",
            "41 P.S. § 403 (Act 6) & 35 P.S. § 1680.403c (Act 91)",
            "Mandatory judicial foreclosure state: 30-day Act 6/Act 91 pre-foreclosure HEMAP notice followed by Court of Common Pleas action in mortgage foreclosure and County Sheriff's Sale (~9–15 months; no post-Sheriff's-sale mortgage redemption).".to_string(),
        ),
        "WA" => (
            "non_judicial",
            "RCW ch. 61.24 (Washington Deeds of Trust Act)",
            "Non-judicial Deed of Trust state (RCW 61.24.030/.040): 30-day Notice of Default + minimum 120-day Notice of Trustee's Sale (at least 190 days from date of default; borrower right to cure/reinstate up to 11 days before auction under RCW 61.24.090).".to_string(),
        ),
        "OR" => (
            "non_judicial",
            "ORS 86.705–86.815",
            "Non-judicial Trust Deed state (ORS 86.752): minimum 120-day Notice of Sale + mandatory resolution conference notice where applicable; borrower right to cure up to 5 days prior to trustee's sale.".to_string(),
        ),
        "AZ" => (
            "non_judicial",
            "A.R.S. §§ 33-807 & 33-813",
            "Non-judicial Deed of Trust state (A.R.S. § 33-807): minimum 91-day Notice of Trustee's Sale window; borrower may reinstate up to 5:00 p.m. on the last business day before the sale date (anti-deficiency protection on qualifying <=2.5-acre 1–2 family homes under § 33-814(G)).".to_string(),
        ),
        "NV" => (
            "non_judicial",
            "NRS 107.080 & NRS 107.086 (Home Means Nevada Mediation)",
            "Non-judicial Deed of Trust state (NRS 107.080): 90-day Notice of Default & Election to Sell (with mandatory foreclosure mediation opt-in right on owner-occupied homes) + 21-day Notice of Trustee's Sale.".to_string(),
        ),
        "CO" => (
            "non_judicial",
            "C.R.S. §§ 38-38-101 to 38-38-110",
            "Public Trustee non-judicial foreclosure system (C.R.S. § 38-38-101): lender files Notice of Election and Demand (NED) with the County Public Trustee; residential sale scheduled 110–125 days after NED recording; borrower cure statement due >=15 days pre-sale.".to_string(),
        ),
        "GA" => (
            "non_judicial",
            "O.C.G.A. §§ 44-14-162 to 44-14-162.4",
            "Fast-track non-judicial Security Deed state (O.C.G.A. § 44-14-162.2): 30-day written notice to debtor + 4 consecutive weekly legal organ advertisements prior to 'First Tuesday' courthouse sale (~37–60 days).".to_string(),
        ),
        "NC" => (
            "non_judicial",
            "N.C.G.S. ch. 45, art. 2A",
            "Quasi-judicial Clerk of Superior Court power-of-sale state (N.C.G.S. § 45-21.16): 45-day pre-filing home-loan notice + Clerk hearing + 20-day Notice of Sale + statutory 10-day 'upset bid' period after auction.".to_string(),
        ),
        "VA" => (
            "non_judicial",
            "Va. Code § 55.1-321",
            "Fast-track non-judicial Deed of Trust state (Va. Code § 55.1-321): 60-day notice on owner-occupied primary residences + mandatory newspaper advertisement prior to trustee's auction (no post-sale statutory redemption).".to_string(),
        ),
        "MA" => (
            "non_judicial",
            "M.G.L. c. 244 §§ 14 & 35A",
            "Statutory power-of-sale + Land Court Servicemembers proceeding (M.G.L. c. 244 § 35A): 90/150-day Right-to-Cure notice + 21-day weekly publication prior to auction.".to_string(),
        ),
        st if matches!(
            st,
            "CT" | "DE" | "HI" | "IN" | "IA" | "KS" | "KY" | "LA" | "ME" | "ND" | "OH" | "OK"
                | "SC" | "VT" | "WI"
        ) =>
        {
            (
                "judicial",
                "State Judicial Mortgage Foreclosure Statute",
                format!(
                    "{st} is primarily a judicial foreclosure jurisdiction requiring a state court complaint, Lis Pendens, judgment of foreclosure, and Sheriff/Clerk auction (~6–18 months depending on county docket and statutory redemption rules)."
                ),
            )
        }
        other => (
            "non_judicial",
            "State Power-of-Sale / Deed of Trust Foreclosure Statute",
            format!(
                "{} permits non-judicial power-of-sale Deed of Trust / mortgage foreclosure subject to statutory default cure and publication windows; verify county recorder NOD/NTS notices and any tax-sale redemption rights.",
                if other.is_empty() { "Jurisdiction" } else { other }
            ),
        ),
    }
}

fn build_foreclosure_and_distress_section(
    report: &Value,
    geo: &Value,
    facts: &Value,
    valuation: &Value,
    as_of: NaiveDate,
    live_distress: Option<&LiveRecorderOutcome>,
) -> Value {
    let (state, _county, _county_fips, _city) = resolve_state_and_county(geo, facts);
    let (track, stat_cite, stat_note) = statutory_foreclosure_timeline_for_state(&state);

    let title_review = report
        .get("titleReview")
        .or_else(|| report.pointer("/modules/titleReview"))
        .map(unwrap_data)
        .filter(|v| v.is_object());

    let coverage_status = if title_review.is_some() {
        "prelim_uploaded"
    } else if live_distress.map(|d| d.checked).unwrap_or(false) {
        "live_open_distress_checked"
    } else {
        "assessor_and_disclosures_only"
    };

    let fc = facts.get("foreclosure").unwrap_or(&Value::Null);
    let fc_status_str = val_str(fc.get("status")).unwrap_or("").to_ascii_lowercase();
    let fc_is_distressed = fc
        .get("isDistressed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Check Prelim & live feeds for active NOD / NTS / Lis Pendens / Tax Lien
    let prelim_nod_nts = title_review
        .and_then(|tr| tr.get("involuntaryLiens"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|l| {
                val_str(l.get("status")) == Some("active")
                    && val_str(l.get("type"))
                        .map(|t| RE_ACTIVE_NOD_NTS.is_match(t))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    let prelim_tax_delinq = title_review
        .and_then(|tr| tr.get("involuntaryLiens"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|l| {
                val_str(l.get("status")) == Some("active")
                    && val_str(l.get("type"))
                        .map(|t| RE_TAX_DELINQUENT.is_match(t))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    let live_has_nod_or_lp = live_distress
        .map(|ld| {
            ld.records.iter().any(|r| {
                let dt = val_str(r.get("documentType")).unwrap_or("");
                RE_ACTIVE_NOD_NTS.is_match(dt)
            })
        })
        .unwrap_or(false);

    let live_has_tax_lien = live_distress
        .map(|ld| {
            ld.records.iter().any(|r| {
                let dt = val_str(r.get("documentType")).unwrap_or("");
                RE_TAX_DELINQUENT.is_match(dt) || dt.to_ascii_lowercase().contains("tax lien")
            })
        })
        .unwrap_or(false);

    let roll_tax_delinq = facts
        .get("taxHistory")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|t| {
                t.get("delinquent").and_then(|v| v.as_bool()) == Some(true)
                    || t.get("taxDefaulted").and_then(|v| v.as_bool()) == Some(true)
            })
        })
        .unwrap_or(false)
        || val_str(facts.get("taxNote"))
            .map(|n| RE_TAX_DELINQUENT.is_match(n))
            .unwrap_or(false);

    let transfers = extract_sorted_transfers(facts);
    let mut historical_distress: Vec<Value> = Vec::new();
    let mut quitclaim_in_chain = false;
    for tr in &transfers {
        let combined = format!("{} {}", tr.deed_type, tr.note);
        if RE_QUITCLAIM.is_match(&combined) {
            quitclaim_in_chain = true;
        }
        if RE_DISTRESSED_DEED.is_match(&combined) {
            historical_distress.push(json!({
                "date": tr.date_str,
                "deedType": tr.deed_type,
                "priceUsd": tr.price,
            }));
        }
    }
    if let Some(top_deed) = val_str(facts.pointer("/titleAndLiens/deedType")) {
        if RE_QUITCLAIM.is_match(top_deed) {
            quitclaim_in_chain = true;
        }
    }

    let active_nod_or_nts = prelim_nod_nts
        || live_has_nod_or_lp
        || fc_is_distressed
        || RE_ACTIVE_NOD_NTS.is_match(&fc_status_str)
        || fc_status_str.contains("nod")
        || fc_status_str.contains("nts")
        || fc_status_str.contains("auction");

    let tax_delinquent = prelim_tax_delinq
        || live_has_tax_lien
        || roll_tax_delinq
        || RE_TAX_DELINQUENT.is_match(&fc_status_str);

    let prior_reo = !historical_distress.is_empty() || fc_status_str.contains("reo");

    let distress_stage = if active_nod_or_nts {
        "active_nod_or_nts"
    } else if tax_delinquent {
        "tax_delinquent"
    } else if prior_reo {
        "prior_reo_or_trustee_deed"
    } else {
        "none_in_public_records"
    };

    let active_filing = if active_nod_or_nts || tax_delinquent {
        Some(json!({
            "filingType": val_str(fc.get("status")).unwrap_or(if active_nod_or_nts { "Pre-Foreclosure NOD / NTS / Lis Pendens" } else { "Delinquent Tax / Redemption Notice" }),
            "filingDate": fc.get("filingDate").cloned().unwrap_or(Value::Null),
            "auctionDate": fc.get("auctionDate").cloned().unwrap_or(Value::Null),
            "openingBidUsd": val_i64(fc.get("openingBid")),
            "defaultAmountUsd": val_i64(fc.get("defaultAmount")),
            "caseNumber": fc.get("caseNumber").cloned().unwrap_or(Value::Null),
        }))
    } else {
        None
    };

    // FHA 90 / 180-Day Anti-Flipping Status (24 CFR 203.37a)
    let subject_price = resolve_subject_price(facts, valuation);
    let fha_anti_flipping = if let Some(latest) = transfers.first() {
        let hold_days = (as_of - latest.date).num_days();
        let markup_pct = match (latest.price.filter(|&p| p > 0), subject_price) {
            (Some(acq), Some(cur)) => Some(round_n(((cur - acq as f64) / acq as f64) * 100.0, 1)),
            _ => None,
        };
        if (0..=90).contains(&hold_days) {
            json!({
                "status": "ineligible_under_90d",
                "rule": "24 CFR § 203.37a(b)(2)",
                "fhaEligible": false,
                "secondAppraisalRequired": false,
                "sellerHoldDays": hold_days,
                "resaleMarkupPct": markup_pct,
                "note": format!("Seller acquired parcel {} days ago ({}). FHA financing is ineligible until day 91 unless an estate, HUD/GSE REO, or relocation exemption applies.", hold_days, latest.date_str),
            })
        } else if (91..=180).contains(&hold_days) {
            let needs_second = markup_pct.map(|m| m >= 100.0).unwrap_or(false);
            json!({
                "status": if needs_second { "second_appraisal_91_180d" } else { "watch_91_180d" },
                "rule": "24 CFR § 203.37a(b)(3)",
                "fhaEligible": true,
                "secondAppraisalRequired": needs_second,
                "sellerHoldDays": hold_days,
                "resaleMarkupPct": markup_pct,
                "note": if needs_second {
                    format!("Seller hold is {} days with +{:.1}% markup (>=100%): FHA requires a second independent appraisal at lender/seller expense.", hold_days, markup_pct.unwrap_or(100.0))
                } else {
                    format!("Seller hold is {} days (91–180d window); FHA eligible, subject to second-appraisal check if contract price >= 200% of seller's acquisition basis.", hold_days)
                },
            })
        } else {
            json!({
                "status": "eligible_over_180d",
                "rule": "24 CFR § 203.37a",
                "fhaEligible": true,
                "secondAppraisalRequired": false,
                "sellerHoldDays": hold_days.max(0),
                "resaleMarkupPct": markup_pct,
                "note": format!("Last recorded transfer was {} days ago ({}) — clears FHA 90/180-day anti-flipping window.", hold_days.max(0), latest.date_str),
            })
        }
    } else {
        json!({
            "status": "unknown_hold_period",
            "rule": "24 CFR § 203.37a",
            "fhaEligible": true,
            "secondAppraisalRequired": false,
            "sellerHoldDays": Value::Null,
            "resaleMarkupPct": Value::Null,
            "note": "No recorded transfer date in public summary; verify seller's deed recording date on Prelim Schedule A if using FHA financing.",
        })
    };

    let nearby_distressed_count = fc
        .get("nearbyDistressed")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let (buyer_leverage_note, seller_cure_note, agent_escrow_note) = match distress_stage {
        "active_nod_or_nts" => (
            "Active default/foreclosure filing creates urgent seller timeline pressure: make close-of-escrow contingent on verified full lender/junior-lien payoff or written short-sale approval, and never release earnest money directly to seller.".to_string(),
            "Request a formal statutory Reinstatement Quote & Payoff Demand immediately, request trustee sale postponement once a bona fide purchase contract & proof of funds are opened in escrow, and consult foreclosure/bankruptcy counsel.".to_string(),
            "Confirm whether total payoffs + closing costs fit within gross contract price (equity sale vs. short sale), verify auction postponement in writing, and guard against IRS 120-day redemption or surviving HOA/municipal liens.".to_string(),
        ),
        "tax_delinquent" => (
            "Delinquent property taxes hold first-priority statutory lien status and accrue steep monthly redemption penalties (up to 1.5%/mo / 18%/yr in CA): require full County Tax Collector redemption payoff from seller proceeds at closing.".to_string(),
            "Order an official County Tax Collector Redemption Certificate showing exact payoff through projected closing date so penalty accruals do not create a last-minute net-sheet shortfall.".to_string(),
            "Have escrow pull the official redemption payoff certificate early and verify no tax-deed auction date has been noticed.".to_string(),
        ),
        "prior_reo_or_trustee_deed" => (
            "Historical REO / Trustee's Deed in chain: verify title insurer issues standard/ALTA owner's coverage with zero surviving junior tax-lien, municipal code-lien, or uninsured-deed exceptions.".to_string(),
            "Provide prior owner's title policy and permits for post-REO renovations to reassure buyers and appraisers.".to_string(),
            "Check chain of title for any uninsured Quitclaim or post-foreclosure gap and confirm FHA 90/180-day seasoning if marketing to FHA buyers.".to_string(),
        ),
        _ => (
            "No active pre-foreclosure NOD, NTS, or tax default surfaced in available public records; still verify current-year property tax installments and mortgage payoffs on the Prelim.".to_string(),
            "Keep property tax installments and HOA dues current through closing day to avoid late-penalty debits on the escrow settlement statement.".to_string(),
            "Verify seller's hold period (>90/180 days for FHA buyers) and confirm all HELOCs have signed freeze/termination letters in escrow.".to_string(),
        ),
    };

    json!({
        "distressStage": distress_stage,
        "coverageStatus": coverage_status,
        "isActivelyDistressed": active_nod_or_nts || tax_delinquent,
        "statutoryForeclosureTrack": track,
        "statutoryCitation": stat_cite,
        "statutoryTimelineNote": stat_note,
        "activeFiling": active_filing,
        "historicalDistressTransfers": historical_distress,
        "quitclaimInChain": quitclaim_in_chain,
        "fhaAntiFlipping": fha_anti_flipping,
        "nearbyDistressedCount": nearby_distressed_count,
        "buyerLeverageNote": buyer_leverage_note,
        "sellerCureNote": seller_cure_note,
        "agentEscrowNote": agent_escrow_note,
    })
}

// =================================================================================================
// 4. CourtListener Federal/Bankruptcy Dockets, Code Enforcement & SB-800 (`litigationAndDockets`)
// =================================================================================================

fn sanitize_single_party_clause(raw_party: &str) -> String {
    let cleaned = redact_title_pii(raw_party.trim()).masked_text;
    let trimmed = cleaned.trim().trim_matches(|c: char| c == ',' || c == ';');
    if trimmed.is_empty() {
        return "[Redacted Party]".to_string();
    }
    if RE_INSTITUTIONAL_PARTY.is_match(trimmed) {
        // Even if institutional, strip any leading natural person co-party before "and" if present
        let parts: Vec<&str> = trimmed.split(" and ").collect();
        if parts.len() == 2 {
            let left_inst = RE_INSTITUTIONAL_PARTY.is_match(parts[0]);
            let right_inst = RE_INSTITUTIONAL_PARTY.is_match(parts[1]);
            if !left_inst && right_inst {
                return format!("[Redacted Party] and {}", parts[1].trim());
            } else if left_inst && !right_inst {
                return format!("{} and [Redacted Party]", parts[0].trim());
            }
        }
        trimmed.chars().take(90).collect()
    } else {
        "[Redacted Party]".to_string()
    }
}

/// Strictly scrubs natural-person plaintiff, defendant, debtor, and estate names from court docket
/// captions while preserving corporate/institutional/government party names and case nature.
pub fn sanitize_court_docket_title(raw_case_name: &str, suit_nature: Option<&str>) -> String {
    let pre_scrubbed = redact_title_pii(raw_case_name.trim()).masked_text;
    let base = pre_scrubbed.trim();
    if base.is_empty() {
        return suit_nature
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("Federal Docket ({})", s.trim()))
            .unwrap_or_else(|| "Federal Civil / Bankruptcy Docket".to_string());
    }

    if let Some(caps) = RE_IN_RE_PREFIX.captures(base) {
        let prefix = caps[1].trim();
        let subject = sanitize_single_party_clause(&caps[2]);
        let label = format!("{prefix} {subject}");
        return match suit_nature.map(str::trim).filter(|s| !s.is_empty()) {
            Some(nat) => format!("{label} ({nat})"),
            None => label,
        };
    }

    let vs_parts: Vec<&str> = RE_CASE_VS_SPLIT.split(base).collect();
    if vs_parts.len() >= 2 {
        let plaintiff = sanitize_single_party_clause(vs_parts[0]);
        let defendant = sanitize_single_party_clause(vs_parts[1]);
        let caption = format!("{plaintiff} v. {defendant}");
        return match suit_nature.map(str::trim).filter(|s| !s.is_empty()) {
            Some(nat) => format!("{caption} ({nat})"),
            None => caption,
        };
    }

    // Single-clause non-vs caption: preserve only if explicitly institutional/in-rem
    if RE_INSTITUTIONAL_PARTY.is_match(base) {
        base.chars().take(110).collect()
    } else {
        match suit_nature.map(str::trim).filter(|s| !s.is_empty()) {
            Some(nat) => format!("[Redacted Party] Docket ({nat})"),
            None => "[Redacted Party] Civil / Bankruptcy Docket".to_string(),
        }
    }
}

fn builder_repose_for_state(
    state: &str,
    year_built: Option<i64>,
    current_year: i64,
) -> (bool, Value) {
    let st = state.trim().to_ascii_uppercase();
    let (statute, repose_years, statute_desc) = match st.as_str() {
        "CA" => (
            "CA Civ. Code §§ 895–945.5 (SB-800 Right to Repair Act)",
            10_i64,
            "California SB-800 Right to Repair Act imposes a 10-year structural statute of repose from substantial completion (with shorter sub-deadlines: 1-yr fit & finish §900, 2-yr MEP/dryer ducts §896, 4-yr plumbing/sewer/steel, 5-yr paint/stucco) and mandatory pre-litigation builder notice/repair procedures.",
        ),
        "TX" => (
            "TX Civ. Prac. & Rem. Code § 16.009 & TX Prop. Code ch. 27 (RCLA)",
            10_i64,
            "Texas residential construction defect claims are governed by the Residential Construction Liability Act (RCLA) pre-suit notice rules and a 10-year general statute of repose (reduced to 6 years when builder provides qualifying statutory 1-2-6 written warranty under HB 2024).",
        ),
        "FL" => (
            "Fla. Stat. § 95.11(3)(c) & ch. 558",
            7_i64,
            "Florida imposes a 7-year construction-defect statute of repose (SB 360) and mandatory Chapter 558 60-day pre-suit Notice of Claim & builder inspection right.",
        ),
        "NY" => (
            "NY Gen. Bus. Law § 777-a (Housing Merchant Implied Warranty)",
            6_i64,
            "New York statutory new-home warranty covers 1 year workmanship, 2 years plumbing/electrical/HVAC, and 6 years major material structural defects.",
        ),
        "WA" => (
            "RCW 4.16.310 & RCW ch. 64.50",
            6_i64,
            "Washington imposes a 6-year construction statute of repose from substantial completion (plus 45-day pre-suit Notice of Construction Defect).",
        ),
        "CO" => (
            "C.R.S. §§ 13-80-104 & 13-20-803.5 (CDARA)",
            6_i64,
            "Colorado Construction Defect Action Reform Act (CDARA) sets a 6-year statute of repose (up to +2 yrs if defect manifests in years 5–6) with mandatory Notice of Claim.",
        ),
        "NV" => (
            "NRS 11.202 & NRS ch. 40",
            10_i64,
            "Nevada Chapter 40 sets a 10-year construction-defect statute of repose (AB 421) with mandatory pre-litigation notice and opportunity to repair.",
        ),
        "AZ" => (
            "A.R.S. § 12-552 & § 12-1363 (Purchaser Dwelling Act)",
            8_i64,
            "Arizona sets an 8-year implied-warranty/contract statute of repose from substantial completion (up to 9 yrs if discovered in year 8).",
        ),
        _ => (
            "State Construction Statute of Repose & Implied Habitability Warranty",
            10_i64,
            "Most US states enforce a 6-to-10-year statute of repose on latent structural construction defects running from original substantial completion / certificate of occupancy.",
        ),
    };

    if let Some(yb) = year_built.filter(|&y| y > 1700 && y <= current_year + 2) {
        let active = yb >= current_year - repose_years;
        let sb800_active = st == "CA" && yb >= current_year - 10;
        let expires_year = yb + repose_years;
        let years_rem = (expires_year - current_year).max(0);
        let note = if active {
            format!(
                "Built in {yb}: INSIDE the {repose_years}-year statutory construction-defect repose window (through ~{expires_year}, ~{years_rem} yr(s) remaining). {statute_desc}"
            )
        } else {
            format!(
                "Built in {yb}: original {repose_years}-year builder statute of repose (~{expires_year}) has expired; check contractor warranties/permits on any recent additions or remodels."
            )
        };
        (
            sb800_active,
            json!({
                "state": if st.is_empty() { Value::Null } else { json!(st) },
                "statute": statute,
                "reposeYears": repose_years,
                "yearBuilt": yb,
                "expiresYear": expires_year,
                "yearsRemaining": years_rem,
                "active": active,
                "sb800Active": sb800_active,
                "note": note,
            }),
        )
    } else {
        (
            false,
            json!({
                "state": if st.is_empty() { Value::Null } else { json!(st) },
                "statute": statute,
                "reposeYears": repose_years,
                "yearBuilt": Value::Null,
                "expiresYear": Value::Null,
                "yearsRemaining": Value::Null,
                "active": false,
                "sb800Active": false,
                "note": format!("Year built unavailable; cannot evaluate {repose_years}-year builder repose window. {statute_desc}"),
            }),
        )
    }
}

fn extract_code_enforcement(facts: &Value) -> (usize, usize, Vec<Value>) {
    let mut items: Vec<Value> = Vec::new();
    let mut seen_cases: HashSet<String> = HashSet::new();

    let mut push_case = |c: &Value| {
        let case_no = val_str(c.get("caseNumber"))
            .or_else(|| val_str(c.get("number")))
            .unwrap_or("UNNUMBERED")
            .to_string();
        if !seen_cases.insert(case_no.clone()) {
            return;
        }
        let status = val_str(c.get("status")).unwrap_or("Unknown");
        let status_lc = status.to_ascii_lowercase();
        let is_open = c
            .get("isOpen")
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| {
                !(status_lc.contains("closed")
                    || status_lc.contains("resolved")
                    || status_lc.contains("complied")
                    || status_lc.contains("abated"))
            });
        let raw_desc = val_str(c.get("description")).unwrap_or("");
        let clean_desc = scrub_competitor_brands(&redact_title_pii(raw_desc).masked_text);
        items.push(json!({
            "caseNumber": if case_no == "UNNUMBERED" { Value::Null } else { json!(case_no) },
            "date": c.get("date").cloned().unwrap_or(Value::Null),
            "closedDate": c.get("closedDate").cloned().unwrap_or(Value::Null),
            "status": status,
            "isOpen": is_open,
            "description": clean_desc,
        }));
    };

    if let Some(arr) = facts
        .pointer("/disclosures/codeEnforcement/items")
        .and_then(|v| v.as_array())
    {
        for c in arr {
            push_case(c);
        }
    }
    if let Some(arr) = facts.get("codeEnforcement").and_then(|v| v.as_array()) {
        for c in arr {
            push_case(c);
        }
    }

    let open_count = items
        .iter()
        .filter(|i| i.get("isOpen").and_then(|v| v.as_bool()) == Some(true))
        .count();
    let closed_count = items.len().saturating_sub(open_count);
    (open_count, closed_count, items)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum LiveDocketStatus {
    #[default]
    NotQueriedOffline,
    LiveChecked,
    Unauthenticated,
    Unavailable,
}

#[derive(Debug, Default, Clone)]
struct LiveDocketOutcome {
    checked: bool,
    status: LiveDocketStatus,
    error_note: Option<String>,
    hits: Vec<Value>,
}

fn build_litigation_and_dockets_section(
    report: &Value,
    geo: &Value,
    facts: &Value,
    hoa_profile: &Value,
    as_of: NaiveDate,
    live_dockets: Option<&LiveDocketOutcome>,
) -> Value {
    let (state, _county, _county_fips, _city) = resolve_state_and_county(geo, facts);
    let current_year = i64::from(as_of.year());
    let year_built = val_i64(facts.get("yearBuilt"))
        .or_else(|| val_i64(facts.pointer("/buildingRegister/yearBuilt")))
        .filter(|&y| y > 1700);

    let (sb800_active, builder_repose) = builder_repose_for_state(&state, year_built, current_year);
    let (open_ce_count, closed_ce_count, ce_cases) = extract_code_enforcement(facts);

    // HOA / Condo Litigation extraction (`hoaProfile`, `hoaReview`, `disclosures`)
    let hoa_review = report
        .get("hoaReview")
        .or_else(|| report.pointer("/modules/hoaReview"))
        .map(unwrap_data)
        .filter(|v| v.is_object());

    let mut hoa_lit_flag = hoa_profile
        .get("litigationFlag")
        .or_else(|| hoa_profile.pointer("/litigation/hasActiveLitigation"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || hoa_review
            .and_then(|hr| {
                hr.get("litigationFlag")
                    .or_else(|| hr.pointer("/litigation/active"))
            })
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

    let mut hoa_lit_detail: Option<String> = val_str(hoa_profile.pointer("/litigation/summary"))
        .or_else(|| val_str(hoa_review.and_then(|hr| hr.pointer("/litigation/summary"))))
        .map(|s| scrub_competitor_brands(&redact_title_pii(s).masked_text));

    if !hoa_lit_flag {
        if let Ok(hoa_blob) = serde_json::to_string(hoa_profile) {
            if RE_HOA_LITIGATION.is_match(&hoa_blob) && !RE_NO_HOA_LITIGATION.is_match(&hoa_blob) {
                hoa_lit_flag = true;
                hoa_lit_detail = Some(
                    "HOA profile references pending association or structural defect litigation; request HOA litigation counsel disclosure letter.".to_string(),
                );
            }
        }
    }

    let docket_hits = live_dockets.map(|d| d.hits.clone()).unwrap_or_default();
    let docket_status = live_dockets
        .map(|d| d.status)
        .unwrap_or(LiveDocketStatus::NotQueriedOffline);

    let coverage_status = match docket_status {
        LiveDocketStatus::LiveChecked => "courtlistener_live_checked",
        LiveDocketStatus::Unauthenticated => "courtlistener_unauthenticated",
        LiveDocketStatus::Unavailable => "courtlistener_unavailable",
        LiveDocketStatus::NotQueriedOffline => "local_disclosures_and_assessor_only",
    };
    let coverage_note = match docket_status {
        LiveDocketStatus::LiveChecked => format!(
            "Checked CourtListener RECAP federal civil & bankruptcy dockets by exact street address ({} verified docket match(es)). State/County Superior Court quiet-title, partition, probate, and mechanic's-lien actions require Prelim Schedule B & County Superior Court index verification.",
            docket_hits.len()
        ),
        LiveDocketStatus::Unauthenticated => "Screened local municipal code-enforcement records, HOA disclosures, and statutory builder-repose windows. Federal RECAP dockets require COURTLISTENER_API_TOKEN (unauthenticated — not queried); County Superior Court civil/probate actions also require Prelim Schedule B verification.".to_string(),
        LiveDocketStatus::Unavailable => format!(
            "Screened local municipal code-enforcement records, HOA disclosures, and statutory builder-repose windows. Live CourtListener RECAP query was unavailable ({}); County Superior Court civil/probate actions require Prelim Schedule B verification.",
            live_dockets
                .and_then(|d| d.error_note.as_deref())
                .unwrap_or("upstream HTTP error")
        ),
        LiveDocketStatus::NotQueriedOffline => "Screened local municipal code-enforcement records, HOA disclosures, and statutory builder-repose windows. Federal RECAP dockets were not queried in offline/cached mode, and County Superior Court civil/probate actions require Prelim Schedule B verification.".to_string(),
    };

    let prop_type_lc = val_str(facts.get("propertyType"))
        .or_else(|| val_str(facts.pointer("/buildingRegister/propertyType")))
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_condo_or_townhome = prop_type_lc.contains("condo")
        || prop_type_lc.contains("townh")
        || prop_type_lc.contains("co-op")
        || prop_type_lc.contains("coop");
    let has_hoa = val_f64(facts.get("hoaMonthly")).unwrap_or(0.0) > 0.0
        || hoa_profile
            .get("hasHoa")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

    let (warrant_impact, warrant_note) = if hoa_lit_flag {
        (
            "non_warrantable_risk",
            "Active HOA / structural defect litigation flagged: under Fannie Mae Selling Guide B4-2.1-03 & Freddie Mac §5701.3, condo/PUD projects in non-minor structural, safety, or habitability litigation are generally NON-WARRANTABLE, blocking standard conventional/FHA/VA loans and requiring cash or portfolio/non-QM financing.",
        )
    } else if !docket_hits.is_empty() || open_ce_count > 0 {
        (
            "review_required",
            "Federal docket hit or open municipal code-enforcement case detected: lenders require open health/safety code violations and title-affecting litigation (Lis Pendens / bankruptcy stay under 11 U.S.C. § 362) to be cleared or court-approved prior to loan funding.",
        )
    } else if is_condo_or_townhome || has_hoa {
        (
            "no_litigation_blocker_detected",
            "No HOA structural litigation or open code-enforcement blocker detected in available records; still require full Fannie Mae Form 1076 Condo Project Questionnaire (reserves >=10%, delinquency <=15%, zero structural litigation) during underwriting.",
        )
    } else {
        (
            "not_applicable_sfr",
            "Detached non-HOA property (Fannie Mae Condo Project Warrantability questionnaire does not apply); verify zero open municipal building/code notices and clean Prelim Schedule B.",
        )
    };

    json!({
        "coverageStatus": coverage_status,
        "coverageNote": coverage_note,
        "docketHits": docket_hits,
        "docketHitCount": docket_hits.len(),
        "hoaLitigationFlag": hoa_lit_flag,
        "hoaLitigationDetail": hoa_lit_detail,
        "codeEnforcement": {
            "openCount": open_ce_count,
            "closedCount": closed_ce_count,
            "cases": ce_cases,
        },
        "sb800WarrantyActive": sb800_active,
        "builderWarrantyRepose": builder_repose,
        "condoWarrantabilityImpact": warrant_impact,
        "condoWarrantabilityNote": warrant_note,
    })
}

// =================================================================================================
// 5. Live Keyless Open-Data Connectors (CourtListener v4, NYC ACRIS, Cook County, SF, BYOK ATTOM)
// =================================================================================================

fn is_external_http_blocked(http: &HttpClient, env: &EnvConfig) -> bool {
    if http.is_offline_blocked() {
        return true;
    }
    env.get("OFFLINE_BLOCK_EXTERNAL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

async fn query_courtlistener_dockets(
    http: &HttpClient,
    env: &EnvConfig,
    street: &str,
    city: &str,
    sources: &mut Vec<SourceItem>,
    warnings: &mut Vec<String>,
) -> LiveDocketOutcome {
    let clean_street = street.trim();
    let clean_city = city.trim();
    if clean_street.len() < 5 || clean_city.len() < 2 {
        return LiveDocketOutcome::default();
    }

    let base_url = env
        .get("COURTLISTENER_API_URL")
        .unwrap_or(COURTLISTENER_SEARCH_DEFAULT_URL);
    let exact_q = format!("\"{clean_street}\" \"{clean_city}\"");
    let Ok(parsed_url) =
        Url::parse_with_params(base_url, &[("type", "r"), ("q", exact_q.as_str())])
    else {
        return LiveDocketOutcome::default();
    };

    let auth_val = env
        .get("COURTLISTENER_API_TOKEN")
        .map(str::trim)
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("Token {tok}"));
    if auth_val.is_none() && base_url == COURTLISTENER_SEARCH_DEFAULT_URL {
        warnings.push(
            "title-legal-radar: CourtListener RECAP query skipped — COURTLISTENER_API_TOKEN not configured (unauthenticated)".to_string(),
        );
        return LiveDocketOutcome {
            checked: false,
            status: LiveDocketStatus::Unauthenticated,
            error_note: Some(
                "Federal RECAP dockets require COURTLISTENER_API_TOKEN (unauthenticated)"
                    .to_string(),
            ),
            hits: Vec::new(),
        };
    }

    let hdr_vec: Vec<(&str, &str)> = match auth_val.as_deref() {
        Some(v) => vec![("Authorization", v), ("Accept", "application/json")],
        None => vec![("Accept", "application/json")],
    };

    match http
        .get_json_opts(
            parsed_url.as_str(),
            LIVE_QUERY_TIMEOUT_MS,
            0,
            86_400,
            Some(&hdr_vec),
        )
        .await
    {
        Ok(resp) => {
            sources.push(SourceItem::new(
                "Free Law Project — CourtListener REST API v4 (RECAP Federal Civil & Bankruptcy Dockets)",
                "https://www.courtlistener.com/help/api/rest/v4/",
            ));
            let street_lc = clean_street.to_ascii_lowercase();
            let mut hits = Vec::new();
            if let Some(arr) = resp.get("results").and_then(|v| v.as_array()) {
                for item in arr.iter().take(8) {
                    let raw_case = val_str(item.get("caseName"))
                        .or_else(|| val_str(item.get("case_name")))
                        .unwrap_or("");
                    let suit_nature =
                        val_str(item.get("suitNature")).or_else(|| val_str(item.get("cause")));
                    // Verify street substring match across returned text fields to prevent fuzzy false positives
                    let hay = format!(
                        "{} {} {}",
                        raw_case,
                        val_str(item.get("snippet")).unwrap_or(""),
                        val_str(item.get("short_description")).unwrap_or("")
                    )
                    .to_ascii_lowercase();
                    if !hay.is_empty() && !hay.contains(&street_lc) && arr.len() > 2 {
                        continue;
                    }
                    let safe_title = sanitize_court_docket_title(raw_case, suit_nature);
                    let court = val_str(item.get("court"))
                        .or_else(|| val_str(item.get("court_citation_string")))
                        .unwrap_or("U.S. Federal / Bankruptcy Court");
                    let raw_docket_num = val_str(item.get("docketNumber"))
                        .or_else(|| val_str(item.get("docket_number")))
                        .unwrap_or("");
                    let safe_docket_num = redact_title_pii(raw_docket_num).masked_text;
                    let date_filed =
                        val_str(item.get("dateFiled")).or_else(|| val_str(item.get("date_filed")));
                    let abs_url = val_str(item.get("absolute_url"))
                        .map(|u| {
                            if u.starts_with("http") {
                                u.to_string()
                            } else {
                                format!("https://www.courtlistener.com{u}")
                            }
                        })
                        .unwrap_or_else(|| "https://www.courtlistener.com/recap/".to_string());

                    hits.push(json!({
                        "title": safe_title,
                        "court": court,
                        "docketNumber": if safe_docket_num.is_empty() { Value::Null } else { json!(safe_docket_num) },
                        "dateFiled": date_filed,
                        "natureOfSuit": suit_nature,
                        "url": abs_url,
                    }));
                    if hits.len() >= 5 {
                        break;
                    }
                }
            }
            LiveDocketOutcome {
                checked: true,
                status: LiveDocketStatus::LiveChecked,
                error_note: None,
                hits,
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            let is_unauth = err_str.contains("HTTP 401") || err_str.contains("HTTP 403");
            warnings.push(sanitize_user_warning(&format!(
                "title-legal-radar: CourtListener docket check {}: ({err_str})",
                if is_unauth {
                    "unauthenticated"
                } else {
                    "unavailable"
                }
            )));
            LiveDocketOutcome {
                checked: false,
                status: if is_unauth {
                    LiveDocketStatus::Unauthenticated
                } else {
                    LiveDocketStatus::Unavailable
                },
                error_note: Some(if is_unauth {
                    "Federal RECAP dockets require COURTLISTENER_API_TOKEN (unauthenticated)"
                        .to_string()
                } else {
                    sanitize_user_warning(&err_str)
                }),
                hits: Vec::new(),
            }
        }
    }
}

async fn query_open_recorder_and_distress(
    http: &HttpClient,
    env: &EnvConfig,
    state: &str,
    county_fips: &str,
    city: &str,
    street: &str,
    apn: Option<&str>,
    sources: &mut Vec<SourceItem>,
    warnings: &mut Vec<String>,
) -> (LiveRecorderOutcome, LiveRecorderOutcome) {
    let mut rec_out = LiveRecorderOutcome::default();
    let mut dist_out = LiveRecorderOutcome::default();

    let st = state.trim().to_ascii_uppercase();
    let fips = county_fips.trim();
    let clean_street = street.trim();

    // 1. NYC ACRIS Real Property Legals + DOF Tax Lien Sale List (5 NYC boroughs)
    let is_nyc = st == "NY"
        && (matches!(fips, "36005" | "36047" | "36061" | "36081" | "36085")
            || matches!(
                city.trim().to_ascii_lowercase().as_str(),
                "new york" | "manhattan" | "brooklyn" | "queens" | "bronx" | "staten island"
            ));

    if is_nyc && clean_street.len() >= 4 {
        if let Ok(acris_url) = Url::parse_with_params(
            "https://data.cityofnewyork.us/resource/8h5j-fqxa.json",
            &[("$limit", "8"), ("$q", clean_street)],
        ) {
            match http
                .get_json_opts(acris_url.as_str(), LIVE_QUERY_TIMEOUT_MS, 0, 86_400, None)
                .await
            {
                Ok(Value::Array(rows)) => {
                    rec_out.checked = true;
                    rec_out.source_name =
                        Some("NYC Dept. of Finance — ACRIS Open Data".to_string());
                    rec_out.source_url = Some(
                        "https://data.cityofnewyork.us/City-Government/ACRIS-Real-Property-Legals/8h5j-fqxa".to_string(),
                    );
                    sources.push(SourceItem::new(
                        "NYC ACRIS Real Property Legals (NYC Open Data Socrata)",
                        "https://data.cityofnewyork.us/City-Government/ACRIS-Real-Property-Legals/8h5j-fqxa",
                    ));
                    for row in rows.iter().take(6) {
                        rec_out.records.push(json!({
                            "documentId": val_str(row.get("document_id")),
                            "documentType": val_str(row.get("property_type")).unwrap_or("ACRIS Indexed Parcel Instrument"),
                            "recordedDate": val_str(row.get("good_through_date")),
                            "boroughBlockLot": format!(
                                "{}-{}-{}",
                                val_str(row.get("borough")).unwrap_or("?"),
                                val_str(row.get("block")).unwrap_or("?"),
                                val_str(row.get("lot")).unwrap_or("?")
                            ),
                        }));
                    }
                }
                Ok(_) => {}
                Err(e) => warnings.push(sanitize_user_warning(&format!(
                    "title-legal-radar: NYC ACRIS lookup failed ({e})"
                ))),
            }
        }

        if let Ok(lien_url) = Url::parse_with_params(
            "https://data.cityofnewyork.us/resource/9f9k-226w.json",
            &[("$limit", "5"), ("$q", clean_street)],
        ) {
            if let Ok(Value::Array(rows)) = http
                .get_json_opts(lien_url.as_str(), LIVE_QUERY_TIMEOUT_MS, 0, 43_200, None)
                .await
            {
                dist_out.checked = true;
                dist_out.source_name = Some("NYC DOF Tax Lien Sale List (Open Data)".to_string());
                dist_out.source_url = Some(
                    "https://data.cityofnewyork.us/City-Government/DOF-Tax-Lien-Sale-List/9f9k-226w"
                        .to_string(),
                );
                for row in rows.iter().take(5) {
                    dist_out.records.push(json!({
                        "documentType": "NYC DOF Tax Lien Sale Notice",
                        "recordedDate": val_str(row.get("month")),
                        "documentId": format!("{}-{}", val_str(row.get("block")).unwrap_or(""), val_str(row.get("lot")).unwrap_or("")),
                    }));
                }
            }
        }
    }

    // 2. Cook County / Chicago Open Building & Code Lien Violations (FIPS 17031)
    if (fips == "17031" || (st == "IL" && city.eq_ignore_ascii_case("chicago")))
        && clean_street.len() >= 4
    {
        if let Ok(chi_url) = Url::parse_with_params(
            "https://data.cityofchicago.org/resource/22u3-xenr.json",
            &[("$limit", "6"), ("$q", clean_street)],
        ) {
            if let Ok(Value::Array(rows)) = http
                .get_json_opts(chi_url.as_str(), LIVE_QUERY_TIMEOUT_MS, 0, 86_400, None)
                .await
            {
                rec_out.non_lien_proxy = true;
                rec_out.source_name =
                    Some("Cook County / City of Chicago Open Municipal & Clerk Data".to_string());
                rec_out.source_url = Some("https://datacatalog.cookcountyil.gov/".to_string());
                sources.push(SourceItem::new(
                    "Cook County / Chicago Open Municipal Code & Clerk Portal",
                    "https://datacatalog.cookcountyil.gov/",
                ));
                for row in rows.iter().take(5) {
                    let st_txt = val_str(row.get("violation_status")).unwrap_or("INDEXED");
                    rec_out.records.push(json!({
                        "documentId": val_str(row.get("id")),
                        "documentType": format!("Municipal Building/Code Record ({st_txt}) — non-lien proxy"),
                        "recordedDate": val_str(row.get("violation_date")),
                    }));
                }
            }
        }
    }

    // 3. San Francisco Assessor-Recorder Open Data (FIPS 06075)
    if fips == "06075" && (apn.is_some() || clean_street.len() >= 4) {
        let q_tok = apn.unwrap_or(clean_street);
        if let Ok(sf_url) = Url::parse_with_params(
            "https://data.sfgov.org/resource/wv5m-vpq2.json",
            &[("$limit", "5"), ("$q", q_tok)],
        ) {
            if let Ok(Value::Array(rows)) = http
                .get_json_opts(sf_url.as_str(), LIVE_QUERY_TIMEOUT_MS, 0, 86_400, None)
                .await
            {
                rec_out.non_lien_proxy = true;
                rec_out.source_name =
                    Some("San Francisco Assessor-Recorder DataSF Open Roll".to_string());
                rec_out.source_url = Some("https://data.sfgov.org/".to_string());
                sources.push(SourceItem::new(
                    "City & County of San Francisco Assessor-Recorder Open Data",
                    "https://data.sfgov.org/",
                ));
                for row in rows.iter().take(4) {
                    rec_out.records.push(json!({
                        "documentId": val_str(row.get("parcel_number")),
                        "documentType": "SF Secured Assessor-Recorder Roll Entry (non-lien proxy)",
                        "recordedDate": val_str(row.get("closed_roll_year")),
                    }));
                }
            }
        }
    }

    // 4. Optional BYOK ATTOM Recorder / Mortgage Detail Check (zero-PII redaction)
    if let Some(attom_key) = env
        .get("ATTOM_API_KEY")
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        if clean_street.len() >= 4 && !city.trim().is_empty() && !st.is_empty() {
            let addr2 = format!("{}, {}", city.trim(), st);
            if let Ok(attom_url) = Url::parse_with_params(
                "https://api.gateway.attomdata.com/propertyapi/v1.0.0/property/detailmortgage",
                &[("address1", clean_street), ("address2", addr2.as_str())],
            ) {
                let hdrs = [("apikey", attom_key), ("Accept", "application/json")];
                if let Ok(resp) = http
                    .get_json_opts(
                        attom_url.as_str(),
                        LIVE_QUERY_TIMEOUT_MS,
                        0,
                        86_400,
                        Some(&hdrs),
                    )
                    .await
                {
                    rec_out.checked = true;
                    rec_out.source_name =
                        Some("ATTOM Recorded Mortgage & Deed Index (BYOK)".to_string());
                    rec_out.source_url = Some("https://api.developer.attomdata.com/".to_string());
                    sources.push(SourceItem::new(
                        "ATTOM US County Recorder & Mortgage API",
                        "https://api.developer.attomdata.com/",
                    ));
                    if let Some(prop) = resp.pointer("/property/0").and_then(|v| v.as_object()) {
                        if let Some(mtg) = prop.get("mortgage") {
                            let lender_raw = mtg
                                .pointer("/FirstConcurrent/lenderLastName")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Institutional Lender");
                            let safe_lender = sanitize_single_party_clause(lender_raw);
                            rec_out.records.push(json!({
                                "documentId": mtg.pointer("/FirstConcurrent/trustDeedDocumentNumber").cloned().unwrap_or(Value::Null),
                                "documentType": format!("Recorded First Mortgage / Deed of Trust ({safe_lender})"),
                                "recordedDate": mtg.pointer("/FirstConcurrent/date").cloned().unwrap_or(Value::Null),
                            }));
                        }
                    }
                }
            }
        }
    }

    (rec_out, dist_out)
}

// =================================================================================================
// 6. Top-Level Assembly (`build_title_legal_radar_deterministic` & `run_title_legal_radar`)
// =================================================================================================

fn assemble_title_legal_radar(
    report: &Value,
    live_recorder: Option<&LiveRecorderOutcome>,
    live_distress: Option<&LiveRecorderOutcome>,
    live_dockets: Option<&LiveDocketOutcome>,
) -> Value {
    let geo = report.get("geo").map(unwrap_data).unwrap_or(&Value::Null);
    let facts = report.get("facts").map(unwrap_data).unwrap_or(&Value::Null);
    let valuation = module_data(report, "valuation");
    let hoa_profile = report
        .get("hoaProfile")
        .or_else(|| report.pointer("/modules/hoaProfile"))
        .map(unwrap_data)
        .unwrap_or(&Value::Null);

    let as_of = resolve_as_of_date(report);
    let subject_price = resolve_subject_price(facts, valuation);

    let rate_filing = compute_state_title_rate_filing(geo, facts, subject_price, as_of);
    let mut recorded_liens =
        build_recorded_liens_section(report, geo, facts, hoa_profile, live_recorder);
    let mut foreclosure =
        build_foreclosure_and_distress_section(report, geo, facts, valuation, as_of, live_distress);
    let mut litigation =
        build_litigation_and_dockets_section(report, geo, facts, hoa_profile, as_of, live_dockets);

    // If `report` already carries live-fetched sub-sections (e.g. when `normalize_stored_report`
    // runs on a report previously enriched by `run_title_legal_radar`), preserve live coverage & hits!
    let existing = report
        .get("titleLegalRadar")
        .or_else(|| report.pointer("/modules/titleLegalRadar"))
        .map(unwrap_data);
    if live_recorder.is_none() {
        if let Some(prev_liens) = existing.and_then(|e| e.get("recordedLiensAndEncumbrances")) {
            if val_str(prev_liens.get("coverageStatus")) == Some("live_open_recorder_checked") {
                let src = val_str(prev_liens.get("liveRecorderSource")).unwrap_or("");
                let is_non_lien_proxy = src.contains("Open Municipal & Clerk Data")
                    || src.contains("Assessor-Recorder DataSF Open Roll");
                if !is_non_lien_proxy {
                    recorded_liens = prev_liens.clone();
                }
            }
        }
    }
    if live_distress.is_none() {
        if let Some(prev_fc) = existing.and_then(|e| e.get("foreclosureAndDistress")) {
            if val_str(prev_fc.get("coverageStatus")) == Some("live_open_distress_checked") {
                foreclosure = prev_fc.clone();
            }
        }
    }
    if live_dockets.is_none() {
        if let Some(prev_lit) = existing.and_then(|e| e.get("litigationAndDockets")) {
            if val_str(prev_lit.get("coverageStatus")) == Some("courtlistener_live_checked") {
                litigation = prev_lit.clone();
            }
        }
    }

    let active_distress = foreclosure
        .get("isActivelyDistressed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let hoa_lit = litigation
        .get("hoaLitigationFlag")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let docket_cnt = litigation
        .get("docketHitCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let open_ce = litigation
        .pointer("/codeEnforcement/openCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let critical_lien = recorded_liens
        .get("flags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|f| matches!(val_str(f.get("severity")), Some("critical" | "major")))
        })
        .unwrap_or(false);
    let prelim_uploaded = val_str(recorded_liens.get("coverageStatus")) == Some("prelim_uploaded");

    let overall_status =
        if active_distress || hoa_lit || critical_lien || docket_cnt > 0 || open_ce > 0 {
            "action_required"
        } else if recorded_liens
            .get("flags")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            "watchouts_present"
        } else if prelim_uploaded {
            "prelim_verified_clear"
        } else {
            "screened_verify_prelim"
        };

    let headline = match overall_status {
        "action_required" => "Active title, lien, distress, or litigation item(s) detected — require formal escrow payoff/clearance before removing contingencies.".to_string(),
        "watchouts_present" => "No active NOD/NTS or litigation blocker in public records, but title/UCC-1/vesting watchouts require Preliminary Title Report verification.".to_string(),
        "prelim_verified_clear" => "Uploaded Preliminary Title Report screened with no active involuntary lien blockers.".to_string(),
        _ => "Public records & statutory title rates screened — order an official Preliminary Title Report (Prelim) in escrow to verify unindexed liens and easements.".to_string(),
    };

    json!({
        "asOfDate": as_of.format("%Y-%m-%d").to_string(),
        "overallRadarStatus": overall_status,
        "headline": headline,
        "underwriterRateFiling": rate_filing,
        "recordedLiensAndEncumbrances": recorded_liens,
        "foreclosureAndDistress": foreclosure,
        "litigationAndDockets": litigation,
        "disclaimer": TITLE_LEGAL_RADAR_DISCLAIMER,
    })
}

/// Synchronous, pure deterministic builder usable inside `normalize_stored_report` on cached/fixture reports.
pub fn build_title_legal_radar_deterministic(report: &Value) -> Value {
    assemble_title_legal_radar(report, None, None, None)
}

/// Convenience helper ensuring `report["titleLegalRadar"]` is populated on a mutable report JSON object.
pub fn ensure_title_legal_radar_on_report(report: &mut Value) {
    let radar = build_title_legal_radar_deterministic(report);
    if let Some(obj) = report.as_object_mut() {
        obj.insert("titleLegalRadar".to_string(), radar);
    }
}

/// Live async Title, Lien, Pre-Foreclosure & Court Docket Radar enrichment for `orchestrator.rs`.
/// Queries keyless open-data County Recorder APIs, optional BYOK `ATTOM_API_KEY`, and
/// **CourtListener REST API v4** (`4s` timeout, offline guard, strict zero-PII redaction).
pub async fn run_title_legal_radar(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
) -> ModuleOutput {
    let geo = report.get("geo").map(unwrap_data).unwrap_or(&Value::Null);
    let facts = report.get("facts").map(unwrap_data).unwrap_or(&Value::Null);
    let (state, _county, county_fips, city) = resolve_state_and_county(geo, facts);
    let street_opt = resolve_street_line(report, geo, facts);
    let apn_opt = val_str(facts.get("apn"));

    let mut sources = vec![SourceItem::new(
        "State Insurance Regulators & Title Rating Bureaus (Statutory & State-Filed Title Manuals)",
        "https://www.alta.org/",
    )];
    let mut warnings = Vec::new();

    if is_external_http_blocked(http, env) {
        let data = build_title_legal_radar_deterministic(report);
        return ModuleOutput {
            data,
            sources,
            warnings,
        };
    }

    let street_ref = street_opt.as_deref().unwrap_or("");
    let mut docket_sources = Vec::new();
    let mut docket_warnings = Vec::new();
    let mut rec_sources = Vec::new();
    let mut rec_warnings = Vec::new();

    let (docket_out, (rec_out, dist_out)) = tokio::join!(
        query_courtlistener_dockets(
            http,
            env,
            street_ref,
            &city,
            &mut docket_sources,
            &mut docket_warnings,
        ),
        query_open_recorder_and_distress(
            http,
            env,
            &state,
            &county_fips,
            &city,
            street_ref,
            apn_opt,
            &mut rec_sources,
            &mut rec_warnings,
        )
    );

    sources.extend(docket_sources);
    sources.extend(rec_sources);
    warnings.extend(docket_warnings);
    warnings.extend(rec_warnings);

    let data =
        assemble_title_legal_radar(report, Some(&rec_out), Some(&dist_out), Some(&docket_out));
    ModuleOutput {
        data,
        sources,
        warnings,
    }
}

/// Native `DataExtensionAdapter` for `title_underwriter_rates`, backed directly by
/// `compute_state_title_rate_filing` (no external web scraping or fabricated fee estimates).
pub struct TitleUnderwriterRatesAdapter;

impl DataExtensionAdapter for TitleUnderwriterRatesAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: "title_underwriter_rates".to_string(),
            name: "50-State Statutory & Filed Title Insurance Rate Engine".to_string(),
            version: "1.0.0".to_string(),
            description: "Computes promulgated statutory title insurance rates (TX TDI R-1, FL, NM, IA), state-filed rating bureau tariffs (NY, PA, NJ, OH PR-1, NC), and clearly labeled benchmark schedules for all 50 states + DC.".to_string(),
            domain: CapabilityDomain::DomainInsurance as i32,
            additional_domains: Vec::new(),
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                states: vec!["*".to_string()],
                counties: Vec::new(),
                zip_prefixes: Vec::new(),
                zip_codes: Vec::new(),
                cities: Vec::new(),
                nationwide: true,
                specificity_override: Some(100),
            }),
            auth: None,
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 50,
                burst: 100,
                timeout_ms: 1_000,
                max_retries: 0,
            }),
            cache_ttl_secs: 24 * 3600,
            priority: 100,
            enabled: true,
            builtin: true,
            proto_message_type: "openestate.extension.v1.InsuranceQuoteProto".to_string(),
            openapi_spec_file: None,
            documentation_url: Some("https://www.tdi.texas.gov/title/titleman.html".to_string()),
            codeowners: vec!["open-estate-legal".to_string()],
            default_provenance: ProvenanceTier::ProvenanceOfficialTariff as i32,
            depends_on: Vec::new(),
            input_from_domains: Vec::new(),
            upstream_require_mode: "any".to_string(),
        }
    }

    fn execute_sync(&self, query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        let state = query.state.trim().to_ascii_uppercase();
        if state.len() != 2 {
            return Ok(Vec::new());
        }
        let geo = json!({
            "state": state,
            "county": query.county_name.as_deref().unwrap_or(""),
            "countyFips": query.county_fips.trim(),
            "city": query.city.trim(),
        });
        let mut facts_map = serde_json::Map::new();
        if let Some(d) = query
            .extra_params
            .get("prior_sale_date")
            .or_else(|| query.extra_params.get("last_sale_date"))
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let prior_price = query
                .extra_params
                .get("prior_sale_price")
                .or_else(|| query.extra_params.get("last_sale_price"))
                .and_then(|s| s.trim().parse::<i64>().ok());
            facts_map.insert(
                "lastSale".to_string(),
                json!({ "date": d, "price": prior_price }),
            );
        }
        let facts = Value::Object(facts_map);

        let price_from_params = ["price", "list_price_usd", "purchase_price", "listPrice"]
            .iter()
            .find_map(|k| {
                query
                    .extra_params
                    .get(*k)
                    .and_then(|v| v.trim().parse::<f64>().ok())
            });
        let price_from_upstream = query.upstream_envelopes.iter().find_map(|env| {
            match &env.payload {
                Some(Payload::MarketValuation(m)) => m.estimated_value_usd.map(|v| v as f64),
                Some(Payload::PropertyFacts(f)) => {
                    f.list_price_usd.or(f.last_sale_price_usd).map(|v| v as f64)
                }
                _ => None,
            }
        });
        let subject_price = price_from_params
            .or(price_from_upstream)
            .filter(|p| p.is_finite() && *p > 0.0);

        let as_of = Utc::now().date_naive();
        let filing = compute_state_title_rate_filing(&geo, &facts, subject_price, as_of);
        let regime = filing["rateRegime"].as_str().unwrap_or("estimated_schedule");
        let provenance = if matches!(regime, "promulgated_statutory" | "state_filed_tariff") {
            ProvenanceTier::ProvenanceOfficialTariff
        } else {
            ProvenanceTier::ProvenanceUnverified
        };
        let source_name = filing["regulator"]
            .as_str()
            .unwrap_or("State Title Insurance Rate Manual");
        let source_url = filing["filingUrl"]
            .as_str()
            .unwrap_or("https://www.alta.org/");
        let owner_std = filing["ownerStandardUsd"]
            .as_i64()
            .map(|v| format!("${v}"))
            .unwrap_or_else(|| "null (price required)".to_string());
        let sim_lender = filing["simultaneousLenderUsd"]
            .as_i64()
            .map(|v| format!("${v}"))
            .unwrap_or_else(|| "null".to_string());
        let escrow = filing["escrowSettlementUsd"]
            .as_i64()
            .map(|v| format!("${v}"))
            .unwrap_or_else(|| "null".to_string());
        let citation = filing["citation"].as_str().unwrap_or("");
        let basis = filing["rateScheduleBasis"].as_str().unwrap_or(regime);
        let serialized = serde_json::to_string(&filing).unwrap_or_default();

        // Keep annual homeowners premium fields `None` so `aggregator::merge_insurance` never
        // conflates one-time closing title insurance with annual hazard/fire premiums.
        let quote = InsuranceQuoteProto {
            estimated_annual_premium_usd: None,
            effective_pct_of_value: None,
            fair_plan_or_surplus_risk: None,
            flood_insurance_annual_usd: None,
            earthquake_insurance_annual_usd: None,
            carrier_market_status: None,
            underwriting_notes: vec![
                format!(
                    "title_underwriter_rates [{state}] regime={regime} basis={basis} ownerStandard={owner_std} simultaneousLender={sim_lender} escrow={escrow} citation={citation}"
                ),
                serialized.clone(),
            ],
        };

        Ok(vec![ExtensionEnvelopeProto {
            extension_id: "title_underwriter_rates".to_string(),
            domain: CapabilityDomain::DomainInsurance as i32,
            priority: 100,
            geo_specificity: 100,
            attribution: Some(SourceAttributionProto {
                extension_id: "title_underwriter_rates".to_string(),
                source_name: source_name.to_string(),
                source_url: source_url.to_string(),
                provenance_tier: provenance as i32,
                fetched_at_rfc3339: now_iso(),
                schedule_or_version: Some(regime.to_string()),
                notes: Some(serialized),
            }),
            latency_ms: 0,
            cached: false,
            payload: Some(Payload::InsuranceQuote(quote)),
        }])
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        _http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move { self.execute_sync(query) })
    }
}

// =================================================================================================
// Unit Tests (100% Real Fixtures + Statutory Rate Verification + Zero-Mock/Zero-PII Guarantees)
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::manifest::parse_and_validate_package;
    use std::path::PathBuf;

    #[test]
    fn empty_report_emits_null_dollars_and_never_claims_zero_liens() {
        let radar = build_title_legal_radar_deterministic(&json!({}));

        let rate = &radar["underwriterRateFiling"];
        assert!(rate["basisPriceUsd"].is_null());
        assert!(rate["ownerStandardUsd"].is_null());
        assert!(rate["ownerEnhancedUsd"].is_null());
        assert!(rate["simultaneousLenderUsd"].is_null());
        assert!(rate["reissueSavingsUsd"].is_null());
        assert!(rate["escrowSettlementUsd"].is_null());

        let liens = &radar["recordedLiensAndEncumbrances"];
        assert_eq!(
            liens["coverageStatus"].as_str(),
            Some("assessor_and_disclosures_only")
        );
        assert_eq!(liens["requiresPrelimVerification"].as_bool(), Some(true));
        assert_eq!(liens["verifiedZeroLiens"].as_bool(), Some(false));

        let dumped = radar.to_string().to_ascii_lowercase();
        assert!(
            !dumped.contains("0 liens on title"),
            "must never claim '0 liens on title' without an uploaded Prelim"
        );
    }

    #[test]
    fn statutory_promulgated_rates_exact_tx_fl_nm_and_all_50_states_dc() {
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 15).unwrap();

        // 1. Florida exact statutory formula: $500,000 -> $575 (first $100k @ $5.75/$1k) + $2,000 ($400k @ $5.00/$1k) = $2,575
        let fl = compute_state_title_rate_filing(
            &json!({ "state": "FL", "county": "Miami-Dade County", "countyFips": "12086" }),
            &json!({ "lastSale": { "date": "2023-06-01", "price": 420000 } }),
            Some(500_000.0),
            as_of,
        );
        assert_eq!(fl["rateRegime"], "promulgated_statutory");
        assert_eq!(fl["ownerStandardUsd"], 2_575);
        assert_eq!(fl["reissueEligible"], true);
        assert!(fl["reissueSavingsUsd"].as_i64().unwrap_or(0) > 500);

        // 2. Texas TDI Basic Manual R-1 (Commissioner Order No. 2025-9697, eff. March 1, 2026):
        //    $500,000 -> $780 (first $100k) + 400 * $4.94 ($1,976) = $2,756
        let tx = compute_state_title_rate_filing(
            &json!({ "state": "TX", "county": "Travis County", "countyFips": "48453" }),
            &json!({}),
            Some(500_000.0),
            as_of,
        );
        assert_eq!(tx["rateRegime"], "promulgated_statutory");
        assert_eq!(tx["rateScheduleBasis"], "statutory_formula");
        assert_eq!(tx["ownerStandardUsd"], 2_756);
        assert_eq!(tx["simultaneousLenderUsd"], 195);

        // 2b. Ohio OTIRB PR-1 (eff. January 1, 2026):
        //     $500,000 -> 250 * $5.80 ($1,450) + 250 * $4.10 ($1,025) = $2,475
        let oh = compute_state_title_rate_filing(
            &json!({ "state": "OH", "county": "Franklin County", "countyFips": "39049" }),
            &json!({}),
            Some(500_000.0),
            as_of,
        );
        assert_eq!(oh["rateRegime"], "state_filed_tariff");
        assert_eq!(oh["rateScheduleBasis"], "rating_bureau_manual");
        assert_eq!(oh["ownerStandardUsd"], 2_475);

        // 3. New Mexico 13.14.9.18 NMAC exact promulgated formula: $500,000 -> $679 + 400 * $4.74 ($1,896) = $2,575
        let nm = compute_state_title_rate_filing(
            &json!({ "state": "NM", "county": "Bernalillo County" }),
            &json!({ "lastSale": { "date": "2020-05-10", "price": 380000 } }),
            Some(500_000.0),
            as_of,
        );
        assert_eq!(nm["rateRegime"], "promulgated_statutory");
        assert_eq!(nm["ownerStandardUsd"], 2_575);
        assert_eq!(nm["reissueEligible"], true);

        // 4. Verify all 50 states + DC return valid positive rates and honest regime strings
        let all_codes = [
            "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL",
            "IN", "IA", "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE",
            "NV", "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD",
            "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
        ];
        assert_eq!(all_codes.len(), 51);
        for code in all_codes {
            let out = compute_state_title_rate_filing(
                &json!({ "state": code }),
                &json!({}),
                Some(650_000.0),
                as_of,
            );
            let regime = out["rateRegime"].as_str().unwrap_or("");
            assert!(
                matches!(
                    regime,
                    "promulgated_statutory" | "state_filed_tariff" | "estimated_schedule"
                ),
                "unexpected rateRegime `{regime}` for {code}"
            );
            assert!(
                out["ownerStandardUsd"].as_i64().unwrap_or(0) >= 175,
                "expected positive ownerStandardUsd for {code}"
            );
        }
    }

    #[test]
    fn synth_campbell_and_sanjose_reports_surface_reissue_solar_and_code_enforcement() {
        let mut campbell = crate::test_synth::synth_report_secondary();
        campbell["facts"]["lastSale"] = json!({ "date": "2022-06-14", "price": 2_050_000 });
        campbell["facts"]["disclosures"] = json!({
            "codeEnforcement": {
                "openCount": 0,
                "closedCount": 2,
                "items": [
                    { "caseNumber": "SYNTH-CE-2020-01", "status": "Closed" },
                    { "caseNumber": "SYNTH-CE-2021-04", "status": "Closed" }
                ]
            }
        });
        let radar_c = build_title_legal_radar_deterministic(&campbell);

        // Sold ~4.4 yrs before 2026-11-01 -> CA 5-yr reissue eligible!
        assert_eq!(radar_c["underwriterRateFiling"]["state"], "CA");
        assert_eq!(
            radar_c["underwriterRateFiling"]["rateRegime"],
            "estimated_schedule"
        );
        assert_eq!(
            radar_c["underwriterRateFiling"]["rateScheduleBasis"],
            "modeled_regional_benchmark"
        );
        assert_eq!(radar_c["underwriterRateFiling"]["reissueEligible"], true);
        assert!(
            radar_c["underwriterRateFiling"]["reissueSavingsUsd"]
                .as_i64()
                .unwrap_or(0)
                > 300
        );

        // Solar PV panels in `facts.features` must trigger `installed_verify_ownership` UCC-1 check
        assert_eq!(
            radar_c["recordedLiensAndEncumbrances"]["solarOwnershipStatus"],
            "installed_verify_ownership"
        );
        assert_eq!(
            radar_c["recordedLiensAndEncumbrances"]["coverageStatus"],
            "assessor_and_disclosures_only"
        );
        assert_eq!(
            radar_c["recordedLiensAndEncumbrances"]["requiresPrelimVerification"],
            true
        );

        // 2 closed municipal code enforcement cases must be surfaced
        assert_eq!(
            radar_c["litigationAndDockets"]["codeEnforcement"]["openCount"],
            0
        );
        assert_eq!(
            radar_c["litigationAndDockets"]["codeEnforcement"]["closedCount"],
            2
        );
        // 1958 build -> SB-800 10-year window is false
        assert_eq!(
            radar_c["litigationAndDockets"]["sb800WarrantyActive"],
            false
        );

        // Synthetic San Jose report (sold ~1.7 yrs ago)
        let mut sj = crate::test_synth::synth_report_primary();
        sj["facts"]["lastSale"] = json!({ "date": "2025-03-05", "price": 1_980_000 });
        let radar_sj = build_title_legal_radar_deterministic(&sj);
        assert_eq!(radar_sj["underwriterRateFiling"]["reissueEligible"], true);
        assert_eq!(
            radar_sj["foreclosureAndDistress"]["distressStage"],
            "none_in_public_records"
        );
        assert!(radar_sj["foreclosureAndDistress"]["statutoryTimelineNote"]
            .as_str()
            .unwrap_or("")
            .contains("2924"));
    }

    #[test]
    fn sb800_and_zero_pii_court_docket_scrubber_enforced() {
        // 1. CA 2021 build -> within 10-year SB-800 Right to Repair Act window in 2026
        let recent_ca = json!({
            "meta": { "generatedAt": "2026-11-15T00:00:00Z" },
            "geo": { "state": "CA", "county": "Santa Clara County", "countyFips": "06085" },
            "facts": { "yearBuilt": 2021, "listPrice": 1500000 }
        });
        let radar = build_title_legal_radar_deterministic(&recent_ca);
        assert_eq!(radar["litigationAndDockets"]["sb800WarrantyActive"], true);
        assert_eq!(
            radar["litigationAndDockets"]["builderWarrantyRepose"]["expiresYear"],
            2031
        );

        // 2. Zero-PII docket title scrubber must strip all natural-person names while keeping institutional names
        let s1 = sanitize_court_docket_title(
            "Johnathan R. Miller and Sarah Miller v. Wells Fargo Bank, N.A.",
            Some("220 Foreclosure"),
        );
        assert!(!s1.contains("Miller"), "leaked personal surname: {s1}");
        assert!(
            !s1.contains("Johnathan"),
            "leaked personal given name: {s1}"
        );
        assert!(s1.contains("Wells Fargo Bank, N.A."));

        let s2 = sanitize_court_docket_title("In re Maria Elena Gonzalez", Some("Chapter 13"));
        assert!(!s2.contains("Maria") && !s2.contains("Gonzalez"));
        assert!(s2.contains("[Redacted Party]"));
    }

    #[test]
    fn all_four_title_legal_extension_manifests_validate_cleanly() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("extensions");
        for ext_id in [
            "county_recorder_liens",
            "preforeclosure_distress",
            "court_litigation_dockets",
            "title_underwriter_rates",
        ] {
            let p = base.join(ext_id).join("manifest.yaml");
            let yaml = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("missing {}: {e}", p.display()));
            let parsed = parse_and_validate_package(&yaml, None, Some(true))
                .unwrap_or_else(|e| panic!("manifest validation failed for {ext_id}: {e}"));
            assert_eq!(parsed.extension_id, ext_id);
        }
    }

    #[test]
    fn title_who_pays_ambiguous_states_emit_null_and_attorney_states_assign_buyer() {
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 15).unwrap();

        // Statewide CA without county FIPS resolves to `varies_by_county`: must NOT fabricate 50/50 split.
        let ca_statewide = compute_state_title_rate_filing(
            &json!({ "state": "CA" }),
            &json!({}),
            Some(900_000.0),
            as_of,
        );
        let ca_wp = &ca_statewide["whoPaysCustom"];
        assert_eq!(ca_wp["ownersPolicyPayer"], "varies_by_county");
        assert_eq!(ca_wp["whoPaysDeterministic"], false);
        assert!(ca_wp["buyerEstimatedTitleEscrowUsd"].is_null());
        assert!(ca_wp["sellerEstimatedTitleEscrowUsd"].is_null());

        // NY downstate (`buyer_and_lender_attorneys`) and NC (`closing_attorney`) assign closing costs to buyer cleanly.
        for st in ["NY", "NC"] {
            let geo = if st == "NY" {
                json!({ "state": "NY", "countyFips": "36061" })
            } else {
                json!({ "state": "NC" })
            };
            let out = compute_state_title_rate_filing(&geo, &json!({}), Some(600_000.0), as_of);
            let wp = &out["whoPaysCustom"];
            assert_eq!(
                wp["whoPaysDeterministic"], true,
                "expected deterministic allocation for {st}"
            );
            assert!(
                wp["buyerEstimatedTitleEscrowUsd"].as_i64().unwrap_or(0) > 0,
                "expected positive buyer share for {st}"
            );
        }
    }

    #[test]
    fn chicago_and_sf_open_data_proxies_report_assessor_or_violations_only() {
        let chi_outcome = LiveRecorderOutcome {
            checked: false,
            non_lien_proxy: true,
            source_name: Some("Cook County / City of Chicago Open Municipal & Clerk Data".to_string()),
            source_url: Some("https://datacatalog.cookcountyil.gov/".to_string()),
            records: vec![json!({
                "documentId": "12345",
                "documentType": "Municipal Building/Code Record (OPEN) — non-lien proxy",
                "recordedDate": "2025-02-01"
            })],
        };
        let sec = build_recorded_liens_section(
            &json!({}),
            &json!({ "state": "IL", "county": "Cook County", "countyFips": "17031", "city": "Chicago" }),
            &json!({}),
            &json!({}),
            Some(&chi_outcome),
        );
        assert_eq!(sec["coverageStatus"], "assessor_or_violations_only");
        assert!(sec["coverageSummary"]
            .as_str()
            .unwrap_or("")
            .contains("NOT a county grantor-grantee lien index"));

        // Legacy cached reports labeled `live_open_recorder_checked` from SF/Chicago open data must downgrade cleanly.
        let legacy_sf = json!({
            "geo": { "state": "CA", "county": "San Francisco County", "countyFips": "06075" },
            "facts": { "listPrice": 1_500_000 },
            "titleLegalRadar": {
                "recordedLiensAndEncumbrances": {
                    "coverageStatus": "live_open_recorder_checked",
                    "liveRecorderSource": "San Francisco Assessor-Recorder DataSF Open Roll"
                }
            }
        });
        let rebuilt = assemble_title_legal_radar(&legacy_sf, None, None, None);
        assert_eq!(
            rebuilt["recordedLiensAndEncumbrances"]["coverageStatus"],
            "assessor_and_disclosures_only"
        );
    }

    #[tokio::test]
    async fn courtlistener_missing_token_and_401_report_unauthenticated() {
        use std::collections::HashMap;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let http = HttpClient::new(None);
        let empty_env = EnvConfig::from_map(HashMap::new());
        let mut sources = Vec::new();
        let mut warnings = Vec::new();

        // 1. Default CourtListener URL without COURTLISTENER_API_TOKEN must never claim "offline/cached mode"
        let unauth_out = query_courtlistener_dockets(
            &http,
            &empty_env,
            "742 Synth Ave",
            "Synth City",
            &mut sources,
            &mut warnings,
        )
        .await;
        assert_eq!(unauth_out.status, LiveDocketStatus::Unauthenticated);
        let sec = build_litigation_and_dockets_section(
            &json!({}),
            &json!({ "state": "CA" }),
            &json!({ "yearBuilt": 2000 }),
            &json!({}),
            NaiveDate::from_ymd_opt(2026, 11, 15).unwrap(),
            Some(&unauth_out),
        );
        assert_eq!(sec["coverageStatus"], "courtlistener_unauthenticated");
        let note = sec["coverageNote"].as_str().unwrap_or("");
        assert!(note.contains("COURTLISTENER_API_TOKEN"));
        assert!(!note.to_ascii_lowercase().contains("offline/cached mode"));

        // 2. HTTP 401 from an overridden CourtListener URL also maps cleanly to `courtlistener_unauthenticated`
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let srv = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"detail":"Authentication credentials were not provided."}"#;
                let resp = format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let env_401 = EnvConfig::from_map(HashMap::from([(
            "COURTLISTENER_API_URL".to_string(),
            format!("http://{addr}/api/rest/v4/search/"),
        )]));
        let mut s2 = Vec::new();
        let mut w2 = Vec::new();
        let out_401 = query_courtlistener_dockets(
            &HttpClient::from_env(&env_401, None),
            &env_401,
            "742 Synth Ave",
            "Synth City",
            &mut s2,
            &mut w2,
        )
        .await;
        let _ = srv.await;
        assert_eq!(out_401.status, LiveDocketStatus::Unauthenticated);
        assert!(w2.iter().any(|w| w.contains("unauthenticated")));
    }

    #[test]
    fn title_underwriter_rates_adapter_matches_rate_filing() {
        use std::collections::HashMap;
        use crate::extensions::adapter::DataExtensionAdapter;
        use crate::extensions::proto::v1::ExtensionQueryProto;

        let adapter = TitleUnderwriterRatesAdapter;
        let manifest = adapter.manifest();
        assert_eq!(manifest.extension_id, "title_underwriter_rates");
        assert_eq!(manifest.domain, CapabilityDomain::DomainInsurance as i32);

        let tx_q = ExtensionQueryProto {
            state: "TX".to_string(),
            county_fips: "48453".to_string(),
            extra_params: HashMap::from([("price".to_string(), "500000".to_string())]),
            ..Default::default()
        };
        let tx_envs = adapter.execute_sync(&tx_q).expect("TX sync");
        assert_eq!(tx_envs.len(), 1);
        let tx_attr = tx_envs[0].attribution.as_ref().expect("attribution");
        assert_eq!(
            tx_attr.provenance_tier,
            ProvenanceTier::ProvenanceOfficialTariff as i32
        );
        let parsed: Value =
            serde_json::from_str(tx_attr.notes.as_deref().unwrap_or("{}")).expect("json");
        assert_eq!(parsed["ownerStandardUsd"], 2_756);
        assert_eq!(parsed["rateRegime"], "promulgated_statutory");
        let Some(Payload::InsuranceQuote(q)) = &tx_envs[0].payload else {
            panic!("expected InsuranceQuote payload");
        };
        assert!(q.estimated_annual_premium_usd.is_none());

        let ca_q = ExtensionQueryProto {
            state: "CA".to_string(),
            extra_params: HashMap::from([("price".to_string(), "500000".to_string())]),
            ..Default::default()
        };
        let ca_envs = adapter.execute_sync(&ca_q).expect("CA sync");
        assert_eq!(
            ca_envs[0].attribution.as_ref().unwrap().provenance_tier,
            ProvenanceTier::ProvenanceUnverified as i32
        );
    }
}
