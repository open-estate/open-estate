// [feat/deed-title-marketplace]
// Always-on Deed / Title / Title-Insurance Analyzer and Compliant 3-Vertical Marketplace
// (Mortgage, Home Insurance, Title & Settlement) for Open Estate.
//
// Compliance built in:
// - FHA anti-flipping (24 CFR 203.37a) <= 90d ineligibility & 91-180d >=100% markup 2nd appraisal
// - RESPA Section 8 (12 U.S.C. 2607): flat display or CPC only, no contingent referral fees
// - RESPA Section 9 (12 U.S.C. 2608): seller cannot condition sale on a particular title insurer
// - TILA / Reg Z (12 CFR 1026.24): APR shown alongside every interest rate (APR >= note rate)
// - Fair Housing & privacy-safe impression/click tracking: zero PII, zero IP stored

use crate::guardrails::GuardrailError;
use crate::registry::loan_limits::resolve_loan_limits_from_report_parts;
pub use crate::registry::rates_and_costs::{
    lookup_who_pays, rebuild_cost_per_sqft, state_effective_insurance_pct_of_price, WhoPaysRule,
    WHO_PAYS_LABEL, WHO_PAYS_TABLE,
};
use crate::shared::{round_n, EnvConfig};
use chrono::{NaiveDate, Utc};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SPONSORED_DISCLOSURE_TEXT: &str =
    "Sponsored placement. Not a commitment to lend or bind coverage. Equal Housing Opportunity.";
pub const COMPENSATION_MODEL: &str = "flat_display_or_cpc";
pub const BENCHMARK_NOTICE: &str = "Benchmark, not an offer";

static RE_QUITCLAIM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bquit\s*claim\b").expect("quitclaim regex"));
static RE_TRUSTEE_REO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:trustee['’]?s\s+deed(?:\s+upon\s+sale)?|sheriff['’]?s\s+deed|foreclosure\s+deed|reo\b|bank[- ]owned)\b")
        .expect("trustee reo regex")
});
static RE_TAX_DEED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:tax\s+deed|treasurer['’]?s\s+deed|tax[- ]sale\s+deed)\b")
        .expect("tax deed regex")
});
static RE_FAMILY_ESTATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:interspousal|probate|estate\s+deed|executor['’]?s?\s+deed|administrator['’]?s?\s+deed|personal\s+representative|affidavit\s+of\s+death|transfer\s+on\s+death|tod\s+deed|inherited|trust\s+transfer)\b")
        .expect("family estate regex")
});
static RE_INHERITED_EXEMPT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:inherit(?:ed|ance)|estate|probate|executor|administrator|devise|heir)\b")
        .expect("inherited regex")
});
static RE_HUD_EXEMPT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:hud\s+reo|hud\s+home|department\s+of\s+housing|va\s+reo|federal\s+agency|fannie\s+mae|freddie\s+mac)\b")
        .expect("hud exempt regex")
});
static RE_RELO_EXEMPT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:relocation|relo\s+company|employer\s+relocation)\b")
        .expect("relo exempt regex")
});
static RE_TAX_DELINQ: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:tax\s+default(?:ed)?|delinquent\s+(?:property\s+)?tax(?:es)?|unpaid\s+(?:prior[- ]year\s+)?tax(?:es)?|tax\s+lien|irs\s+lien|federal\s+tax\s+lien|state\s+tax\s+lien|franchise\s+tax\s+board)\b")
        .expect("tax delinq regex")
});
static RE_PACE_HERO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:pace\s+(?:assessment|lien|program)|hero\s+(?:assessment|lien|program)|ygrene|renovate\s+america|property\s+assessed\s+clean\s+energy)\b")
        .expect("pace hero regex")
});
static RE_EASEMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:easement|encroach(?:ment|ing)?|right[- ]of[- ]way|shared\s+driveway|boundary\s+dispute|setback\s+violation|avigation|pipeline|transmission\s+line|cc&r|covenant)\b")
        .expect("easement regex")
});

#[derive(Debug, Clone, Default)]
pub struct MarketplaceQueryOptions {
    pub vertical: Option<String>,
    pub price_override: Option<f64>,
    pub down_payment_pct: Option<f64>,
    pub occupancy: Option<String>,
    pub credit_score: Option<i64>,
    pub loan_term_years: Option<usize>,
    pub discount_points: Option<f64>,
    pub as_of_date: Option<NaiveDate>,
}

#[derive(Debug, Clone)]
pub struct DeedTitleMarketplaceOutput {
    pub title_insurance_and_deed: Value,
    pub marketplace: Value,
}

pub const MAX_CLICK_STORE_KEYS: usize = 50_000;

pub const CANONICAL_PRICE_TIERS: &[&str] = &[
    "under_400k",
    "400k_800k",
    "800k_1500k",
    "1500k_3m",
    "1500k_3000k",
    "3m_plus",
    "over_3000k",
    "unknown",
];

const MORTGAGE_BENCHMARK_IDS: &[&str] = &[
    "benchmark-30yr-fixed",
    "benchmark-15yr-fixed",
    "benchmark-7-6-sofr-arm",
    "benchmark-fha-35",
    "benchmark-va-0",
    "benchmark-jumbo-30yr",
    "benchmark-dscr-investor",
    "benchmark-sba-504",
];

const INSURANCE_BENCHMARK_IDS: &[&str] = &[
    "benchmark-ho3-admitted",
    "benchmark-ho5-admitted",
    "benchmark-insurtech-mga",
    "benchmark-fair-plan-dic",
    "benchmark-nfip-flood",
    "benchmark-private-flood",
    "benchmark-earthquake",
];

const TITLE_BENCHMARK_IDS: &[&str] = &[
    "benchmark-title-standard",
    "benchmark-title-enhanced",
    "benchmark-section-c-settlement",
];

#[derive(Debug, Default)]
pub struct ClickCounterEntry {
    pub impressions: AtomicU64,
    pub clicks: AtomicU64,
}

/// Privacy-safe impression and click counters keyed strictly by
/// `(vertical, partner_id, zip, price_tier)` with zero PII, zero IP stored,
/// and a bounded key capacity to prevent memory exhaustion.
#[derive(Debug)]
pub struct MarketplaceClickStore {
    counters: DashMap<(String, String, String, String), ClickCounterEntry>,
    max_keys: usize,
}

impl Default for MarketplaceClickStore {
    fn default() -> Self {
        Self::with_max_keys(MAX_CLICK_STORE_KEYS)
    }
}

impl MarketplaceClickStore {
    pub fn with_max_keys(max_keys: usize) -> Self {
        Self {
            counters: DashMap::new(),
            max_keys,
        }
    }

    pub fn len(&self) -> usize {
        self.counters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    pub fn try_record_event(
        &self,
        vertical: &str,
        partner_id: &str,
        zip: &str,
        price_tier: &str,
        is_click: bool,
    ) -> Option<(u64, u64)> {
        let key = (
            vertical.to_ascii_lowercase(),
            partner_id.to_string(),
            zip.to_string(),
            price_tier.to_string(),
        );
        if let Some(entry) = self.counters.get(&key) {
            if is_click {
                entry.clicks.fetch_add(1, Ordering::Relaxed);
            } else {
                entry.impressions.fetch_add(1, Ordering::Relaxed);
            }
            return Some((
                entry.impressions.load(Ordering::Relaxed),
                entry.clicks.load(Ordering::Relaxed),
            ));
        }
        if self.max_keys == 0 || self.counters.len() >= self.max_keys {
            return None;
        }
        let entry = self.counters.entry(key).or_default();
        if is_click {
            entry.clicks.fetch_add(1, Ordering::Relaxed);
        } else {
            entry.impressions.fetch_add(1, Ordering::Relaxed);
        }
        Some((
            entry.impressions.load(Ordering::Relaxed),
            entry.clicks.load(Ordering::Relaxed),
        ))
    }

    pub fn record_event(
        &self,
        vertical: &str,
        partner_id: &str,
        zip: &str,
        price_tier: &str,
        is_click: bool,
    ) -> (u64, u64) {
        self.try_record_event(vertical, partner_id, zip, price_tier, is_click)
            .unwrap_or((0, 0))
    }

    pub fn get_counts(
        &self,
        vertical: &str,
        partner_id: &str,
        zip: &str,
        price_tier: &str,
    ) -> (u64, u64) {
        let key = (
            vertical.to_ascii_lowercase(),
            partner_id.to_string(),
            zip.to_string(),
            price_tier.to_string(),
        );
        self.counters
            .get(&key)
            .map(|e| {
                (
                    e.impressions.load(Ordering::Relaxed),
                    e.clicks.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((0, 0))
    }

    pub fn record_impressions_for_marketplace(
        &self,
        marketplace: &Value,
        zip: &str,
        price_tier: &str,
    ) {
        let clean_zip = if zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit()) {
            zip
        } else {
            "00000"
        };
        for vertical in ["mortgage", "insurance", "title"] {
            for bucket in ["benchmarks", "sponsoredOffers"] {
                if let Some(arr) = marketplace
                    .pointer(&format!("/verticals/{vertical}/{bucket}"))
                    .and_then(|v| v.as_array())
                {
                    for row in arr {
                        if let Some(pid) = row
                            .get("partnerId")
                            .or_else(|| row.get("id"))
                            .and_then(|v| v.as_str())
                        {
                            self.record_event(vertical, pid, clean_zip, price_tier, false);
                        }
                    }
                }
            }
        }
    }

    pub fn stats_for_zip_tier(&self, zip: &str, price_tier: Option<&str>) -> Vec<Value> {
        let mut out = Vec::new();
        for item in self.counters.iter() {
            let (v, pid, z, pt) = item.key();
            if z == zip && price_tier.map(|t| t == pt).unwrap_or(true) {
                out.push(json!({
                    "vertical": v,
                    "partnerId": pid,
                    "zip": z,
                    "priceTier": pt,
                    "impressions": item.value().impressions.load(Ordering::Relaxed),
                    "clicks": item.value().clicks.load(Ordering::Relaxed),
                }));
            }
        }
        out.sort_by(|a, b| {
            let ka = (
                a["vertical"].as_str().unwrap_or(""),
                a["partnerId"].as_str().unwrap_or(""),
            );
            let kb = (
                b["vertical"].as_str().unwrap_or(""),
                b["partnerId"].as_str().unwrap_or(""),
            );
            ka.cmp(&kb)
        });
        out
    }
}

pub fn price_tier_for_price(price: f64) -> &'static str {
    if !price.is_finite() || price <= 0.0 {
        "unknown"
    } else if price < 400_000.0 {
        "under_400k"
    } else if price < 800_000.0 {
        "400k_800k"
    } else if price < 1_500_000.0 {
        "800k_1500k"
    } else if price < 3_000_000.0 {
        "1500k_3m"
    } else {
        "3m_plus"
    }
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
    for fmt in ["%Y-%m-%d", "%m/%d/%Y", "%Y/%m/%d"] {
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

fn rebuild_per_sqft(st: &str) -> f64 {
    rebuild_cost_per_sqft(st, None)
}

fn base_insurance_rate(st: &str) -> f64 {
    state_effective_insurance_pct_of_price(st)
}

fn resolve_county_limits(geo: &Value, facts: &Value, valuation: &Value) -> (i64, i64, i64, bool) {
    let lim = resolve_loan_limits_from_report_parts(geo, facts, Some(valuation));
    (
        lim.conforming_limit,
        lim.fha_limit,
        lim.baseline_limit,
        lim.is_high_cost_area,
    )
}

#[derive(Debug, Clone)]
struct TransferRecord {
    date_str: String,
    date: NaiveDate,
    price: Option<i64>,
    deed_type: String,
    note: String,
    is_transfer_like: bool,
}

fn extract_transfers(facts: &Value) -> Vec<TransferRecord> {
    let mut by_date: BTreeMap<String, TransferRecord> = BTreeMap::new();

    let mut insert_raw =
        |dt_raw: &str, price: Option<i64>, deed_type: &str, note: &str, event: &str| {
            let Some(date) = parse_flexible_date(dt_raw) else {
                return;
            };
            let norm_dt = date.format("%Y-%m-%d").to_string();
            let ev_lc = event.to_lowercase();
            let is_sale_or_transfer = ev_lc.is_empty()
                || ev_lc.contains("sold")
                || ev_lc.contains("sale")
                || ev_lc.contains("close")
                || ev_lc.contains("transfer")
                || ev_lc.contains("deed");
            if !is_sale_or_transfer {
                return;
            }
            let clean_deed = if deed_type.trim().is_empty() {
                if !event.trim().is_empty() && !event.eq_ignore_ascii_case("sold") {
                    event.trim().to_string()
                } else {
                    "Recorded sale / transfer".to_string()
                }
            } else {
                deed_type.trim().to_string()
            };
            by_date
                .entry(norm_dt.clone())
                .and_modify(|existing| {
                    if existing.price.is_none() && price.filter(|&p| p > 0).is_some() {
                        existing.price = price.filter(|&p| p > 0);
                    }
                    if existing.deed_type == "Recorded sale / transfer"
                        && clean_deed != "Recorded sale / transfer"
                    {
                        existing.deed_type = clean_deed.clone();
                    }
                    if existing.note.is_empty() && !note.trim().is_empty() {
                        existing.note = note.trim().to_string();
                    }
                })
                .or_insert_with(|| TransferRecord {
                    date_str: norm_dt,
                    date,
                    price: price.filter(|&p| p > 0),
                    deed_type: clean_deed,
                    note: note.trim().to_string(),
                    is_transfer_like: true,
                });
        };

    if let Some(arr) = facts
        .pointer("/titleAndLiens/transfers")
        .and_then(|v| v.as_array())
    {
        for tr in arr {
            let dt = tr.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let pr = tr.get("price").and_then(|v| v.as_i64());
            let dtype = tr.get("deedType").and_then(|v| v.as_str()).unwrap_or("");
            let note = tr.get("note").and_then(|v| v.as_str()).unwrap_or("");
            let ev = tr.get("event").and_then(|v| v.as_str()).unwrap_or("");
            insert_raw(dt, pr, dtype, note, ev);
        }
    }

    if let Some(arr) = facts.get("saleHistory").and_then(|v| v.as_array()) {
        for s in arr {
            let dt = s.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let pr = s.get("price").and_then(|v| v.as_i64());
            let dtype = s.get("deedType").and_then(|v| v.as_str()).unwrap_or("");
            let note = s.get("note").and_then(|v| v.as_str()).unwrap_or("");
            let ev = s.get("event").and_then(|v| v.as_str()).unwrap_or("");
            insert_raw(dt, pr, dtype, note, ev);
        }
    }

    for key in ["lastSale", "priorSale"] {
        if let Some(ls) = facts.get(key).and_then(|v| v.as_object()) {
            let dt = ls.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let pr = ls.get("price").and_then(|v| v.as_i64());
            let dtype = ls.get("deedType").and_then(|v| v.as_str()).unwrap_or("");
            let note = ls.get("note").and_then(|v| v.as_str()).unwrap_or("");
            insert_raw(dt, pr, dtype, note, "Sold");
        }
    }

    let mut list: Vec<TransferRecord> = by_date
        .into_values()
        .filter(|t| t.is_transfer_like)
        .collect();
    list.sort_by(|a, b| b.date.cmp(&a.date));
    list
}

fn extract_latest_mortgage_date(facts: &Value) -> Option<NaiveDate> {
    facts
        .pointer("/titleAndLiens/mortgages")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| {
            m.get("date")
                .and_then(|v| v.as_str())
                .and_then(parse_flexible_date)
        })
        .max()
}

fn subject_price_from(
    facts: &Value,
    valuation: &Value,
    market: &Value,
    price_override: Option<f64>,
) -> Option<f64> {
    if let Some(p) = price_override.filter(|p| p.is_finite() && *p > 0.0) {
        return Some(p);
    }
    let candidates = [
        facts.get("listPrice").and_then(|v| v.as_f64()),
        facts.get("price").and_then(|v| v.as_f64()),
        facts
            .pointer("/mlsListing/listPrice")
            .and_then(|v| v.as_f64()),
        facts.pointer("/listing/listPrice").and_then(|v| v.as_f64()),
        valuation
            .pointer("/estimate/value")
            .and_then(|v| v.as_f64()),
        market.pointer("/zhvi/latest").and_then(|v| v.as_f64()),
        facts.pointer("/lastSale/price").and_then(|v| v.as_f64()),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.is_finite() && *p > 0.0)
}

fn build_recorder_guidance(facts: &Value, geo: &Value) -> Value {
    let rec = facts.pointer("/titleAndLiens/recorderLinks");
    let county = geo
        .get("county")
        .and_then(|v| v.as_str())
        .unwrap_or("County");
    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let rec_name = rec
        .and_then(|r| r.get("recorderName"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("{county} {st} Clerk / Recorder").trim().to_string());
    let rec_url = rec
        .and_then(|r| r.get("recorderUrl"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            let raw_q = format!("{county} {st} county recorder property fraud alert");
            let q = urlencoding::encode(&raw_q);
            format!("https://www.google.com/search?q={q}")
        });

    json!({
        "countyRecorderService": {
            "name": rec_name,
            "url": rec_url,
            "cost": "Free",
            "recommendation": format!("Enroll in {rec_name}'s free recording-notification / property fraud alert service after closing to receive instant alerts whenever a deed, mortgage, or lien is indexed under your name or parcel.")
        },
        "paidTitleLockCritique": "Paid 'title lock' or deed-monitoring subscriptions add little value: third-party companies cannot lock county land records or block a recording, they only notify you after a fraudulent deed is already recorded (duplicating free county recorder alerts), and they do not provide title insurance indemnity or quiet-title legal defense.",
        "altaHomeownersProtection": "Unlike paid monitoring subscriptions or standard CLTA/ALTA owner's policies, an ALTA Homeowner's (Enhanced) Policy covers post-policy forgery and impersonation after closing—paying covered legal defense costs to clear forged deeds."
    })
}

fn push_flag(
    flags: &mut Vec<Value>,
    seen_codes: &mut HashSet<String>,
    risk_points: &mut i64,
    code: &str,
    severity: &str,
    points: i64,
    title: &str,
    detail: String,
    statute_or_basis: &str,
    mitigation: &str,
) {
    if seen_codes.insert(code.to_string()) {
        *risk_points += points;
        flags.push(json!({
            "code": code,
            "severity": severity,
            "title": title,
            "detail": detail,
            "basis": statute_or_basis,
            "mitigation": mitigation,
        }));
    }
}

/// Pure deterministic deed, ownership-chain, title-insurance, and closing-cost analysis
/// run before any Preliminary Title Report PDF is uploaded.
pub fn analyze_deed_and_title(
    facts: &Value,
    valuation: &Value,
    hazards: &Value,
    geo: &Value,
    as_of: NaiveDate,
) -> Value {
    analyze_deed_and_title_with_price(facts, valuation, hazards, geo, as_of, None)
}

fn analyze_deed_and_title_with_price(
    facts: &Value,
    valuation: &Value,
    hazards: &Value,
    geo: &Value,
    as_of: NaiveDate,
    price_override: Option<f64>,
) -> Value {
    let transfers = extract_transfers(facts);
    let subject_price = subject_price_from(facts, valuation, &Value::Null, price_override);
    let resale_eval_date = facts
        .get("resaleDate")
        .or_else(|| facts.get("pendingSaleDate"))
        .and_then(|v| v.as_str())
        .and_then(parse_flexible_date)
        .unwrap_or(as_of);

    let mut flags: Vec<Value> = Vec::new();
    let mut seen_codes: HashSet<String> = HashSet::new();
    let mut risk_points: i64 = if transfers.is_empty() { 18 } else { 10 };

    if transfers.is_empty() {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "NO_PUBLIC_DEED_HISTORY",
            "info",
            0,
            "Limited Public Deed History Before Prelim",
            "No recorded transfers supplied in public records; order a Preliminary Title Report (Prelim) to verify vesting and chain of title.".to_string(),
            "Pre-Prelim public records screening",
            "Request Preliminary Title Report and prior owner's policy from escrow.",
        );
    }

    // 1. FHA Anti-Flipping (24 CFR 203.37a)
    let mut fha_eligible = true;
    let mut fha_ineligible_reason: Option<String> = None;
    let mut second_appraisal_required = false;
    let mut second_appraisal_reason: Option<String> = None;
    let mut current_hold_days: Option<i64> = None;
    let mut current_markup_pct: Option<f64> = None;
    let mut possible_exemption: Option<String> = None;

    if let Some(latest) = transfers.first() {
        let hold_days = (resale_eval_date - latest.date).num_days();
        if hold_days >= 0 {
            current_hold_days = Some(hold_days);
            let combined_latest = format!("{} {}", latest.deed_type, latest.note);
            let fc_status = facts
                .pointer("/foreclosure/status")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if RE_INHERITED_EXEMPT.is_match(&combined_latest) {
                possible_exemption = Some("Inherited / estate transfer (potential 24 CFR 203.37a(c)(1) exemption if verified)".to_string());
            } else if RE_HUD_EXEMPT.is_match(&combined_latest) {
                possible_exemption = Some("HUD / federal / GSE REO sale (potential 24 CFR 203.37a(c) exemption if verified)".to_string());
            } else if RE_RELO_EXEMPT.is_match(&combined_latest) {
                possible_exemption = Some("Employer / relocation agency sale (potential 24 CFR 203.37a(c)(1) exemption if verified)".to_string());
            } else if RE_TRUSTEE_REO.is_match(&combined_latest) || fc_status == "reo_bank_owned" {
                possible_exemption = Some("Chartered lender / GSE REO post-foreclosure resale (potential 24 CFR 203.37a(c)(4) exemption)".to_string());
            }

            if let (Some(acq_p), Some(resale_p)) = (
                latest.price.filter(|&p| p > 0),
                subject_price.filter(|&p| p > 0.0),
            ) {
                current_markup_pct = Some(round_n(
                    ((resale_p - acq_p as f64) / acq_p as f64) * 100.0,
                    1,
                ));
            }

            if hold_days <= 90 {
                fha_eligible = false;
                let reason = format!(
                    "Seller acquired property {} days ago on {} (<= 90 days). Under 24 CFR 203.37a(b)(2), FHA-insured financing is ineligible until day 91 unless a statutory exception applies (e.g., inherited property, HUD REO, GSE/bank REO, or employer relocation).",
                    hold_days, latest.date_str
                );
                fha_ineligible_reason = Some(reason.clone());
                push_flag(
                    &mut flags,
                    &mut seen_codes,
                    &mut risk_points,
                    "FHA_FLIP_UNDER_90_DAYS",
                    "critical",
                    25,
                    "FHA Anti-Flipping Ineligible (<= 90-Day Hold)",
                    reason,
                    "24 CFR 203.37a(b)(2) & (c)",
                    "Use conventional/cash financing, wait until day 91+ after seller's acquisition to execute purchase contract, or obtain lender verification of a 24 CFR 203.37a(c) exception (inherited, HUD REO, bank REO, relocation).",
                );
            } else if (91..=180).contains(&hold_days) {
                if let (Some(acq_p), Some(resale_p)) = (
                    latest.price.filter(|&p| p > 0),
                    subject_price.filter(|&p| p > 0.0),
                ) {
                    if resale_p >= (acq_p as f64) * 2.0 {
                        let markup = current_markup_pct.unwrap_or(100.0);
                        second_appraisal_required = true;
                        let reason = format!(
                            "Resale at {} days after seller's {} acquisition with +{:.1}% price markup (>= 100% above seller's ${} purchase price). 24 CFR 203.37a(b)(3) requires a second independent FHA appraisal paid by the lender/seller plus documented rehab/value justification.",
                            hold_days,
                            latest.date_str,
                            markup,
                            acq_p
                        );
                        second_appraisal_reason = Some(reason.clone());
                        push_flag(
                            &mut flags,
                            &mut seen_codes,
                            &mut risk_points,
                            "FHA_FLIP_SECOND_APPRAISAL",
                            "major",
                            16,
                            "FHA Second Appraisal Required (91–180 Days, >=100% Markup)",
                            reason,
                            "24 CFR 203.37a(b)(3)",
                            "Require itemized contractor permits, receipts, and scope of renovations; expect a second independent appraisal and 1–2 extra weeks in underwriting.",
                        );
                    }
                }
            }
        }
    }

    // Also inspect consecutive pairs in recorded deed history for rapid flips
    for pair in transfers.windows(2) {
        let newer = &pair[0];
        let older = &pair[1];
        let gap_days = (newer.date - older.date).num_days();
        if (0..=90).contains(&gap_days) {
            push_flag(
                &mut flags,
                &mut seen_codes,
                &mut risk_points,
                "CHAIN_RAPID_FLIP_90D",
                "major",
                14,
                "Historical Rapid Flip in Transfer Chain (<= 90 Days)",
                format!(
                    "Prior deed chain shows a {} -> {} transfer in {} days. Rapid resales warrant closer inspection of unpermitted cosmetic work and mechanic's lien exposure.",
                    older.date_str, newer.date_str, gap_days
                ),
                "Ownership chain turnover",
                "Check city permit history for work performed during the short hold and require ALTA Homeowner's unrecorded mechanic's lien coverage.",
            );
        } else if (91..=180).contains(&gap_days) {
            if let (Some(p_old), Some(p_new)) = (older.price, newer.price) {
                if p_old > 0 && p_new >= p_old * 2 {
                    let pct = round_n(((p_new - p_old) as f64 / p_old as f64) * 100.0, 1);
                    push_flag(
                        &mut flags,
                        &mut seen_codes,
                        &mut risk_points,
                        "CHAIN_FLIP_100PCT_MARKUP",
                        "major",
                        12,
                        "Historical 91–180 Day Flip with >= 100% Markup",
                        format!(
                            "Recorded chain shows a resale {} days after acquisition ({} -> {}) with +{:.1}% price jump (${} -> ${}).",
                            gap_days, older.date_str, newer.date_str, pct, p_old, p_new
                        ),
                        "24 CFR 203.37a / Flip inspection",
                        "Verify building permits for all structural, electrical, plumbing, and roof work.",
                    );
                }
            }
        }
    }

    // 2. Transfer-type cloud signals
    let top_deed = facts
        .pointer("/titleAndLiens/deedType")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let top_vesting = facts
        .pointer("/titleAndLiens/vesting")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut transfer_blobs: Vec<String> = transfers
        .iter()
        .map(|t| format!("{} {} {}", t.date_str, t.deed_type, t.note))
        .collect();
    if !top_deed.is_empty() || !top_vesting.is_empty() {
        transfer_blobs.push(format!("{top_deed} {top_vesting}"));
    }
    let all_transfer_text = transfer_blobs.join(" | ");

    if RE_QUITCLAIM.is_match(&all_transfer_text) {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "QUITCLAIM_DEED_CLOUD",
            "major",
            18,
            "Quitclaim Deed in Ownership Chain",
            "A quitclaim deed conveys only the grantor's existing interest without title warranties or covenants against encumbrances. Title insurers often treat uninsured quitclaim deeds as potential cloud or duress/capacity flags.".to_string(),
            "Deed warranty / title insurability",
            "Confirm whether title insurer requires an Uninsured Deed Affidavit, spousal release, or full warranty/grant deed re-execution.",
        );
    }
    if RE_TRUSTEE_REO.is_match(&all_transfer_text)
        || facts
            .pointer("/foreclosure/status")
            .and_then(|v| v.as_str())
            .map(|s| s == "reo_bank_owned" || s == "pre_foreclosure_nod")
            .unwrap_or(false)
    {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "TRUSTEE_DEED_REO_CLOUD",
            "major",
            20,
            "Trustee's Deed / REO / Foreclosure Chain Signal",
            "Property history includes a Trustee's Deed Upon Sale, Sheriff's Deed, or REO/bank-owned transfer. Foreclosure-derived chains carry IRS 120-day tax-lien redemption risks, surviving municipal/HOA super-liens, and potential wrongful-foreclosure challenges.".to_string(),
            "Foreclosure chain & statutory redemption",
            "Require full title clearance of junior IRS liens, municipal code liens, HOA super-priority assessments, and vacant possession.",
        );
    }
    if RE_TAX_DEED.is_match(&all_transfer_text) {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "TAX_DEED_CLOUD",
            "critical",
            32,
            "Tax Deed / Tax Sale Chain Signal",
            "A tax deed or tax-defaulted sale transfer appears in the title chain. Tax deeds often require a judicial quiet-title action or statutory waiting period before standard title insurance can be issued.".to_string(),
            "Tax-sale due process & title insurability",
            "Confirm standard owner's title insurability early with the title officer (or budget for a quiet-title action / specialty tax-title certification).",
        );
    }
    if RE_FAMILY_ESTATE.is_match(&all_transfer_text) {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "FAMILY_OR_ESTATE_TRANSFER_CLOUD",
            "moderate",
            12,
            "Interspousal / Probate / Estate Transfer Signal",
            "Chain includes an interspousal, probate, executor/administrator, affidavit of death, or trust/TOD transfer. Requires verifying final probate court authority, estate tax clearance, or marital property release.".to_string(),
            "Probate / marital / estate authority",
            "Verify court letters testamentary / order confirming sale, trust certification, or interspousal release in Prelim Schedule A/B.",
        );
    }

    // 3. Tax delinquency, PACE/HERO, involuntary liens & easements
    let mut lien_texts: Vec<String> = Vec::new();
    for path in [
        "/titleAndLiens/involuntaryLiens",
        "/titleAndLiens/easementsAndRestrictions",
    ] {
        if let Some(arr) = facts.pointer(path).and_then(|v| v.as_array()) {
            for item in arr {
                lien_texts.push(item.to_string());
            }
        }
    }
    if let Some(arr) = facts.get("taxHistory").and_then(|v| v.as_array()) {
        for t in arr {
            if t.get("delinquent").and_then(|v| v.as_bool()) == Some(true)
                || t.get("taxDefaulted").and_then(|v| v.as_bool()) == Some(true)
            {
                lien_texts.push("delinquent property taxes".to_string());
            }
            if let Some(note) = t.get("note").and_then(|v| v.as_str()) {
                lien_texts.push(note.to_string());
            }
        }
    }
    if let Some(ln) = facts.get("localNotes") {
        lien_texts.push(ln.to_string());
    }
    if let Some(disc) = facts.get("disclosures") {
        // Extract only parcel-specific disclosure signals, never generic buyer checklists like
        // `brokerPacketGuide` which mention "easements" as boilerplate questions and cause false positives.
        match disc {
            Value::String(s) => lien_texts.push(s.clone()),
            Value::Array(arr) => {
                for item in arr {
                    lien_texts.push(item.to_string());
                }
            }
            Value::Object(map) => {
                for (k, v) in map {
                    if matches!(
                        k.as_str(),
                        "brokerPacketGuide"
                            | "permitPortal"
                            | "nhdZones"
                            | "pastListings"
                            | "permits"
                            | "permitNotes"
                            | "disclaimer"
                    ) {
                        continue;
                    }
                    if k == "statutoryChecklist" {
                        if let Some(arr) = v.as_array() {
                            for item in arr {
                                let status = item
                                    .get("status")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("");
                                let flagged = item
                                    .get("disclosed")
                                    .and_then(|b| b.as_bool())
                                    .unwrap_or(false)
                                    || item
                                        .get("flagged")
                                        .and_then(|b| b.as_bool())
                                        .unwrap_or(false)
                                    || matches!(
                                        status,
                                        "disclosed" | "flagged" | "present" | "yes" | "active"
                                    );
                                if flagged {
                                    lien_texts.push(item.to_string());
                                }
                            }
                        }
                        continue;
                    }
                    lien_texts.push(v.to_string());
                }
            }
            _ => {}
        }
    }
    if let Some(fc) = facts.get("foreclosure") {
        if fc.get("isDistressed").and_then(|v| v.as_bool()) == Some(true) {
            lien_texts.push(fc.to_string());
        }
    }
    let combined_lien_blob = lien_texts.join(" | ");

    if RE_TAX_DELINQ.is_match(&combined_lien_blob) {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "TAX_DELINQUENCY_RISK",
            "critical",
            28,
            "Property Tax Delinquency / Tax Lien Signal",
            "Supplied records indicate delinquent property taxes, tax default, or a federal/state tax lien. Delinquent property taxes hold super-priority ahead of mortgages and accrue steep statutory penalties.".to_string(),
            "Super-priority property/tax lien",
            "Obtain an official County Tax Collector redemption/payoff statement and require full payoff from seller proceeds at closing.",
        );
    }

    if RE_PACE_HERO.is_match(&combined_lien_blob) {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "PACE_HERO_ASSESSMENT",
            "critical",
            26,
            "PACE / HERO Super-Priority Assessment Lien",
            "Records reference a PACE or HERO clean-energy property tax assessment. PACE assessments carry super-priority over first-lien mortgages, causing Fannie Mae, Freddie Mac, and FHA to prohibit financing unless paid off at closing.".to_string(),
            "Super-priority special tax assessment",
            "Verify exact PACE/HERO payoff balance on the tax roll and negotiate seller payoff in escrow prior to funding.",
        );
    }

    // Check easements & encroachment exposure (from titleAndLiens, checks, hazards/footprint)
    let has_explicit_easement = RE_EASEMENT.is_match(&combined_lien_blob)
        || facts
            .pointer("/titleAndLiens/easementsAndRestrictions")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
    let has_footprint_or_boundary_note = facts
        .get("checks")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|c| {
                let ok = c.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
                let txt = c
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                !ok || txt.contains("encroach")
                    || txt.contains("easement")
                    || txt.contains("setback")
            })
        })
        .unwrap_or(false);

    if has_explicit_easement || has_footprint_or_boundary_note {
        let sev = if combined_lien_blob.to_lowercase().contains("encroach")
            || has_footprint_or_boundary_note
        {
            "major"
        } else {
            "moderate"
        };
        let pts = if sev == "major" { 15 } else { 8 };
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "EASEMENT_ENCROACHMENT_EXPOSURE",
            sev,
            pts,
            "Easement / Encroachment / Covenant Exposure",
            "Records indicate recorded easements, CC&Rs, shared-access rights, or potential structure/lot-line boundary conflicts that can restrict additions, pools, or ADUs.".to_string(),
            "Schedule B title exceptions & boundary survey",
            "Pull underlying Schedule B easement instruments, map easement corridors against planned improvements, and consider an ALTA Homeowner's Policy for encroachment protection.",
        );
    } else {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "PRE_PRELIM_EASEMENT_SCREEN",
            "info",
            2,
            "Unverified Easement & Boundary Encroachment Exposure",
            "Before a Preliminary Title Report and boundary survey are reviewed, utility/drainage easements, shared driveways, and fence/structure encroachments remain unconfirmed.".to_string(),
            "Pre-Prelim title & survey diligence",
            "Review Schedule B exceptions on the Prelim and verify fence/structure setbacks; ALTA Homeowner's policies cover certain unknown encroachments subject to sublimits.",
        );
    }

    // Turnover check: >= 3 transfers in 5 years
    let transfers_5y = transfers
        .iter()
        .filter(|t| (resale_eval_date - t.date).num_days() <= 1826)
        .count();
    if transfers_5y >= 3 {
        push_flag(
            &mut flags,
            &mut seen_codes,
            &mut risk_points,
            "HIGH_TURNOVER_CHAIN",
            "moderate",
            10,
            "High Ownership Turnover (3+ Transfers in 5 Years)",
            format!("{transfers_5y} recorded transfers occurred within the past 5 years; verify unpermitted repairs, disclosure gaps, and prior mechanic's liens."),
            "Chain-of-title velocity",
            "Request 5-year insurance loss run (CLUE report) and municipal permit records.",
        );
    }

    let risk_score = risk_points.clamp(0, 100);
    let risk_level = match risk_score {
        0..=24 => "low",
        25..=49 => "moderate",
        50..=74 => "elevated",
        _ => "high",
    };

    // Title insurance & closing analyzer
    let title_ins = analyze_title_insurance_and_closing(
        facts,
        valuation,
        hazards,
        geo,
        as_of,
        &transfers,
        subject_price,
    );

    let recorder_guidance = build_recorder_guidance(facts, geo);

    json!({
        "prelimUploaded": false,
        "asOfDate": as_of.format("%Y-%m-%d").to_string(),
        "deedAnalysis": {
            "riskScore": risk_score,
            "riskLevel": risk_level,
            "riskScoreNote": "0-100 deed & pre-Prelim title risk index (0 = lowest public-record risk, 100 = severe title/lien cloud).",
            "recordedTransferCount": transfers.len(),
            "latestTransferDate": transfers.first().map(|t| t.date_str.clone()),
            "latestTransferPrice": transfers.first().and_then(|t| t.price),
            "latestDeedType": transfers.first().map(|t| t.deed_type.clone()),
            "fhaAntiFlipping": {
                "rule": "24 CFR 203.37a",
                "fhaEligible": fha_eligible,
                "ineligibleReason": fha_ineligible_reason,
                "secondAppraisalRequired": second_appraisal_required,
                "secondAppraisalReason": second_appraisal_reason,
                "sellerHoldDays": current_hold_days,
                "resaleMarkupPct": current_markup_pct,
                "possibleExemption": possible_exemption,
                "exceptions": [
                    "Inherited properties acquired by devise or estate distribution (24 CFR 203.37a(c)(1))",
                    "HUD REO and other federal agency sales (VA, USDA) (24 CFR 203.37a(c)(2)-(3))",
                    "Properties acquired by state/federally chartered lenders or GSEs via foreclosure/deed-in-lieu (24 CFR 203.37a(c)(4))",
                    "Employer or relocation-company sales for employee transfers",
                    "HUD-approved nonprofit agency sales & newly constructed never-occupied homes",
                    "Presidentially Declared Major Disaster Areas during active HUD waiver periods"
                ]
            },
            "flags": flags,
            "deedFraudGuidance": recorder_guidance,
        },
        "titleInsurance": title_ins,
        "disclaimer": "Pre-Prelim public-record deed, title-insurance custom, and closing-cost estimator only — not legal advice, a title abstract, or a title insurance commitment. Confirm all exceptions, rates, and fee splits with your title officer and escrow.",
    })
}

fn estimate_standard_owner_premium(price: f64, rate_regime: &str) -> i64 {
    if !price.is_finite() || price <= 0.0 {
        return 0;
    }
    // Realistic U.S. tiered title premium curve (~0.32% to 0.58% depending on price & regime)
    let raw = if rate_regime == "promulgated" {
        // TX/FL/NM promulgated schedules run slightly higher on mid-price tiers
        if price <= 100_000.0 {
            830.0
        } else {
            830.0 + (price - 100_000.0) * 0.0052
        }
    } else if price <= 250_000.0 {
        350.0 + price * 0.0038
    } else if price <= 1_000_000.0 {
        1_300.0 + (price - 250_000.0) * 0.0029
    } else {
        3_475.0 + (price - 1_000_000.0) * 0.0021
    };
    raw.round().max(650.0) as i64
}

fn estimate_concurrent_lender_premium(loan: f64, rate_regime: &str) -> i64 {
    if !loan.is_finite() || loan <= 0.0 {
        return 0;
    }
    if rate_regime == "promulgated" {
        // Promulgated simultaneous-issue rate + mandatory title endorsements (T-19 / T-30 / EPA)
        (175.0 + (loan / 1_000_000.0) * 260.0).round() as i64
    } else {
        (450.0 + (loan * 0.00055)).round().clamp(450.0, 2_400.0) as i64
    }
}

fn analyze_title_insurance_and_closing(
    facts: &Value,
    _valuation: &Value,
    _hazards: &Value,
    geo: &Value,
    as_of: NaiveDate,
    transfers: &[TransferRecord],
    subject_price: Option<f64>,
) -> Value {
    let valid_price = subject_price.filter(|p| p.is_finite() && *p > 0.0);
    let rule = lookup_who_pays(geo);

    // Reissue / short-term rate discount (within ~3-5 years of prior sale or refinance)
    let latest_transfer_dt = transfers.first().map(|t| t.date);
    let latest_mtg_dt = extract_latest_mortgage_date(facts);
    let latest_policy_event = match (latest_transfer_dt, latest_mtg_dt) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let (reissue_eligible, years_since_prior, discount_pct, discount_range, reissue_note) =
        if let Some(ev_dt) = latest_policy_event {
            let days = (as_of - ev_dt).num_days().max(0);
            let yrs = round_n(days as f64 / 365.25, 2);
            if yrs <= 2.0 {
                (
                    true,
                    Some(yrs),
                    30.0,
                    [20, 40],
                    format!("Prior sale/refinance {:.1} years ago (<= 2 yrs): typically eligible for 20%–40% short-term/reissue title rate discount (subject to insurer state rate filing and copy of prior policy).", yrs),
                )
            } else if yrs <= 3.0 {
                (
                    true,
                    Some(yrs),
                    25.0,
                    [15, 35],
                    format!("Prior sale/refinance {:.1} years ago (<= 3 yrs): typically eligible for 15%–35% reissue rate discount (subject to insurer state rate filing and prior policy proof).", yrs),
                )
            } else if yrs <= 5.0 {
                (
                    true,
                    Some(yrs),
                    15.0,
                    [10, 20],
                    format!("Prior sale/refinance {:.1} years ago (3–5 yrs): may qualify for a 10%–20% reissue rate discount depending on state rate filing.", yrs),
                )
            } else {
                (
                    false,
                    Some(yrs),
                    0.0,
                    [0, 0],
                    format!("Last recorded sale/mortgage was {:.1} years ago (> 5 yrs); standard rate applies unless seller refinanced more recently.", yrs),
                )
            }
        } else {
            (
                false,
                None,
                0.0,
                [0, 0],
                "No prior sale or refinance date on file; ask seller for any title policy issued within the past 3–5 years to check reissue discount eligibility.".to_string(),
            )
        };

    let table_json: Vec<Value> = WHO_PAYS_TABLE
        .iter()
        .map(|r| {
            json!({
                "regionKey": r.region_key,
                "regionName": r.region_name,
                "state": r.state,
                "countyFips": r.county_fips,
                "ownersPolicyPayer": r.owners_policy_payer,
                "lendersPolicyPayer": r.lenders_policy_payer,
                "escrowFeePayer": r.escrow_fee_payer,
                "rateRegime": r.rate_regime,
                "closingModel": r.closing_model,
                "customNote": r.custom_note,
                "label": r.label,
            })
        })
        .collect();

    let respa_sec_9 = json!({
        "statute": "RESPA Section 9 (12 U.S.C. § 2608)",
        "sellerCannotRequireTitleInsurer": true,
        "summary": "Under RESPA Section 9 (12 U.S.C. § 2608), a seller cannot require a homebuyer—directly or indirectly—as a condition of sale to purchase title insurance from any particular title company when a federally related mortgage is used. Violations trigger liability up to 3× all title charges.",
    });

    let customary_who_pays = json!({
        "label": WHO_PAYS_LABEL,
        "matchedRegion": {
            "regionKey": rule.region_key,
            "regionName": rule.region_name,
            "state": rule.state,
            "countyFips": rule.county_fips,
            "ownersPolicyPayer": rule.owners_policy_payer,
            "lendersPolicyPayer": rule.lenders_policy_payer,
            "escrowFeePayer": rule.escrow_fee_payer,
            "rateRegime": rule.rate_regime,
            "closingModel": rule.closing_model,
            "customNote": rule.custom_note,
            "label": rule.label,
        },
        "table": table_json,
    });

    let Some(price) = valid_price else {
        return json!({
            "status": "missing_required_input",
            "basisPrice": Value::Null,
            "basisLoan80Ltv": Value::Null,
            "policyComparison": {
                "lendersPolicy": {
                    "name": "Lender's Title Policy (ALTA Loan Policy)",
                    "protects": "Lender only (mortgagee lien priority up to unpaid loan principal)",
                    "buyerEquityProtected": false,
                    "estimatedConcurrentPremiumUsd": Value::Null,
                    "covers": ["Prior undisclosed liens affecting lender priority", "Defective mortgage execution or recording"],
                    "doesNotCover": ["Buyer's down payment or home equity", "Post-closing deed forgery or boundary claims"]
                },
                "ownersStandardPolicy": {
                    "name": "Owner's Standard Policy (CLTA / ALTA Owner's Policy)",
                    "protects": "Buyer / owner equity up to purchase price as of policy recording date",
                    "buyerEquityProtected": true,
                    "estimatedPremiumUsd": Value::Null,
                    "premiumWithReissueUsd": Value::Null,
                    "covers": ["Forged or defective prior deeds before closing", "Undisclosed prior liens, tax liens, or missing heirs", "Lack of legal right of access (ALTA)"],
                    "doesNotCover": ["Post-policy forgery or deed fraud after closing", "Unrecorded mechanic's liens or unknown boundary encroachments", "Pre-existing permit or zoning violations"]
                },
                "altaHomeownersPolicy": {
                    "name": "ALTA Homeowner's Policy (Enhanced / Eagle Owner's Policy)",
                    "protects": "Buyer / owner equity with expanded & post-policy coverages (auto-inflation up to ~150%)",
                    "buyerEquityProtected": true,
                    "premiumMarkupPct": 12.5,
                    "markupRangePct": [10, 15],
                    "estimatedPremiumUsd": Value::Null,
                    "premiumWithReissueUsd": Value::Null,
                    "covers": [
                        "All Standard Owner's Policy risks",
                        "Post-policy forgery & impersonation (deed fraud after closing)",
                        "Encroachments onto or from neighboring land (subject to sublimits)",
                        "Unrecorded mechanic's liens for pre-closing work",
                        "Limited building-permit & zoning violation coverage (with sublimits & deductibles)",
                        "Subdivision & restrictive covenant violation protections (with sublimits)"
                    ],
                    "sublimitsNote": "Permit, zoning, and encroachment coverages carry policy-specific sublimits and deductibles (commonly $5,000–$25,000); typically ~10%–15% above standard owner's premium."
                }
            },
            "customaryWhoPays": customary_who_pays,
            "reissueDiscount": {
                "eligible": reissue_eligible,
                "yearsSincePriorSaleOrRefi": years_since_prior,
                "typicalDiscountRangePct": [10, 40],
                "matchedRangePct": discount_range,
                "estimatedDiscountPct": discount_pct,
                "estimatedSavingsStandardUsd": Value::Null,
                "estimatedSavingsEnhancedUsd": Value::Null,
                "dependsOnRateFiling": true,
                "note": reissue_note,
            },
            "respaSection9": respa_sec_9,
            "sectionCShoppableServices": Value::Null,
        });
    };

    let loan_80 = (price * 0.80).round();
    let title_filing = crate::modules::title_legal_radar::compute_state_title_rate_filing(
        geo,
        facts,
        Some(price),
        as_of,
    );
    let radar_rate_regime = title_filing["rateRegime"]
        .as_str()
        .unwrap_or(rule.rate_regime);
    let rate_schedule_basis = title_filing["rateScheduleBasis"]
        .as_str()
        .unwrap_or("modeled_regional_benchmark");
    let std_owner_prem = title_filing["ownerStandardUsd"]
        .as_i64()
        .filter(|&p| p > 0)
        .unwrap_or_else(|| estimate_standard_owner_premium(price, rule.rate_regime));
    let enhanced_owner_prem = ((std_owner_prem as f64) * 1.125).round() as i64; // +12.5% (10-15% range)
    let concurrent_lender_prem = title_filing["simultaneousLenderUsd"]
        .as_i64()
        .filter(|&p| p >= 0)
        .unwrap_or_else(|| estimate_concurrent_lender_premium(loan_80, rule.rate_regime));
    let reissue_savings_std = ((std_owner_prem as f64) * (discount_pct / 100.0)).round() as i64;
    let reissue_savings_enh =
        ((enhanced_owner_prem as f64) * (discount_pct / 100.0)).round() as i64;

    // Loan Estimate Section C ("Services You Can Shop For") + RESPA Section 9
    let buyer_pays_owners =
        rule.owners_policy_payer == "buyer" || rule.owners_policy_payer == "split_50_50";
    let is_promulgated = matches!(radar_rate_regime, "promulgated_statutory" | "promulgated");
    let is_attorney_state = rule.closing_model.to_lowercase().contains("attorney");

    let (exam_low, exam_typ, exam_high) = (250_i64, 425_i64, 595_i64);
    let (settle_low, settle_typ, settle_high) = if is_attorney_state {
        (950_i64, 1_350_i64, 1_850_i64)
    } else {
        (750_i64, 1_150_i64, 1_650_i64)
    };
    let (lender_pol_low, lender_pol_typ, lender_pol_high) = if is_promulgated {
        (
            concurrent_lender_prem,
            concurrent_lender_prem,
            concurrent_lender_prem,
        )
    } else {
        (
            (concurrent_lender_prem as f64 * 0.80).round() as i64,
            concurrent_lender_prem,
            (concurrent_lender_prem as f64 * 1.25).round() as i64,
        )
    };
    let (owner_pol_low, owner_pol_typ, owner_pol_high) = if buyer_pays_owners && !is_promulgated {
        (
            (std_owner_prem as f64 * 0.85).round() as i64,
            std_owner_prem,
            (std_owner_prem as f64 * 1.15).round() as i64,
        )
    } else {
        (0_i64, 0_i64, 0_i64)
    };
    let (courier_low, courier_typ, courier_high) = (95_i64, 195_i64, 325_i64);

    let sec_c_low = exam_low + settle_low + lender_pol_low + owner_pol_low + courier_low;
    let sec_c_typ = exam_typ + settle_typ + lender_pol_typ + owner_pol_typ + courier_typ;
    let sec_c_high = exam_high + settle_high + lender_pol_high + owner_pol_high + courier_high;
    let sec_c_savings = (sec_c_typ - sec_c_low).max(250);
    let sec_c_max_spread = (sec_c_high - sec_c_low).max(sec_c_savings + 200);

    let mut section_c_items = vec![
        json!({
            "service": "Title Search & Examination",
            "lowUsd": exam_low,
            "typicalUsd": exam_typ,
            "highUsd": exam_high,
            "shoppable": true,
        }),
        json!({
            "service": if is_attorney_state { "Closing Attorney / Settlement Fee" } else { "Settlement / Escrow Closing Fee" },
            "lowUsd": settle_low,
            "typicalUsd": settle_typ,
            "highUsd": settle_high,
            "shoppable": true,
        }),
        json!({
            "service": "Lender's Title Policy (Concurrent Issue)",
            "lowUsd": lender_pol_low,
            "typicalUsd": lender_pol_typ,
            "highUsd": lender_pol_high,
            "shoppable": !is_promulgated,
            "note": if is_promulgated { "Base premium promulgated by state; shop settlement & endorsement fees." } else { "Competitive filed rates vary by underwriter & agent." },
        }),
        json!({
            "service": "Notary, E-Recording & Wire / Courier Coordination",
            "lowUsd": courier_low,
            "typicalUsd": courier_typ,
            "highUsd": courier_high,
            "shoppable": true,
        }),
    ];
    if buyer_pays_owners && !is_promulgated {
        section_c_items.push(json!({
            "service": "Owner's Title Policy (Where Buyer Pays by Local Custom)",
            "lowUsd": owner_pol_low,
            "typicalUsd": owner_pol_typ,
            "highUsd": owner_pol_high,
            "shoppable": true,
        }));
    }

    let title_rate_basis_note = if is_promulgated {
        "Calculated from state-promulgated statutory title insurance rate formula."
    } else if radar_rate_regime == "state_filed_tariff" {
        "Calculated from state-filed title rating bureau tariff manual."
    } else {
        "Estimated regional benchmark schedule (insurer-filed or competitive schedule applies; verify with written Closing Disclosure or Title Fee Sheet)."
    };

    json!({
        "status": "ok",
        "basisPrice": price.round() as i64,
        "basisLoan80Ltv": loan_80 as i64,
        "rateRegime": radar_rate_regime,
        "rateScheduleBasis": rate_schedule_basis,
        "escrowSettlementUsd": title_filing["escrowSettlementUsd"].clone(),
        "titleRateBasisNote": title_rate_basis_note,
        "policyComparison": {
            "lendersPolicy": {
                "name": "Lender's Title Policy (ALTA Loan Policy)",
                "protects": "Lender only (mortgagee lien priority up to unpaid loan principal)",
                "buyerEquityProtected": false,
                "estimatedConcurrentPremiumUsd": concurrent_lender_prem,
                "covers": ["Prior undisclosed liens affecting lender priority", "Defective mortgage execution or recording"],
                "doesNotCover": ["Buyer's down payment or home equity", "Post-closing deed forgery or boundary claims"]
            },
            "ownersStandardPolicy": {
                "name": "Owner's Standard Policy (CLTA / ALTA Owner's Policy)",
                "protects": "Buyer / owner equity up to purchase price as of policy recording date",
                "buyerEquityProtected": true,
                "estimatedPremiumUsd": std_owner_prem,
                "premiumWithReissueUsd": (std_owner_prem - reissue_savings_std).max(0),
                "covers": ["Forged or defective prior deeds before closing", "Undisclosed prior liens, tax liens, or missing heirs", "Lack of legal right of access (ALTA)"],
                "doesNotCover": ["Post-policy forgery or deed fraud after closing", "Unrecorded mechanic's liens or unknown boundary encroachments", "Pre-existing permit or zoning violations"]
            },
            "altaHomeownersPolicy": {
                "name": "ALTA Homeowner's Policy (Enhanced / Eagle Owner's Policy)",
                "protects": "Buyer / owner equity with expanded & post-policy coverages (auto-inflation up to ~150%)",
                "buyerEquityProtected": true,
                "premiumMarkupPct": 12.5,
                "markupRangePct": [10, 15],
                "estimatedPremiumUsd": enhanced_owner_prem,
                "premiumWithReissueUsd": (enhanced_owner_prem - reissue_savings_enh).max(0),
                "covers": [
                    "All Standard Owner's Policy risks",
                    "Post-policy forgery & impersonation (deed fraud after closing)",
                    "Encroachments onto or from neighboring land (subject to sublimits)",
                    "Unrecorded mechanic's liens for pre-closing work",
                    "Limited building-permit & zoning violation coverage (with sublimits & deductibles)",
                    "Subdivision & restrictive covenant violation protections (with sublimits)"
                ],
                "sublimitsNote": "Permit, zoning, and encroachment coverages carry policy-specific sublimits and deductibles (commonly $5,000–$25,000); typically ~10%–15% above standard owner's premium."
            }
        },
        "customaryWhoPays": customary_who_pays,
        "reissueDiscount": {
            "eligible": reissue_eligible,
            "yearsSincePriorSaleOrRefi": years_since_prior,
            "typicalDiscountRangePct": [10, 40],
            "matchedRangePct": discount_range,
            "estimatedDiscountPct": discount_pct,
            "estimatedSavingsStandardUsd": reissue_savings_std,
            "estimatedSavingsEnhancedUsd": reissue_savings_enh,
            "dependsOnRateFiling": true,
            "note": reissue_note,
        },
        "respaSection9": respa_sec_9,
        "sectionCShoppableServices": {
            "sectionName": "Loan Estimate Section C — Services You Can Shop For",
            "estimatedLowTotalUsd": sec_c_low,
            "estimatedTypicalTotalUsd": sec_c_typ,
            "estimatedHighTotalUsd": sec_c_high,
            "estimatedSavingsUsd": sec_c_savings,
            "maxSpreadSavingsUsd": sec_c_max_spread,
            "items": section_c_items,
            "note": "Under TRID / RESPA Section 9, buyers can request competitive quotes for Section C services from at least 2–3 settlement/title providers (and verify whether your lender's approved title list unlocks lower exam/closing fees).",
        }
    })
}

// -------------------------------------------------------------------------------------------------
// Financial Math (Exact Monthly P&I, Reg Z APR Solver, 5-Year Interest + Fees)
// -------------------------------------------------------------------------------------------------

fn calc_monthly_pi(principal: f64, annual_rate_pct: f64, term_months: usize) -> f64 {
    if principal <= 0.0 || term_months == 0 {
        return 0.0;
    }
    let r = annual_rate_pct / 1200.0;
    if r <= 0.0 {
        return principal / (term_months as f64);
    }
    let factor = (1.0 + r).powi(term_months as i32);
    (principal * r * factor) / (factor - 1.0)
}

fn remaining_balance_after(
    principal: f64,
    annual_rate_pct: f64,
    term_months: usize,
    months_paid: usize,
) -> f64 {
    let r = annual_rate_pct / 1200.0;
    let pmt = calc_monthly_pi(principal, annual_rate_pct, term_months);
    let mut bal = principal;
    for _ in 0..months_paid.min(term_months) {
        let interest = bal * r;
        let prin = (pmt - interest).min(bal);
        bal -= prin;
    }
    bal.max(0.0)
}

/// Computes exact TILA / Regulation Z Annual Percentage Rate via 80-iteration bisection IRR
/// on net amount financed (`base_loan - cash_upfront_finance_charges`) against the full stream of
/// principal, interest, and mortgage-insurance payments.
fn solve_reg_z_apr(
    base_loan: f64,
    financed_upfront_fee: f64,
    note_rate_pct: f64,
    term_months: usize,
    cash_upfront_finance_charges: f64,
    monthly_mi: f64,
    arm_step_rate_after_84m: Option<f64>,
) -> f64 {
    if base_loan <= 0.0 || term_months == 0 {
        return round_n(note_rate_pct, 3);
    }
    let financed_principal = base_loan + financed_upfront_fee.max(0.0);
    let amount_financed = (base_loan - cash_upfront_finance_charges).max(1.0);

    let pmt_initial = calc_monthly_pi(financed_principal, note_rate_pct, term_months);
    let pmt_step = match arm_step_rate_after_84m {
        Some(step_r) if term_months > 84 && step_r > note_rate_pct => {
            let rem_84 =
                remaining_balance_after(financed_principal, note_rate_pct, term_months, 84);
            calc_monthly_pi(rem_84, step_r, term_months - 84)
        }
        _ => pmt_initial,
    };

    let pv_at = |rm: f64| -> f64 {
        if rm <= 1e-12 {
            let mut tot = 0.0;
            for m in 1..=term_months {
                let pi = if m <= 84 { pmt_initial } else { pmt_step };
                tot += pi + monthly_mi;
            }
            return tot;
        }
        let mut pv = 0.0;
        let mut disc = 1.0;
        let inv = 1.0 / (1.0 + rm);
        for m in 1..=term_months {
            disc *= inv;
            let pi = if m <= 84 { pmt_initial } else { pmt_step };
            pv += (pi + monthly_mi) * disc;
        }
        pv
    };

    let mut lo = 0.0_f64;
    let mut hi = 0.05_f64; // up to 60% APR
    while pv_at(hi) > amount_financed && hi < 0.25 {
        hi *= 1.5;
    }
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if pv_at(mid) > amount_financed {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let solved = round_n(0.5 * (lo + hi) * 1200.0, 3);
    solved.max(round_n(note_rate_pct, 3))
}

fn calc_five_year_interest_plus_fees(
    financed_principal: f64,
    note_rate_pct: f64,
    term_months: usize,
    upfront_fees_and_points: f64,
    monthly_mi: f64,
) -> i64 {
    let months = term_months.min(60);
    let r = note_rate_pct / 1200.0;
    let pmt = calc_monthly_pi(financed_principal, note_rate_pct, term_months);
    let mut bal = financed_principal;
    let mut interest_sum = 0.0;
    for _ in 0..months {
        let interest = bal * r;
        let prin = (pmt - interest).min(bal);
        bal -= prin;
        interest_sum += interest;
    }
    let mi_60 = monthly_mi * (months as f64);
    (interest_sum + upfront_fees_and_points + mi_60).round() as i64
}

// -------------------------------------------------------------------------------------------------
// Partners JSON Loader (`MARKETPLACE_PARTNERS_JSON`)
// -------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawPartnerOffer {
    #[serde(alias = "partner_id", alias = "id")]
    partner_id: Option<String>,
    #[serde(alias = "partner_name", alias = "name")]
    partner_name: Option<String>,
    vertical: Option<String>,
    #[serde(alias = "product_type", alias = "program", alias = "label")]
    product_type: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    #[serde(default)]
    zips: Vec<String>,
    #[serde(alias = "min_loan")]
    min_loan: Option<f64>,
    #[serde(alias = "max_loan")]
    max_loan: Option<f64>,
    #[serde(alias = "min_price")]
    min_price: Option<f64>,
    #[serde(alias = "max_price")]
    max_price: Option<f64>,
    // Mortgage fields
    #[serde(alias = "rate_pct", alias = "noteRatePct", alias = "rate")]
    rate_pct: Option<f64>,
    #[serde(alias = "apr_pct", alias = "apr")]
    apr_pct: Option<f64>,
    #[serde(alias = "points_pct", alias = "points")]
    points_pct: Option<f64>,
    #[serde(alias = "credits_usd", alias = "credits")]
    credits_usd: Option<f64>,
    #[serde(alias = "origination_fee_usd", alias = "originationFee")]
    origination_fee_usd: Option<f64>,
    #[serde(alias = "term_years")]
    term_years: Option<usize>,
    #[serde(alias = "down_payment_pct")]
    down_payment_pct: Option<f64>,
    #[serde(alias = "nmls_id", alias = "nmls")]
    nmls_id: Option<String>,
    // Insurance fields
    #[serde(alias = "policy_type")]
    policy_type: Option<String>,
    #[serde(alias = "annual_premium_usd", alias = "annualPremium")]
    annual_premium_usd: Option<f64>,
    #[serde(alias = "deductible_usd")]
    deductible_usd: Option<i64>,
    #[serde(
        alias = "insurance_license",
        alias = "stateLicenseId",
        alias = "licenseNumber"
    )]
    insurance_license: Option<String>,
    // Title fields
    #[serde(alias = "estimated_fee_usd", alias = "settlementFeeUsd")]
    estimated_fee_usd: Option<f64>,
    #[serde(alias = "title_license")]
    title_license: Option<String>,
    // Common optional CTA
    #[serde(alias = "cta_url")]
    cta_url: Option<String>,
    notes: Option<String>,
}

fn load_partners_from_env(env: &EnvConfig, warnings: &mut Vec<String>) -> Vec<RawPartnerOffer> {
    let Some(raw) = env
        .get("MARKETPLACE_PARTNERS_JSON")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let json_text = if raw.starts_with('[') || raw.starts_with('{') {
        raw.to_string()
    } else {
        match std::fs::read_to_string(Path::new(raw)) {
            Ok(txt) => txt,
            Err(e) => {
                warnings.push(format!(
                    "Failed reading MARKETPLACE_PARTNERS_JSON file: {e}"
                ));
                return Vec::new();
            }
        }
    };
    let parsed: Value = match serde_json::from_str(&json_text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("Failed parsing MARKETPLACE_PARTNERS_JSON: {e}"));
            return Vec::new();
        }
    };

    let mut items: Vec<RawPartnerOffer> = Vec::new();
    match parsed {
        Value::Array(arr) => {
            for v in arr {
                if let Ok(p) = serde_json::from_value::<RawPartnerOffer>(v) {
                    items.push(p);
                }
            }
        }
        Value::Object(map) => {
            if let Some(Value::Array(arr)) = map.get("partners") {
                for v in arr {
                    if let Ok(p) = serde_json::from_value::<RawPartnerOffer>(v.clone()) {
                        items.push(p);
                    }
                }
            } else {
                for (vert_key, val) in map {
                    if let Value::Array(arr) = val {
                        for v in arr {
                            if let Ok(mut p) = serde_json::from_value::<RawPartnerOffer>(v.clone())
                            {
                                if p.vertical.is_none() {
                                    p.vertical = Some(vert_key.clone());
                                }
                                items.push(p);
                            }
                        }
                    }
                }
            }
        }
        _ => {
            warnings.push("MARKETPLACE_PARTNERS_JSON must be a JSON array or object.".to_string());
        }
    }
    items
}

fn sanitize_http_url(raw: Option<&str>) -> Option<String> {
    let u = raw?.trim();
    if u.starts_with("https://") || u.starts_with("http://") {
        Some(u.to_string())
    } else {
        None
    }
}

fn partner_matches_geo_and_size(
    p: &RawPartnerOffer,
    st: &str,
    zip: &str,
    price: f64,
    loan: f64,
) -> bool {
    if !p.states.is_empty()
        && !st.is_empty()
        && !p.states.iter().any(|s| s.eq_ignore_ascii_case(st))
    {
        return false;
    }
    if !p.zips.is_empty() && !zip.is_empty() && !p.zips.iter().any(|z| z.trim() == zip) {
        return false;
    }
    if let Some(min_p) = p.min_price {
        if price < min_p {
            return false;
        }
    }
    if let Some(max_p) = p.max_price {
        if price > max_p {
            return false;
        }
    }
    if let Some(min_l) = p.min_loan {
        if loan < min_l {
            return false;
        }
    }
    if let Some(max_l) = p.max_loan {
        if loan > max_l {
            return false;
        }
    }
    true
}

// -------------------------------------------------------------------------------------------------
// Vertical Builders
// -------------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_mortgage_vertical(
    facts: &Value,
    valuation: &Value,
    market: &Value,
    geo: &Value,
    deed_title: &Value,
    partners: &[RawPartnerOffer],
    price: f64,
    opts: &MarketplaceQueryOptions,
    warnings: &mut Vec<String>,
) -> Value {
    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let zip = geo.get("zip").and_then(|v| v.as_str()).unwrap_or("");
    let (conf_limit, fha_limit, baseline_limit, is_high_cost) =
        resolve_county_limits(geo, facts, valuation);

    let opt_rates = crate::property::resolve_optimal_rates(market);
    let pmms30_opt = market.pointer("/rates/mort30").and_then(|v| v.as_f64());
    let pmms_date = opt_rates
        .optimal30_date
        .clone()
        .or_else(|| opt_rates.pmms30_date.clone());

    let rate_source = if pmms30_opt.is_some() {
        opt_rates.rate_basis.as_str()
    } else {
        "Benchmark default rate (live Freddie Mac PMMS unavailable)"
    };
    let pmms30 = opt_rates.pmms30;
    let pmms15 = opt_rates.pmms15.unwrap_or(opt_rates.optimal15);
    let fico = opts
        .credit_score
        .unwrap_or(crate::property::DEFAULT_OPTIMAL_CREDIT_SCORE)
        .clamp(500, 850);
    let user_pts = opts.discount_points.unwrap_or(0.0).clamp(-2.0, 4.0);

    let fha_flip_eligible = deed_title
        .pointer("/deedAnalysis/fhaAntiFlipping/fhaEligible")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let fha_flip_reason = deed_title
        .pointer("/deedAnalysis/fhaAntiFlipping/ineligibleReason")
        .and_then(|v| v.as_str())
        .map(String::from);
    let fha_second_appraisal = deed_title
        .pointer("/deedAnalysis/fhaAntiFlipping/secondAppraisalRequired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let pt_lc = facts
        .get("propertyType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let occ_lc = opts
        .occupancy
        .as_deref()
        .unwrap_or("primary")
        .to_ascii_lowercase();
    let is_commercial_or_invest = occ_lc == "investment"
        || occ_lc == "commercial"
        || pt_lc.contains("commercial")
        || pt_lc.contains("mixed")
        || pt_lc.contains("5+")
        || pt_lc.contains("multifamily")
        || pt_lc.contains("apartment");

    let down_default = opts.down_payment_pct.unwrap_or(20.0).clamp(0.0, 80.0);
    let loan_80 = (price * (1.0 - down_default / 100.0)).round();
    let is_jumbo_80 = loan_80 > conf_limit as f64;
    let est_rent = valuation
        .pointer("/ownership/rentEstimate")
        .or_else(|| valuation.pointer("/investorMetrics/estimatedMonthlyRent"))
        .or_else(|| facts.get("rentEstimate"))
        .and_then(|v| v.as_f64());

    let standard_lender_fee = 1_495.0;

    #[allow(clippy::too_many_arguments)]
    let mk_bench = |id: &str,
                    product: &str,
                    program_type: &str,
                    down_pct: f64,
                    note_rate: f64,
                    term_years: usize,
                    points_pct: f64,
                    upfront_financed_pct: f64,
                    annual_mi_pct: f64,
                    arm_step_rate: Option<f64>,
                    eligible: bool,
                    ineligible_reason: Option<String>,
                    second_appraisal_req: bool,
                    note: String|
     -> Value {
        let down_usd = (price * (down_pct / 100.0)).round();
        let base_loan = (price - down_usd).max(1.0);
        let financed_upfront_fee = (base_loan * (upfront_financed_pct / 100.0)).round();
        let financed_principal = base_loan + financed_upfront_fee;
        let points_usd = (base_loan * (points_pct / 100.0)).round();
        let upfront_cash_fc = standard_lender_fee + points_usd;
        let term_months = term_years * 12;
        let monthly_pi = calc_monthly_pi(financed_principal, note_rate, term_months);
        let monthly_mi = (base_loan * (annual_mi_pct / 100.0)) / 12.0;
        let apr_pct = solve_reg_z_apr(
            base_loan,
            financed_upfront_fee,
            note_rate,
            term_months,
            upfront_cash_fc,
            monthly_mi,
            arm_step_rate,
        );
        let five_yr = calc_five_year_interest_plus_fees(
            financed_principal,
            note_rate,
            term_months,
            upfront_cash_fc + financed_upfront_fee,
            monthly_mi,
        );
        let pts_display = if points_pct > 0.0 {
            format!("{points_pct:.2} pts (${})", points_usd as i64)
        } else {
            "0.00 pts ($0)".to_string()
        };
        json!({
            "partnerId": id,
            "id": id,
            "product": product,
            "programType": program_type,
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "downPaymentPct": round_n(down_pct, 1),
            "downPaymentUsd": down_usd as i64,
            "baseLoanAmount": base_loan as i64,
            "loanAmount": financed_principal as i64,
            "termYears": term_years,
            "ratePct": round_n(note_rate, 3),
            "aprPct": apr_pct,
            "pointsPct": round_n(points_pct, 3),
            "creditsUsd": 0,
            "pointsOrCredits": pts_display,
            "assumedOriginationFeesUsd": standard_lender_fee as i64,
            "monthlyPI": monthly_pi.round() as i64,
            "monthlyMI": monthly_mi.round() as i64,
            "fiveYearInterestPlusFees": five_yr,
            "fiveYearCostUsd": five_yr,
            "eligible": eligible,
            "ineligibleReason": ineligible_reason,
            "secondAppraisalRequired": second_appraisal_req,
            "rateSource": rate_source,
            "note": note,
            "tilaDisclosure": format!("Note rate {:.3}% / {:.3}% APR (Reg Z 12 CFR 1026.24 benchmark illustration; not an offer to lend).", note_rate, apr_pct),
        })
    };

    let is_condo_prop =
        pt_lc.contains("condo") || pt_lc.contains("co-op") || pt_lc.contains("town");
    let is_multi_unit_prop = pt_lc.contains("duplex")
        || pt_lc.contains("triplex")
        || pt_lc.contains("fourplex")
        || pt_lc.contains("2-4")
        || pt_lc.contains("2–4");
    let ltv_selected = (100.0 - down_default).clamp(0.0, 100.0);
    let llpa_pts = crate::property::conventional_llpa_points_vs_optimal(
        fico,
        ltv_selected,
        &occ_lc,
        is_condo_prop,
        is_multi_unit_prop,
        loan_80 > baseline_limit as f64 && loan_80 <= conf_limit as f64,
    );
    let conv_pmi_30 = crate::property::conventional_pmi_annual_pct(fico, ltv_selected);
    let conv_pmi_15 = if ltv_selected > 80.0 {
        (conv_pmi_30 * 0.72).max(0.18)
    } else {
        0.0
    };
    let rate_adj_pts = llpa_pts * 0.25 - user_pts * 0.25;
    let conv30_rate = (opt_rates.optimal30 + rate_adj_pts).max(2.25);
    let conv15_rate = (opt_rates.optimal15 + llpa_pts * 0.14 - user_pts * 0.22).max(2.0);
    let fha_fico_adj = if fico >= 720 {
        -0.05
    } else if fico >= 680 {
        0.0
    } else if fico >= 640 {
        0.15
    } else if fico >= 600 {
        0.38
    } else {
        0.65
    };
    let jumbo_fico_adj = if fico >= 780 {
        -0.08
    } else if fico >= 760 {
        0.0
    } else if fico >= 740 {
        0.10
    } else if fico >= 720 {
        0.22
    } else {
        0.42
    };

    let mut benchmarks: Vec<Value> = Vec::new();

    // 1. 30-Year Fixed
    let conv30_eligible = loan_80 <= conf_limit as f64 && fico >= 620;
    benchmarks.push(mk_bench(
        "benchmark-30yr-fixed",
        "30-Year Fixed Conventional Benchmark",
        "30yr_fixed",
        down_default,
        conv30_rate,
        opts.loan_term_years.unwrap_or(30),
        user_pts.max(0.0),
        0.0,
        conv_pmi_30,
        None,
        conv30_eligible,
        if fico < 620 {
            Some(format!(
                "Credit score ({fico}) is below Fannie Mae / Freddie Mac 620 minimum; compare FHA 3.5% down."
            ))
        } else if !conv30_eligible {
            Some(format!(
                "Loan ${} exceeds county conforming limit (${}); use Jumbo financing or increase down payment.",
                loan_80 as i64, conf_limit
            ))
        } else {
            None
        },
        false,
        format!(
            "Conforming limit ${}; priced for {} FICO ({:+.2} LLPA pts vs optimal 760 tier).",
            conf_limit, fico, llpa_pts
        ),
    ));

    // 2. 15-Year Fixed
    benchmarks.push(mk_bench(
        "benchmark-15yr-fixed",
        "15-Year Fixed Conventional Benchmark",
        "15yr_fixed",
        down_default,
        conv15_rate,
        15,
        user_pts.max(0.0),
        0.0,
        conv_pmi_15,
        None,
        conv30_eligible,
        if fico < 620 {
            Some(format!("Credit score ({fico}) is below conventional 620 minimum."))
        } else if !conv30_eligible {
            Some(format!(
                "Loan ${} exceeds county conforming limit (${}).",
                loan_80 as i64, conf_limit
            ))
        } else {
            None
        },
        false,
        format!(
            "Faster principal amortization; priced for {} FICO (15-yr LLPA waivers apply on term <= 15 yrs).",
            fico
        ),
    ));

    // 3. 7/6 SOFR ARM
    let arm_init = (opt_rates.arm76 + rate_adj_pts).max(2.5);
    let arm_fully_indexed = (conv30_rate + 0.45).max(arm_init + 0.25);
    benchmarks.push(mk_bench(
        "benchmark-7-6-sofr-arm",
        "7/6 SOFR ARM Benchmark",
        "7_6_sofr_arm",
        down_default,
        arm_init,
        30,
        user_pts.max(0.0),
        0.0,
        conv_pmi_30,
        Some(arm_fully_indexed),
        fico >= 620,
        if fico < 620 {
            Some(format!("Credit score ({fico}) is below 620 conventional ARM minimum."))
        } else {
            None
        },
        false,
        format!(
            "Fixed 7 years at {:.2}%, then adjusts every 6 months (30-day SOFR + 2.75% margin, 5/1/5 caps). Reg Z composite APR models reset after year 7.",
            arm_init
        ),
    ));

    // 4. FHA 3.5% Down (ineligible on <90-day flip OR above county FHA limit OR <580 FICO)
    let fha_base_loan = (price * 0.965).round();
    let fha_limit_ok = fha_base_loan <= fha_limit as f64;
    let fha_fico_ok = fico >= 580;
    let fha_eligible = fha_flip_eligible && fha_limit_ok && fha_fico_ok;
    let fha_reason = if !fha_flip_eligible {
        fha_flip_reason.clone()
    } else if !fha_fico_ok {
        Some(format!(
            "FHA 3.5% down requires >= 580 FICO (500–579 FICO requires >= 10% down)."
        ))
    } else if !fha_limit_ok {
        Some(format!(
            "Base loan at 3.5% down (${}) exceeds county FHA limit (${}).",
            fha_base_loan as i64, fha_limit
        ))
    } else {
        None
    };
    let fha_note = if fha_second_appraisal {
        "1.75% UFMIP financed + 0.55%/yr MIP. 24 CFR 203.37a second appraisal triggered (91–180d >=100% markup)."
            .to_string()
    } else {
        "1.75% UFMIP financed + 0.55%/yr annual MIP; subject to 24 CFR 203.37a anti-flipping & county FHA cap."
            .to_string()
    };
    benchmarks.push(mk_bench(
        "benchmark-fha-35",
        "FHA 3.5% Down Benchmark",
        "fha_3_5",
        3.5,
        (opt_rates.fha30 + fha_fico_adj - user_pts * 0.22).max(2.5),
        30,
        user_pts.max(0.0),
        1.75,
        0.55,
        None,
        fha_eligible,
        fha_reason.clone(),
        fha_second_appraisal,
        fha_note,
    ));

    // 5. VA 0% Down
    benchmarks.push(mk_bench(
        "benchmark-va-0",
        "VA 0% Down Benchmark (Eligible Veterans)",
        "va_0",
        0.0,
        (opt_rates.va30 + fha_fico_adj * 0.8 - user_pts * 0.22).max(2.5),
        30,
        user_pts.max(0.0),
        2.15,
        0.0,
        None,
        fico >= 580,
        if fico < 580 {
            Some("Most VA lenders require >= 580–620 FICO overlay.".to_string())
        } else {
            None
        },
        false,
        "For eligible veterans/active-duty; 2.15% first-use funding fee financed (waived for exempt veterans), $0 monthly PMI.".to_string(),
    ));

    // 6. Jumbo 30-Yr Fixed (highlighted when above county conforming limit)
    let jumbo_elig = fico >= 700;
    benchmarks.push(mk_bench(
        "benchmark-jumbo-30yr",
        "30-Year Fixed Jumbo Benchmark",
        "jumbo_30yr",
        20.0,
        (opt_rates.jumbo30 + jumbo_fico_adj - user_pts * 0.22).max(2.75),
        30,
        user_pts.max(0.0),
        0.0,
        0.0,
        None,
        jumbo_elig,
        if !jumbo_elig {
            Some(format!(
                "Most non-agency Jumbo lenders require >= 700 FICO (selected: {fico})."
            ))
        } else {
            None
        },
        false,
        if is_jumbo_80 {
            format!(
                "Recommended tier: 80% LTV loan (${}) exceeds county conforming limit (${}).",
                loan_80 as i64, conf_limit
            )
        } else {
            format!(
                "Non-conforming Jumbo tier applies for loans above county limit (${}).",
                conf_limit
            )
        },
    ));

    // 7. DSCR Investor Benchmark
    let dscr_rate = pmms30 + 0.85;
    let dscr_loan = price * 0.75;
    let dscr_pi = calc_monthly_pi(dscr_loan, dscr_rate, 360);
    let est_tax_mo = price * 0.012 / 12.0;
    let est_ins_mo = price * 0.0035 / 12.0;
    let est_dscr = est_rent.map(|r| round_n(r / (dscr_pi + est_tax_mo + est_ins_mo).max(1.0), 2));
    let mut dscr_row = mk_bench(
        "benchmark-dscr-investor",
        "DSCR Investor 30-Yr Fixed Benchmark",
        "dscr_investor",
        25.0,
        dscr_rate,
        30,
        1.0,
        0.0,
        0.0,
        None,
        true,
        None,
        false,
        format!(
            "Investment-property DSCR qualification based on property cashflow{}; 1.00 pt origination benchmark.",
            est_dscr
                .map(|d| format!(" (est. DSCR {d:.2}x)"))
                .unwrap_or_default()
        ),
    );
    if let Some(dscr_val) = est_dscr {
        dscr_row["estimatedDscr"] = json!(dscr_val);
    }
    benchmarks.push(dscr_row);

    // 8. SBA 504 Commercial Owner-User Benchmark (for commercial or investment/multi-unit use)
    if is_commercial_or_invest {
        benchmarks.push(mk_bench(
            "benchmark-sba-504",
            "SBA 504 Commercial Owner-User Benchmark (10% Down)",
            "sba_504",
            10.0,
            pmms30 + 0.55,
            25,
            0.5,
            1.5,
            0.0,
            None,
            true,
            None,
            false,
            "Blended 50% bank first-lien + 40% SBA 504 CDC debenture structure for eligible owner-occupied commercial properties (51%+ owner occupancy).".to_string(),
        ));
    }

    // Sponsored Mortgage Offers from MARKETPLACE_PARTNERS_JSON (separated from benchmarks!)
    let mut sponsored_offers: Vec<Value> = Vec::new();
    for p in partners {
        if !p
            .vertical
            .as_deref()
            .map(|v| v.eq_ignore_ascii_case("mortgage"))
            .unwrap_or(false)
        {
            continue;
        }
        let pid = p.partner_id.as_deref().unwrap_or("").trim();
        let pname = p.partner_name.as_deref().unwrap_or("").trim();
        let nmls = p.nmls_id.as_deref().unwrap_or("").trim();
        if pid.is_empty() || pname.is_empty() {
            warnings.push("Skipped mortgage partner missing partnerId or partnerName.".to_string());
            continue;
        }
        if nmls.is_empty() {
            warnings.push(format!(
                "Skipped sponsored mortgage partner `{pid}` without NMLS ID (Reg Z / SAFE Act required)."
            ));
            continue;
        }
        let Some(rate) = p.rate_pct.filter(|r| r.is_finite() && *r > 0.0) else {
            warnings.push(format!(
                "Skipped sponsored mortgage partner `{pid}` without valid ratePct."
            ));
            continue;
        };

        let down_pct = p.down_payment_pct.unwrap_or(down_default).clamp(0.0, 80.0);
        let base_loan = (price * (1.0 - down_pct / 100.0)).round().max(1.0);
        if !partner_matches_geo_and_size(p, st, zip, price, base_loan) {
            continue;
        }

        let term_years = p.term_years.unwrap_or(30).clamp(5, 40);
        let term_months = term_years * 12;
        let pts_pct = p.points_pct.unwrap_or(0.0);
        let credits_usd = p.credits_usd.unwrap_or(0.0).max(0.0);
        let orig_fee = p
            .origination_fee_usd
            .unwrap_or(standard_lender_fee)
            .max(0.0);
        let points_usd = (base_loan * (pts_pct / 100.0)).round();
        let net_upfront_fc = orig_fee + points_usd - credits_usd;
        let monthly_pi = calc_monthly_pi(base_loan, rate, term_months);

        let computed_apr = solve_reg_z_apr(
            base_loan,
            0.0,
            rate,
            term_months,
            net_upfront_fc.max(0.0),
            0.0,
            None,
        );
        let apr_pct = match p.apr_pct {
            Some(supplied_apr) if supplied_apr >= rate => round_n(supplied_apr, 3),
            Some(_) => {
                warnings.push(format!(
                    "Adjusted sponsored mortgage partner `{pid}` APR to Reg Z compliant floor (>= note rate)."
                ));
                computed_apr
            }
            None => computed_apr,
        };
        let five_yr =
            calc_five_year_interest_plus_fees(base_loan, rate, term_months, net_upfront_fc, 0.0);
        let pts_display = if credits_usd > 0.0 {
            format!("{pts_pct:.2} pts / -${} credit", credits_usd as i64)
        } else {
            format!("{pts_pct:.2} pts (${})", points_usd as i64)
        };

        let prod_lbl = p
            .product_type
            .as_deref()
            .unwrap_or("30-Year Fixed")
            .to_string();
        let is_fha = prod_lbl.to_lowercase().contains("fha");
        let eligible = if is_fha { fha_eligible } else { true };

        sponsored_offers.push(json!({
            "partnerId": pid,
            "partnerName": pname,
            "nmlsId": nmls,
            "product": prod_lbl,
            "rowType": "sponsored_offer",
            "isSponsored": true,
            "disclosureText": SPONSORED_DISCLOSURE_TEXT,
            "compensationModel": COMPENSATION_MODEL,
            "downPaymentPct": round_n(down_pct, 1),
            "loanAmount": base_loan as i64,
            "termYears": term_years,
            "ratePct": round_n(rate, 3),
            "aprPct": apr_pct,
            "pointsPct": round_n(pts_pct, 3),
            "creditsUsd": credits_usd as i64,
            "pointsOrCredits": pts_display,
            "monthlyPI": monthly_pi.round() as i64,
            "fiveYearInterestPlusFees": five_yr,
            "fiveYearCostUsd": five_yr,
            "eligible": eligible,
            "ineligibleReason": if is_fha && !eligible { fha_reason.clone() } else { None },
            "ctaUrl": sanitize_http_url(p.cta_url.as_deref()),
            "notes": p.notes,
        }));
    }

    json!({
        "vertical": "mortgage",
        "defaultCreditScore": crate::property::DEFAULT_OPTIMAL_CREDIT_SCORE,
        "selectedCreditScore": fico,
        "optimal30RatePct": opt_rates.optimal30,
        "countyConformingLimit": conf_limit,
        "baselineConformingLimit": baseline_limit,
        "countyFhaLimit": fha_limit,
        "isHighCostArea": is_high_cost,
        "isJumboAtSelectedDown": is_jumbo_80,
        "fhaFlipEligible": fha_eligible,
        "fhaSecondAppraisalRequired": fha_second_appraisal,
        "pmms30RatePct": pmms30,
        "pmms15RatePct": pmms15,
        "pmmsDate": pmms_date,
        "rateSource": rate_source,
        "benchmarks": benchmarks,
        "sponsoredOffers": sponsored_offers,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_insurance_vertical(
    facts: &Value,
    hazards: &Value,
    geo: &Value,
    sibling_insurance: Option<&Value>,
    partners: &[RawPartnerOffer],
    price: f64,
    warnings: &mut Vec<String>,
) -> Value {
    let st = geo
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let zip = geo.get("zip").and_then(|v| v.as_str()).unwrap_or("");
    let sqft_opt = facts
        .get("sqft")
        .and_then(|v| v.as_f64())
        .filter(|&s| s.is_finite() && s > 0.0);
    let yb = facts.get("yearBuilt").and_then(|v| v.as_i64());

    // Prefer sibling `report["insurance"]` (feat/insurance-risk schema) if present;
    // keep legacy aliases as secondary fallbacks before estimating from verified sqft.
    let sibling_rebuild = sibling_insurance.and_then(|ins| {
        ins.get("coverageAUSD")
            .or_else(|| ins.pointer("/dwellingReconstruction/coverageAUSD"))
            .or_else(|| ins.get("reconstructionValue"))
            .or_else(|| ins.get("replacementCost"))
            .or_else(|| ins.get("dwellingCoverage"))
            .or_else(|| ins.get("rebuildCost"))
            .and_then(|v| v.as_f64())
            .filter(|&v| v > 0.0)
    });
    let rebuild_value_opt = sibling_rebuild
        .or_else(|| sqft_opt.map(|sq| sq * rebuild_per_sqft(&st)))
        .map(|v| v.round() as i64);

    let sib_tier = sibling_insurance
        .and_then(|ins| ins.get("insurabilityTier"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let sib_tier_lower = sib_tier.to_ascii_lowercase();
    let sib_base_obj = sibling_insurance
        .and_then(|ins| ins.pointer("/annualPremiumRangeUSD/baseHomeownersOrFairPlan"));
    let sib_base_expected = sib_base_obj
        .and_then(|o| o.get("expected"))
        .and_then(|v| v.as_f64())
        .filter(|&v| v > 0.0);
    let sib_base_policy = sib_base_obj
        .and_then(|o| o.get("policyType"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let sib_dic_obj =
        sibling_insurance.and_then(|ins| ins.pointer("/annualPremiumRangeUSD/dicWrap"));
    let sib_dic_expected = sib_dic_obj
        .and_then(|o| o.get("expected"))
        .and_then(|v| v.as_f64())
        .filter(|&v| v > 0.0);
    let sib_dic_required = sib_dic_obj
        .and_then(|o| o.get("requiredWithFairOrWindPool"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let sib_base_is_fair_or_pool = sib_base_policy.contains("fair")
        || sib_base_policy.contains("wind")
        || sib_base_policy.contains("pool")
        || sib_base_policy.contains("citizens");
    let sib_high_non_renewal = sib_dic_required
        || sib_base_is_fair_or_pool
        || sib_tier_lower.contains("high non-renewal")
        || sib_tier_lower.contains("fair")
        || sib_tier_lower.contains("wind pool");

    let wf_ca_fhsz = hazards
        .pointer("/wildfire/california/fhsz")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let wf_in_fhsz = hazards
        .pointer("/wildfire/california/inFhsz")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let wf_whp = hazards
        .pointer("/wildfire/whp/class")
        .or_else(|| hazards.pointer("/wildfire/whp/level"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let wf_nri_pct = hazards
        .pointer("/wildfire/nri/percentile")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let sib_wildfire_high = sibling_insurance
        .and_then(|ins| ins.get("highWildfire").and_then(|v| v.as_bool()))
        .unwrap_or(false)
        || (sib_base_policy.contains("fair") && sib_high_non_renewal);

    let high_wildfire = sib_wildfire_high
        || (wf_in_fhsz && (wf_ca_fhsz.contains("high") || wf_ca_fhsz.contains("very high")))
        || wf_whp.contains("high")
        || wf_nri_pct >= 80.0;
    let fair_or_pool_recommended = high_wildfire || sib_high_non_renewal;

    let sib_flood_obj = sibling_insurance
        .and_then(|ins| ins.pointer("/annualPremiumRangeUSD/floodRequiredOrRecommended"));
    let sib_flood_zone = sib_flood_obj
        .and_then(|o| o.get("femaZone"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase());
    let hazards_flood_zone = hazards
        .pointer("/flood/zone")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase());
    let flood_zone_opt = hazards_flood_zone.or(sib_flood_zone);
    let flood_zone_label = flood_zone_opt.as_deref().unwrap_or("Unmapped");
    let sib_flood_mandatory = sib_flood_obj
        .and_then(|o| o.get("mandatoryByLender"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let is_sfha = hazards
        .pointer("/flood/sfha")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| {
            flood_zone_opt
                .as_deref()
                .map(|fz| fz.starts_with('A') || fz.starts_with('V'))
                .unwrap_or(false)
        })
        || sib_flood_mandatory;

    let pga_opt = hazards
        .pointer("/earthquake/designMaps/pgaM")
        .or_else(|| hazards.pointer("/earthquake/designMaps/pga"))
        .and_then(|v| v.as_f64())
        .filter(|p| p.is_finite() && *p >= 0.0);

    let source_mode = if sibling_insurance.is_some() {
        "sibling_insurance_module"
    } else {
        "local_hazard_calibration"
    };

    let Some(rebuild_value) = rebuild_value_opt else {
        warnings.push(
            "Home insurance benchmarks require verified living area (sqft) or dwelling reconstruction value; omitted."
                .to_string(),
        );
        return json!({
            "vertical": "insurance",
            "status": "missing_required_input",
            "sourceMode": "missing_required_input",
            "reconstructionValueUsd": Value::Null,
            "hazardContext": {
                "highWildfire": high_wildfire,
                "fairOrWindPoolRecommended": fair_or_pool_recommended,
                "insurabilityTier": if sib_tier.is_empty() { Value::Null } else { json!(sib_tier) },
                "floodZone": flood_zone_opt,
                "isSfhaFloodMandatory": is_sfha,
                "seismicPga": pga_opt.map(|p| round_n(p, 2)),
            },
            "benchmarks": [],
            "sponsoredOffers": [],
        });
    };

    let age_mult = match yb {
        Some(y) if y < 1970 => 1.15,
        Some(y) if y < 1990 => 1.08,
        Some(y) if y >= 2015 => 0.92,
        _ => 1.0,
    };
    let wf_mult = if fair_or_pool_recommended { 1.85 } else { 1.0 };

    let sibling_ho3 = sibling_insurance
        .and_then(|ins| {
            ins.get("ho3Annual")
                .or_else(|| ins.get("estimatedAnnualPremium"))
                .or_else(|| ins.get("annualPremium"))
                .and_then(|v| v.as_f64())
                .filter(|&v| v > 0.0)
        })
        .or_else(|| {
            if !sib_base_is_fair_or_pool && !sib_dic_required {
                sib_base_expected
            } else {
                None
            }
        });
    let ho3_ann = sibling_ho3
        .unwrap_or_else(|| {
            ((rebuild_value as f64) * base_insurance_rate(&st) * age_mult * wf_mult
                + price * 0.00018)
                .max(1_150.0)
        })
        .round() as i64;
    let ho5_ann = sibling_insurance
        .and_then(|ins| ins.get("ho5Annual").and_then(|v| v.as_f64()))
        .map(|v| v.round() as i64)
        .unwrap_or_else(|| ((ho3_ann as f64) * 1.14).round() as i64);
    let mga_ann =
        ((ho3_ann as f64) * if fair_or_pool_recommended { 1.35 } else { 1.18 }).round() as i64;

    let fair_fire_ann = sibling_insurance
        .and_then(|ins| {
            ins.get("fairPlanAnnual")
                .and_then(|v| v.as_f64())
                .filter(|&v| v > 0.0)
        })
        .or_else(|| {
            if sib_base_is_fair_or_pool || sib_high_non_renewal {
                sib_base_expected
            } else {
                None
            }
        })
        .map(|v| v.round() as i64)
        .unwrap_or_else(|| {
            ((rebuild_value as f64)
                * if fair_or_pool_recommended {
                    0.0078
                } else {
                    0.0048
                })
            .round() as i64
        });
    let dic_ann = sib_dic_expected
        .or_else(|| {
            sibling_insurance.and_then(|ins| {
                ins.get("dicAnnual")
                    .and_then(|v| v.as_f64())
                    .filter(|&v| v > 0.0)
            })
        })
        .map(|v| v.round() as i64)
        .unwrap_or_else(|| ((rebuild_value as f64) * 0.0022).max(950.0).round() as i64);
    let fair_dic_total = fair_fire_ann + dic_ann;

    let sib_flood_expected = sib_flood_obj
        .and_then(|o| o.get("expected"))
        .and_then(|v| v.as_f64())
        .filter(|&v| v > 0.0);
    let nfip_ann = sibling_insurance
        .and_then(|ins| {
            ins.get("nfipAnnual")
                .and_then(|v| v.as_f64())
                .filter(|&v| v > 0.0)
        })
        .or(sib_flood_expected)
        .map(|v| v.round() as i64)
        .unwrap_or_else(|| {
            if flood_zone_label.starts_with('V') {
                4_200
            } else if is_sfha {
                2_350
            } else if flood_zone_label == "D" {
                1_150
            } else {
                680
            }
        });
    let private_flood_ann = sibling_insurance
        .and_then(|ins| {
            ins.get("privateFloodAnnual")
                .and_then(|v| v.as_f64())
                .filter(|&v| v > 0.0)
        })
        .or_else(|| sib_flood_expected.map(|v| (v * 0.85).round()))
        .map(|v| v.round() as i64)
        .unwrap_or_else(|| {
            if flood_zone_label.starts_with('V') {
                3_650
            } else if is_sfha {
                1_890
            } else {
                540
            }
        });

    let sib_eq_obj =
        sibling_insurance.and_then(|ins| ins.pointer("/annualPremiumRangeUSD/earthquakeOptional"));
    let sib_eq_expected = sib_eq_obj
        .and_then(|o| o.get("expected"))
        .and_then(|v| v.as_f64())
        .filter(|&v| v > 0.0);
    let sib_eq_rec = sib_eq_obj
        .and_then(|o| o.get("recommended"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let sib_eq_provider = sib_eq_obj
        .and_then(|o| o.get("provider"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut benchmarks = vec![
        json!({
            "partnerId": "benchmark-ho3-admitted",
            "id": "benchmark-ho3-admitted",
            "product": "Admitted HO-3 (Special Form) Benchmark",
            "policyType": "HO-3",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value,
            "deductibleUsd": 2_500,
            "estimatedAnnualUsd": ho3_ann,
            "estimatedMonthlyUsd": (ho3_ann as f64 / 12.0).round() as i64,
            "recommended": !fair_or_pool_recommended,
            "note": if fair_or_pool_recommended {
                "Elevated wildfire or carrier non-renewal risk may trigger admitted declines; compare FAIR/wind pool + DIC fallback."
            } else {
                "Open-peril dwelling coverage (excludes flood & earthquake); named-peril personal property."
            }
        }),
        json!({
            "partnerId": "benchmark-ho5-admitted",
            "id": "benchmark-ho5-admitted",
            "product": "Admitted HO-5 (Comprehensive Form) Benchmark",
            "policyType": "HO-5",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value,
            "deductibleUsd": 2_500,
            "estimatedAnnualUsd": ho5_ann,
            "estimatedMonthlyUsd": (ho5_ann as f64 / 12.0).round() as i64,
            "recommended": false,
            "note": "Broader open-peril coverage on both dwelling and personal property with higher sublimits."
        }),
        json!({
            "partnerId": "benchmark-insurtech-mga",
            "id": "benchmark-insurtech-mga",
            "product": "Insurtech / Specialty MGA Homeowners Benchmark",
            "policyType": "Insurtech MGA",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value,
            "deductibleUsd": 2_500,
            "estimatedAnnualUsd": mga_ann,
            "estimatedMonthlyUsd": (mga_ann as f64 / 12.0).round() as i64,
            "recommended": false,
            "note": "Flexible underwriting via MGA / surplus-lines carriers when older roof, electrical panel, or catastrophe score limits admitted quotes."
        }),
        json!({
            "partnerId": "benchmark-fair-plan-dic",
            "id": "benchmark-fair-plan-dic",
            "product": if st == "CA" { "California FAIR Plan + Private DIC Wrap Benchmark" } else { "State FAIR Plan / Residual Fire Pool + Private DIC Benchmark" },
            "policyType": "FAIR Plan + DIC",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value,
            "fairPlanFirePortionUsd": fair_fire_ann,
            "dicWrapPortionUsd": dic_ann,
            "estimatedAnnualUsd": fair_dic_total,
            "estimatedMonthlyUsd": (fair_dic_total as f64 / 12.0).round() as i64,
            "applicableForHighWildfire": true,
            "recommended": fair_or_pool_recommended,
            "note": if fair_or_pool_recommended {
                "High wildfire or residual-pool non-renewal tier detected: FAIR Plan / wind pool covers core fire/wind peril, paired with a private DIC wrap for liability, water, and theft."
            } else {
                "Last-resort fire-pool + DIC wrap option if admitted carriers decline coverage."
            }
        }),
        json!({
            "partnerId": "benchmark-nfip-flood",
            "id": "benchmark-nfip-flood",
            "product": "NFIP (FEMA Risk Rating 2.0) Flood Benchmark",
            "policyType": "NFIP Flood",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value.min(250_000),
            "statutoryCapNote": "NFIP caps residential building coverage at $250,000 ($100,000 contents) with a standard 30-day waiting period.",
            "mandatoryForMortgage": is_sfha,
            "estimatedAnnualUsd": nfip_ann,
            "estimatedMonthlyUsd": (nfip_ann as f64 / 12.0).round() as i64,
            "note": format!("FEMA Flood Zone {} ({}).", flood_zone_label, if is_sfha { "High-Risk SFHA — lender flood insurance mandatory" } else { "Optional outside SFHA" })
        }),
        json!({
            "partnerId": "benchmark-private-flood",
            "id": "benchmark-private-flood",
            "product": "Private Flood Insurance Benchmark",
            "policyType": "Private Flood",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value,
            "mandatoryForMortgage": is_sfha,
            "estimatedAnnualUsd": private_flood_ann,
            "estimatedMonthlyUsd": (private_flood_ann as f64 / 12.0).round() as i64,
            "note": "Private carriers can cover full replacement cost above NFIP's $250k cap, include loss-of-use/ALE, and offer shorter waiting periods."
        }),
    ];

    let pga_val = pga_opt.unwrap_or(0.0);
    if pga_opt.map(|p| p >= 0.35).unwrap_or(false)
        || st == "CA"
        || sib_eq_rec
        || sib_eq_expected.is_some()
    {
        let eq_ann = sib_eq_expected
            .unwrap_or_else(|| {
                (rebuild_value as f64) * if pga_val >= 0.5 { 0.0026 } else { 0.0018 }
            })
            .round() as i64;
        let eq_note = match pga_opt {
            Some(p) => format!(
                "Design PGA {:.2} g; standard HO-3/HO-5 policies exclude earth movement.",
                p
            ),
            None => "Standard HO-3/HO-5 policies exclude earth movement.".to_string(),
        };
        let mut eq_row = json!({
            "partnerId": "benchmark-earthquake",
            "id": "benchmark-earthquake",
            "product": if st == "CA" { "CEA / Private Earthquake Insurance Benchmark" } else { "Private Earthquake Insurance Benchmark" },
            "policyType": "Earthquake",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value,
            "estimatedAnnualUsd": eq_ann,
            "estimatedMonthlyUsd": (eq_ann as f64 / 12.0).round() as i64,
            "recommended": sib_eq_rec || pga_opt.map(|p| p >= 0.4).unwrap_or(false),
            "note": eq_note
        });
        if let Some(prov) = sib_eq_provider {
            eq_row["provider"] = json!(prov);
        }
        benchmarks.push(eq_row);
    }

    let mut sponsored_offers: Vec<Value> = Vec::new();
    for p in partners {
        if !p
            .vertical
            .as_deref()
            .map(|v| v.eq_ignore_ascii_case("insurance"))
            .unwrap_or(false)
        {
            continue;
        }
        let pid = p.partner_id.as_deref().unwrap_or("").trim();
        let pname = p.partner_name.as_deref().unwrap_or("").trim();
        let lic = p.insurance_license.as_deref().unwrap_or("").trim();
        if pid.is_empty() || pname.is_empty() {
            warnings
                .push("Skipped insurance partner missing partnerId or partnerName.".to_string());
            continue;
        }
        if lic.is_empty() {
            warnings.push(format!(
                "Skipped sponsored insurance partner `{pid}` without state insurance license (compliance required)."
            ));
            continue;
        }
        let Some(raw_ann) = p.annual_premium_usd.filter(|a| a.is_finite() && *a > 0.0) else {
            warnings.push(format!(
                "Skipped sponsored insurance partner `{pid}` without explicit annualPremiumUsd."
            ));
            continue;
        };
        if !partner_matches_geo_and_size(p, &st, zip, price, price * 0.8) {
            continue;
        }
        let ann = raw_ann.round() as i64;
        sponsored_offers.push(json!({
            "partnerId": pid,
            "partnerName": pname,
            "insuranceLicense": lic,
            "product": p.product_type.as_deref().or(p.policy_type.as_deref()).unwrap_or("Homeowners Policy"),
            "policyType": p.policy_type.as_deref().unwrap_or("HO-3"),
            "rowType": "sponsored_offer",
            "isSponsored": true,
            "disclosureText": SPONSORED_DISCLOSURE_TEXT,
            "compensationModel": COMPENSATION_MODEL,
            "dwellingCoverageUsd": rebuild_value,
            "deductibleUsd": p.deductible_usd.unwrap_or(2_500),
            "estimatedAnnualUsd": ann,
            "estimatedMonthlyUsd": (ann as f64 / 12.0).round() as i64,
            "ctaUrl": sanitize_http_url(p.cta_url.as_deref()),
            "notes": p.notes,
        }));
    }

    json!({
        "vertical": "insurance",
        "status": "ok",
        "sourceMode": source_mode,
        "reconstructionValueUsd": rebuild_value,
        "hazardContext": {
            "highWildfire": high_wildfire,
            "fairOrWindPoolRecommended": fair_or_pool_recommended,
            "insurabilityTier": if sib_tier.is_empty() { Value::Null } else { json!(sib_tier) },
            "floodZone": flood_zone_opt,
            "isSfhaFloodMandatory": is_sfha,
            "seismicPga": pga_opt.map(|p| round_n(p, 2)),
        },
        "benchmarks": benchmarks,
        "sponsoredOffers": sponsored_offers,
    })
}

fn build_title_vertical(
    geo: &Value,
    deed_title: &Value,
    partners: &[RawPartnerOffer],
    price: f64,
    warnings: &mut Vec<String>,
) -> Value {
    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let zip = geo.get("zip").and_then(|v| v.as_str()).unwrap_or("");
    let ti = &deed_title["titleInsurance"];

    let std_prem_opt = ti
        .pointer("/policyComparison/ownersStandardPolicy/estimatedPremiumUsd")
        .and_then(|v| v.as_i64());
    let std_reissue_opt = ti
        .pointer("/policyComparison/ownersStandardPolicy/premiumWithReissueUsd")
        .and_then(|v| v.as_i64())
        .or(std_prem_opt);
    let enh_prem_opt = ti
        .pointer("/policyComparison/altaHomeownersPolicy/estimatedPremiumUsd")
        .and_then(|v| v.as_i64());
    let enh_reissue_opt = ti
        .pointer("/policyComparison/altaHomeownersPolicy/premiumWithReissueUsd")
        .and_then(|v| v.as_i64())
        .or(enh_prem_opt);
    let lender_prem_opt = ti
        .pointer("/policyComparison/lendersPolicy/estimatedConcurrentPremiumUsd")
        .and_then(|v| v.as_i64());
    let reissue_eligible = ti
        .pointer("/reissueDiscount/eligible")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let sec_c_savings_opt = ti
        .pointer("/sectionCShoppableServices/estimatedSavingsUsd")
        .and_then(|v| v.as_i64());
    let sec_c_low_opt = ti
        .pointer("/sectionCShoppableServices/estimatedLowTotalUsd")
        .and_then(|v| v.as_i64());
    let sec_c_typ_opt = ti
        .pointer("/sectionCShoppableServices/estimatedTypicalTotalUsd")
        .and_then(|v| v.as_i64());
    let owners_payer = ti
        .pointer("/customaryWhoPays/matchedRegion/ownersPolicyPayer")
        .and_then(|v| v.as_str())
        .unwrap_or("negotiable");

    let mut benchmarks = Vec::new();
    if let (Some(std_prem), Some(std_reissue), Some(lender_prem)) =
        (std_prem_opt, std_reissue_opt, lender_prem_opt)
    {
        benchmarks.push(json!({
            "partnerId": "benchmark-title-standard",
            "id": "benchmark-title-standard",
            "product": "Owner's Standard (CLTA/ALTA) + Concurrent Lender's Benchmark",
            "policyTier": "standard_owners",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "ownersPolicyPayerCustom": owners_payer,
            "whoPaysLabel": WHO_PAYS_LABEL,
            "estimatedOwnerPremiumUsd": std_prem,
            "estimatedOwnerWithReissueUsd": std_reissue,
            "estimatedLenderPremiumUsd": lender_prem,
            "reissueDiscountEligible": reissue_eligible,
            "note": "Protects buyer equity against pre-closing title defects; does not cover post-policy forgery or unknown encroachments."
        }));
    }
    if let (Some(enh_prem), Some(enh_reissue), Some(lender_prem)) =
        (enh_prem_opt, enh_reissue_opt, lender_prem_opt)
    {
        benchmarks.push(json!({
            "partnerId": "benchmark-title-enhanced",
            "id": "benchmark-title-enhanced",
            "product": "ALTA Homeowner's (Enhanced) + Concurrent Lender's Benchmark",
            "policyTier": "enhanced_alta_homeowners",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "ownersPolicyPayerCustom": owners_payer,
            "whoPaysLabel": WHO_PAYS_LABEL,
            "estimatedOwnerPremiumUsd": enh_prem,
            "estimatedOwnerWithReissueUsd": enh_reissue,
            "estimatedLenderPremiumUsd": lender_prem,
            "reissueDiscountEligible": reissue_eligible,
            "note": "Typically 10–15% above standard policy; adds post-policy forgery, encroachments, mechanic's liens, and limited permit/zoning coverage with sublimits."
        }));
    }
    if let (Some(sec_c_low), Some(sec_c_typ), Some(sec_c_savings)) =
        (sec_c_low_opt, sec_c_typ_opt, sec_c_savings_opt)
    {
        benchmarks.push(json!({
            "partnerId": "benchmark-section-c-settlement",
            "id": "benchmark-section-c-settlement",
            "product": "Loan Estimate Section C Shoppable Settlement Bundle Benchmark",
            "policyTier": "section_c_settlement",
            "rowType": "benchmark",
            "isSponsored": false,
            "disclosureLabel": BENCHMARK_NOTICE,
            "compensationModel": COMPENSATION_MODEL,
            "estimatedLowTotalUsd": sec_c_low,
            "estimatedTypicalTotalUsd": sec_c_typ,
            "estimatedSectionCSavingsUsd": sec_c_savings,
            "respaSection9Notice": "RESPA Section 9 (12 U.S.C. 2608): Seller cannot require buyer to purchase title insurance from a particular title insurer.",
            "note": format!("Comparing 2–3 settlement providers on Section C shoppable fees yields an estimated ~${sec_c_savings} in closing savings.")
        }));
    }

    let mut sponsored_offers: Vec<Value> = Vec::new();
    for p in partners {
        if !p
            .vertical
            .as_deref()
            .map(|v| v.eq_ignore_ascii_case("title"))
            .unwrap_or(false)
        {
            continue;
        }
        let pid = p.partner_id.as_deref().unwrap_or("").trim();
        let pname = p.partner_name.as_deref().unwrap_or("").trim();
        let lic = p
            .title_license
            .as_deref()
            .or(p.insurance_license.as_deref())
            .unwrap_or("")
            .trim();
        if pid.is_empty() || pname.is_empty() {
            warnings.push("Skipped title partner missing partnerId or partnerName.".to_string());
            continue;
        }
        if lic.is_empty() {
            warnings.push(format!(
                "Skipped sponsored title partner `{pid}` without title/settlement license ID."
            ));
            continue;
        }
        let Some(raw_fee) = p.estimated_fee_usd.filter(|f| f.is_finite() && *f > 0.0) else {
            warnings.push(format!(
                "Skipped sponsored title partner `{pid}` without explicit estimatedFeeUsd."
            ));
            continue;
        };
        if !partner_matches_geo_and_size(p, st, zip, price, price * 0.8) {
            continue;
        }
        let fee = raw_fee.round() as i64;
        sponsored_offers.push(json!({
            "partnerId": pid,
            "partnerName": pname,
            "titleLicense": lic,
            "product": p.product_type.as_deref().or(p.policy_type.as_deref()).unwrap_or("Title & Settlement Services"),
            "rowType": "sponsored_offer",
            "isSponsored": true,
            "disclosureText": SPONSORED_DISCLOSURE_TEXT,
            "compensationModel": COMPENSATION_MODEL,
            "estimatedSettlementAndTitleUsd": fee,
            "reissueDiscountEligible": reissue_eligible,
            "ctaUrl": sanitize_http_url(p.cta_url.as_deref()),
            "notes": p.notes,
        }));
    }

    json!({
        "vertical": "title",
        "status": if benchmarks.is_empty() { "missing_required_input" } else { "ok" },
        "reissueDiscountEligible": reissue_eligible,
        "sectionCSavingsUsd": sec_c_savings_opt,
        "ownersPolicyPayerCustom": owners_payer,
        "whoPaysLabel": WHO_PAYS_LABEL,
        "benchmarks": benchmarks,
        "sponsoredOffers": sponsored_offers,
    })
}

/// Builds the full compliant 3-vertical marketplace (`mortgage`, `insurance`, `title`)
/// strictly when a verified property price/valuation exists (never inventing a fallback price).
#[allow(clippy::too_many_arguments)]
pub fn build_marketplace(
    facts: &Value,
    valuation: &Value,
    hazards: &Value,
    market: &Value,
    geo: &Value,
    sibling_insurance: Option<&Value>,
    deed_title: &Value,
    env: &EnvConfig,
    opts: &MarketplaceQueryOptions,
) -> Value {
    let mut warnings: Vec<String> = Vec::new();
    let partners = load_partners_from_env(env, &mut warnings);
    let zip = geo.get("zip").and_then(|v| v.as_str()).unwrap_or("");
    let want = opts
        .vertical
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty() && s != "all");

    let compliance_obj = json!({
        "compensationModel": COMPENSATION_MODEL,
        "respaSection8": "RESPA Section 8 (12 U.S.C. § 2607): No referral fees or kickbacks contingent on settlement; placements are flat display or CPC only.",
        "respaSection9": "RESPA Section 9 (12 U.S.C. § 2608): Seller cannot require buyer to use a particular title insurer.",
        "tilaRegZ": "TILA / Reg Z (12 CFR § 1026.24): Whenever a mortgage rate is shown, the Annual Percentage Rate (APR) is displayed alongside it.",
        "fairHousing": "Fair Housing Act: Quotes and benchmarks are calibrated strictly on property price, loan limits, hazards, and public rate schedules.",
        "equalHousingOpportunity": true,
        "benchmarkNotice": "Benchmark, not an offer. Derived from Freddie Mac PMMS rates, county FHFA/FHA loan limits, and property risk inputs.",
        "sponsoredDisclosureText": SPONSORED_DISCLOSURE_TEXT,
        "privacyClickTracking": "Counters track only (vertical, partner_id, zip, price_tier) totals with zero PII and zero IP addresses stored."
    });

    let Some(price) = subject_price_from(facts, valuation, market, opts.price_override) else {
        warnings.push(
            "Property price/valuation unavailable; mortgage, insurance, and title dollar benchmarks omitted."
                .to_string(),
        );
        let mut verticals = Map::new();
        for v in ["mortgage", "insurance", "title"] {
            if want.is_none() || want.as_deref() == Some(v) {
                verticals.insert(
                    v.into(),
                    json!({
                        "vertical": v,
                        "status": "missing_required_input",
                        "benchmarks": [],
                        "sponsoredOffers": [],
                    }),
                );
            }
        }
        return json!({
            "status": "missing_required_input",
            "propertyPrice": Value::Null,
            "zip": if zip.is_empty() { Value::Null } else { json!(zip) },
            "priceTier": "unknown",
            "partnersConfigured": !partners.is_empty(),
            "hasSponsoredOffers": false,
            "compensationModel": COMPENSATION_MODEL,
            "verticals": Value::Object(verticals),
            "compliance": compliance_obj,
            "warnings": warnings,
        });
    };

    let price_tier = price_tier_for_price(price);

    let mut verticals = Map::new();
    if want.is_none() || want.as_deref() == Some("mortgage") {
        verticals.insert(
            "mortgage".into(),
            build_mortgage_vertical(
                facts,
                valuation,
                market,
                geo,
                deed_title,
                &partners,
                price,
                opts,
                &mut warnings,
            ),
        );
    }
    if want.is_none() || want.as_deref() == Some("insurance") {
        verticals.insert(
            "insurance".into(),
            build_insurance_vertical(
                facts,
                hazards,
                geo,
                sibling_insurance,
                &partners,
                price,
                &mut warnings,
            ),
        );
    }
    if want.is_none() || want.as_deref() == Some("title") {
        verticals.insert(
            "title".into(),
            build_title_vertical(geo, deed_title, &partners, price, &mut warnings),
        );
    }

    let has_sponsored = verticals.values().any(|v| {
        v.get("sponsoredOffers")
            .and_then(|s| s.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    });

    json!({
        "status": "ok",
        "propertyPrice": price.round() as i64,
        "zip": if zip.is_empty() { Value::Null } else { json!(zip) },
        "priceTier": price_tier,
        "partnersConfigured": !partners.is_empty(),
        "hasSponsoredOffers": has_sponsored,
        "compensationModel": COMPENSATION_MODEL,
        "verticals": Value::Object(verticals),
        "compliance": compliance_obj,
        "warnings": warnings,
    })
}

/// Convenience entrypoint taking separate module outputs directly.
#[allow(clippy::too_many_arguments)]
pub fn analyze_from_parts(
    facts: &Value,
    valuation: &Value,
    hazards: &Value,
    market: &Value,
    geo: &Value,
    sibling_insurance: Option<&Value>,
    env: &EnvConfig,
    opts: &MarketplaceQueryOptions,
) -> DeedTitleMarketplaceOutput {
    let as_of = opts.as_of_date.unwrap_or_else(|| Utc::now().date_naive());
    let title_insurance_and_deed = analyze_deed_and_title_with_price(
        facts,
        valuation,
        hazards,
        geo,
        as_of,
        opts.price_override,
    );
    let marketplace = build_marketplace(
        facts,
        valuation,
        hazards,
        market,
        geo,
        sibling_insurance,
        &title_insurance_and_deed,
        env,
        opts,
    );
    DeedTitleMarketplaceOutput {
        title_insurance_and_deed,
        marketplace,
    }
}

/// Primary report orchestrator entrypoint: extracts `(facts, valuation, hazards, market, geo, insurance)`
/// from a Open Estate report JSON and builds `report["titleInsuranceAndDeed"]` and `report["marketplace"]`.
pub fn build_deed_title_and_marketplace(
    report: &Value,
    env: &EnvConfig,
) -> DeedTitleMarketplaceOutput {
    build_deed_title_and_marketplace_with_options(report, env, &MarketplaceQueryOptions::default())
}

pub fn build_deed_title_and_marketplace_with_options(
    report: &Value,
    env: &EnvConfig,
    opts: &MarketplaceQueryOptions,
) -> DeedTitleMarketplaceOutput {
    let empty = Value::Null;
    let facts = report.get("facts").unwrap_or(&empty);
    let geo = report.get("geo").unwrap_or(&empty);
    let valuation = report
        .pointer("/modules/valuation/data")
        .or_else(|| report.get("valuation"))
        .unwrap_or(&empty);
    let hazards = report
        .pointer("/modules/hazards/data")
        .or_else(|| report.get("hazards"))
        .unwrap_or(&empty);
    let market = report
        .pointer("/modules/market/data")
        .or_else(|| report.get("market"))
        .unwrap_or(&empty);
    let merged_ins_storage: Option<Value>;
    let sibling_insurance = match (
        report
            .get("insurance")
            .or_else(|| report.pointer("/modules/insurance/data"))
            .filter(|v| v.is_object()),
        report.get("insuranceQuotes").filter(|v| v.is_object()),
    ) {
        (Some(ins), None) => Some(ins),
        (None, None) => None,
        (ins_opt, Some(quotes)) => {
            let mut m = ins_opt
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            if !m.contains_key("coverageAUSD") {
                if let Some(da) = quotes.pointer("/coverage/dwellingA").cloned() {
                    m.entry("reconstructionValue").or_insert(da);
                }
            }
            if !m.contains_key("ho3Annual")
                && !m.contains_key("estimatedAnnualPremium")
                && !m.contains_key("annualPremium")
                && m.get("annualPremiumRangeUSD").is_none()
            {
                if let Some(ann) = quotes.get("annualTotal").cloned() {
                    m.insert("ho3Annual".into(), ann);
                }
            }
            if !m.contains_key("nfipAnnual") {
                if let Some(fl) = quotes.pointer("/specialtyAddOns/flood/annual").cloned() {
                    m.insert("nfipAnnual".into(), fl);
                }
            }
            if !m.contains_key("fairPlanAnnual") {
                if let Some(fp) = quotes
                    .pointer("/specialtyAddOns/wildfireFairPlan/fairPlanAnnual")
                    .filter(|v| v.as_f64().unwrap_or(0.0) > 0.0)
                    .cloned()
                {
                    m.insert("fairPlanAnnual".into(), fp);
                }
            }
            if !m.contains_key("dicAnnual") {
                if let Some(dic) = quotes
                    .pointer("/specialtyAddOns/wildfireFairPlan/dicWrapAnnual")
                    .filter(|v| v.as_f64().unwrap_or(0.0) > 0.0)
                    .cloned()
                {
                    m.insert("dicAnnual".into(), dic);
                }
            }
            merged_ins_storage = Some(Value::Object(m));
            merged_ins_storage.as_ref()
        }
    };

    analyze_from_parts(
        facts,
        valuation,
        hazards,
        market,
        geo,
        sibling_insurance,
        env,
        opts,
    )
}

pub const MARKETPLACE_QUOTES_ALLOWED_KEYS: &[&str] = &[
    "slug",
    "vertical",
    "price",
    "estimatedValue",
    "downPaymentPct",
    "down_payment_pct",
    "downPct",
    "occupancy",
    "creditScore",
    "credit_score",
    "loanTermYears",
    "loan_term_years",
    "discountPoints",
    "discount_points",
    "zip",
    "state",
    "countyFips",
    "county_fips",
    "county",
    "sqft",
    "beds",
    "propertyType",
    "property_type",
    "lastSaleDate",
    "last_sale_date",
    "lastSalePrice",
    "last_sale_price",
    "priorSaleDate",
    "prior_sale_date",
    "priorSalePrice",
    "prior_sale_price",
    "deedType",
    "deed_type",
    "mort30",
    "mort15",
    "floodZone",
    "flood_zone",
    "wildfireHazard",
    "wildfire_hazard",
    "pga",
    "asOfDate",
    "as_of_date",
    "trackImpressions",
    "track_impressions",
    "facts",
    "valuation",
    "hazards",
    "market",
    "geo",
    "insurance",
    "report",
];

pub const MARKETPLACE_CLICK_ALLOWED_KEYS: &[&str] = &[
    "vertical",
    "partnerId",
    "partner_id",
    "zip",
    "priceTier",
    "price_tier",
    "price",
    "event",
    "eventType",
    "event_type",
    "slug",
];

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceQuotesQuery {
    pub slug: Option<String>,
    pub vertical: Option<String>,
    pub price: Option<f64>,
    #[serde(alias = "down_payment_pct", alias = "downPct")]
    pub down_payment_pct: Option<f64>,
    pub occupancy: Option<String>,
    #[serde(alias = "credit_score")]
    pub credit_score: Option<i64>,
    #[serde(alias = "loan_term_years")]
    pub loan_term_years: Option<usize>,
    #[serde(alias = "discount_points")]
    pub discount_points: Option<f64>,
    pub zip: Option<String>,
    pub state: Option<String>,
    #[serde(alias = "county_fips")]
    pub county_fips: Option<String>,
    pub county: Option<String>,
    pub sqft: Option<f64>,
    pub beds: Option<i64>,
    #[serde(alias = "property_type")]
    pub property_type: Option<String>,
    #[serde(alias = "last_sale_date")]
    pub last_sale_date: Option<String>,
    #[serde(alias = "last_sale_price")]
    pub last_sale_price: Option<i64>,
    #[serde(alias = "prior_sale_date")]
    pub prior_sale_date: Option<String>,
    #[serde(alias = "prior_sale_price")]
    pub prior_sale_price: Option<i64>,
    #[serde(alias = "deed_type")]
    pub deed_type: Option<String>,
    pub mort30: Option<f64>,
    pub mort15: Option<f64>,
    #[serde(alias = "flood_zone")]
    pub flood_zone: Option<String>,
    #[serde(alias = "wildfire_hazard")]
    pub wildfire_hazard: Option<String>,
    pub pga: Option<f64>,
    #[serde(alias = "as_of_date")]
    pub as_of_date: Option<String>,
    #[serde(alias = "track_impressions")]
    pub track_impressions: Option<bool>,
}

/// Validates marketplace quote query/body parameters into `MarketplaceQueryOptions`.
pub fn parse_marketplace_query_options(
    q: &MarketplaceQuotesQuery,
    body: &Value,
) -> std::result::Result<MarketplaceQueryOptions, GuardrailError> {
    let vertical = q
        .vertical
        .as_deref()
        .or_else(|| body.get("vertical").and_then(|v| v.as_str()))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    if let Some(ref v) = vertical {
        if !matches!(v.as_str(), "all" | "mortgage" | "insurance" | "title") {
            return Err(GuardrailError::new(
                "INVALID_VERTICAL",
                "vertical must be one of: all, mortgage, insurance, title.",
                400,
            ));
        }
    }

    let price_override = q
        .price
        .or_else(|| body.get("price").and_then(|v| v.as_f64()))
        .or_else(|| body.get("estimatedValue").and_then(|v| v.as_f64()));
    if let Some(p) = price_override {
        if !p.is_finite() || p <= 0.0 || p > 1_000_000_000.0 {
            return Err(GuardrailError::new(
                "INVALID_PRICE",
                "price must be a positive number.",
                400,
            ));
        }
    }

    let occupancy = q
        .occupancy
        .as_deref()
        .or_else(|| body.get("occupancy").and_then(|v| v.as_str()))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    if let Some(ref occ) = occupancy {
        if !matches!(
            occ.as_str(),
            "primary" | "second_home" | "second" | "investment" | "commercial"
        ) {
            return Err(GuardrailError::new(
                "INVALID_OCCUPANCY",
                "occupancy must be one of: primary, second_home, investment, commercial.",
                400,
            ));
        }
    }

    let credit_score = q
        .credit_score
        .or_else(|| body.get("creditScore").and_then(|v| v.as_i64()))
        .or_else(|| body.get("credit_score").and_then(|v| v.as_i64()));
    if let Some(cs) = credit_score {
        if !(500..=850).contains(&cs) {
            return Err(GuardrailError::new(
                "INVALID_CREDIT_SCORE",
                "creditScore must be between 500 and 850.",
                400,
            ));
        }
    }

    let loan_term_years = q
        .loan_term_years
        .or_else(|| {
            body.get("loanTermYears")
                .and_then(|v| v.as_u64())
                .map(|x| x as usize)
        })
        .or_else(|| {
            body.get("loan_term_years")
                .and_then(|v| v.as_u64())
                .map(|x| x as usize)
        });
    if let Some(t) = loan_term_years {
        if !matches!(t, 10 | 15 | 20 | 25 | 30) {
            return Err(GuardrailError::new(
                "INVALID_LOAN_TERM",
                "loanTermYears must be 10, 15, 20, 25, or 30.",
                400,
            ));
        }
    }

    let discount_points = q
        .discount_points
        .or_else(|| body.get("discountPoints").and_then(|v| v.as_f64()))
        .or_else(|| body.get("discount_points").and_then(|v| v.as_f64()))
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(-2.0, 4.0));

    let as_of_date = if let Some(ref raw_dt) = q.as_of_date {
        let parsed = NaiveDate::parse_from_str(raw_dt.trim(), "%Y-%m-%d").map_err(|_| {
            GuardrailError::new("INVALID_DATE", "asOfDate must use YYYY-MM-DD format.", 400)
        })?;
        Some(parsed)
    } else {
        None
    };

    Ok(MarketplaceQueryOptions {
        vertical,
        price_override,
        down_payment_pct: q.down_payment_pct.map(|d| d.clamp(0.0, 95.0)),
        occupancy,
        credit_score,
        loan_term_years,
        discount_points,
        as_of_date,
    })
}

/// Validates and assembles an explicit inline property parameter envelope for
/// `/api/marketplace/quotes` (e.g. live slider recalculation) without inventing
/// fallback prices, square footage, or synthetic partner quotes.
/// (Retains legacy `build_synthetic_quotes_report` symbol for `web::server` caller compatibility.)
pub fn build_synthetic_quotes_report(
    q: &MarketplaceQuotesQuery,
    body: &Value,
    price_override: Option<f64>,
) -> std::result::Result<Value, GuardrailError> {
    if let Some(ref z) = q.zip {
        let zt = z.trim();
        if !zt.is_empty() && (zt.len() != 5 || !zt.chars().all(|c| c.is_ascii_digit())) {
            return Err(GuardrailError::new(
                "INVALID_ZIP",
                "zip must be a 5-digit US ZIP code.",
                400,
            ));
        }
    }
    if let Some(ref st) = q.state {
        let stt = st.trim();
        if !stt.is_empty() && (stt.len() != 2 || !stt.chars().all(|c| c.is_ascii_alphabetic())) {
            return Err(GuardrailError::new(
                "INVALID_STATE",
                "state must be a 2-letter US state code.",
                400,
            ));
        }
    }

    let mut geo = body
        .get("geo")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(ref z) = q.zip {
        if !z.trim().is_empty() {
            geo.insert("zip".into(), json!(z.trim()));
        }
    }
    if let Some(ref st) = q.state {
        if !st.trim().is_empty() {
            geo.insert("state".into(), json!(st.trim().to_uppercase()));
        }
    }
    if let Some(ref cf) = q.county_fips {
        if !cf.trim().is_empty() {
            geo.insert("countyFips".into(), json!(cf.trim()));
        }
    }
    if let Some(ref co) = q.county {
        if !co.trim().is_empty() {
            geo.insert("county".into(), json!(co.trim()));
        }
    }

    let mut facts = body
        .get("facts")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(sq) = q.sqft {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(bd) = q.beds {
        facts.insert("beds".into(), json!(bd));
    }
    if let Some(ref pt) = q.property_type {
        if !pt.trim().is_empty() {
            facts.insert("propertyType".into(), json!(pt.trim()));
        }
    }
    if let Some(ref lsd) = q.last_sale_date {
        if !lsd.trim().is_empty() {
            let mut ls = json!({ "date": lsd.trim() });
            if let Some(lsp) = q.last_sale_price {
                ls["price"] = json!(lsp);
            }
            if let Some(ref dt) = q.deed_type {
                ls["deedType"] = json!(dt.trim());
            }
            facts.insert("lastSale".into(), ls);
        }
    }
    if let Some(ref psd) = q.prior_sale_date {
        if !psd.trim().is_empty() {
            let mut ps = json!({ "date": psd.trim() });
            if let Some(psp) = q.prior_sale_price {
                ps["price"] = json!(psp);
            }
            facts.insert("priorSale".into(), ps);
        }
    }

    let mut valuation = body
        .get("valuation")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(p) = price_override {
        if !valuation.contains_key("estimate") {
            valuation.insert("estimate".into(), json!({ "value": p.round() as i64 }));
        }
    }

    let mut market = body
        .get("market")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if q.mort30.is_some() || q.mort15.is_some() {
        let mut rates = market
            .get("rates")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(m30) = q.mort30 {
            rates.insert("mort30".into(), json!(m30));
        }
        if let Some(m15) = q.mort15 {
            rates.insert("mort15".into(), json!(m15));
        }
        market.insert("rates".into(), Value::Object(rates));
    }

    let facts_val = Value::Object(facts);
    let valuation_val = Value::Object(valuation);
    let market_val = Value::Object(market);

    if subject_price_from(&facts_val, &valuation_val, &market_val, price_override).is_none() {
        return Err(GuardrailError::new(
            "MISSING_PROPERTY_PRICE",
            "Provide ?slug= for an existing property report, or supply an explicit positive property price.",
            400,
        ));
    }

    let mut hazards = body
        .get("hazards")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(ref fz) = q.flood_zone {
        let z = fz.trim().to_uppercase();
        let sfha = z.starts_with('A') || z.starts_with('V');
        hazards.insert("flood".into(), json!({ "zone": z, "sfha": sfha }));
    }
    if let Some(ref wf) = q.wildfire_hazard {
        let w = wf.trim();
        let in_fhsz = w.to_lowercase().contains("high");
        hazards.insert(
            "wildfire".into(),
            json!({
                "california": { "fhsz": w, "inFhsz": in_fhsz },
                "whp": { "class": w }
            }),
        );
    }
    if let Some(pga) = q.pga {
        hazards.insert(
            "earthquake".into(),
            json!({ "designMaps": { "pgaM": pga } }),
        );
    }

    let mut envelope = json!({
        "geo": Value::Object(geo),
        "facts": facts_val,
        "modules": {
            "valuation": { "data": valuation_val },
            "market": { "data": market_val },
            "hazards": { "data": Value::Object(hazards) },
        }
    });
    if let Some(ins) = body.get("insurance").filter(|v| v.is_object()) {
        envelope["insurance"] = ins.clone();
    }
    Ok(envelope)
}

#[derive(Debug, Clone)]
pub struct ValidatedClickInput {
    pub vertical: String,
    pub partner_id: String,
    pub zip: String,
    pub price_tier: String,
    pub event_type: String,
    pub is_click: bool,
}

pub fn is_known_partner_for_vertical(vertical: &str, partner_id: &str, env: &EnvConfig) -> bool {
    let bench_ok = match vertical {
        "mortgage" => MORTGAGE_BENCHMARK_IDS.contains(&partner_id),
        "insurance" => INSURANCE_BENCHMARK_IDS.contains(&partner_id),
        "title" => TITLE_BENCHMARK_IDS.contains(&partner_id),
        _ => false,
    };
    if bench_ok {
        return true;
    }
    let mut warnings = Vec::new();
    load_partners_from_env(env, &mut warnings)
        .into_iter()
        .any(|p| {
            let vert_match = p
                .vertical
                .as_deref()
                .map(|v| v.eq_ignore_ascii_case(vertical))
                .unwrap_or(false);
            let pid_match = p.partner_id.as_deref().map(str::trim) == Some(partner_id);
            let name_ok = p
                .partner_name
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let license_ok = match vertical {
                "mortgage" => p
                    .nmls_id
                    .as_deref()
                    .map(str::trim)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false),
                "insurance" => p
                    .insurance_license
                    .as_deref()
                    .map(str::trim)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false),
                "title" => p
                    .title_license
                    .as_deref()
                    .or(p.insurance_license.as_deref())
                    .map(str::trim)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false),
                _ => false,
            };
            vert_match && pid_match && name_ok && license_ok
        })
}

/// Validates a privacy-safe `/api/marketplace/click` body (strictly non-PII, non-IP)
/// against canonical price tiers and known benchmark or licensed partner IDs.
pub fn validate_marketplace_click_input(
    body: &Value,
    cached_report: Option<&Value>,
    env: &EnvConfig,
) -> std::result::Result<ValidatedClickInput, GuardrailError> {
    let vertical = body
        .get("vertical")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(vertical.as_str(), "mortgage" | "insurance" | "title") {
        return Err(GuardrailError::new(
            "INVALID_VERTICAL",
            "vertical must be one of: mortgage, insurance, title.",
            400,
        ));
    }

    let partner_id = body
        .get("partnerId")
        .or_else(|| body.get("partner_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if partner_id.is_empty()
        || partner_id.len() > 80
        || !partner_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(GuardrailError::new(
            "INVALID_PARTNER_ID",
            "partnerId must be 1..=80 safe identifier characters.",
            400,
        ));
    }
    if !is_known_partner_for_vertical(&vertical, &partner_id, env) {
        return Err(GuardrailError::new(
            "UNKNOWN_PARTNER_ID",
            "partnerId must match an active benchmark or configured licensed partner for this vertical.",
            400,
        ));
    }

    let zip = body
        .get("zip")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            cached_report
                .and_then(|r| r.pointer("/geo/zip").and_then(|v| v.as_str()))
                .map(String::from)
        })
        .unwrap_or_default();
    if zip.len() != 5 || !zip.chars().all(|c| c.is_ascii_digit()) {
        return Err(GuardrailError::new(
            "INVALID_ZIP",
            "zip must be a 5-digit US ZIP code.",
            400,
        ));
    }

    let price_tier = if let Some(pt) = body
        .get("priceTier")
        .or_else(|| body.get("price_tier"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let pt_lower = pt.to_ascii_lowercase();
        if !CANONICAL_PRICE_TIERS.contains(&pt_lower.as_str()) {
            return Err(GuardrailError::new(
                "INVALID_PRICE_TIER",
                "priceTier must be one of the canonical tiers (under_400k, 400k_800k, 800k_1500k, 1500k_3m, 1500k_3000k, 3m_plus, over_3000k, unknown).",
                400,
            ));
        }
        pt_lower
    } else if let Some(p) = body.get("price").and_then(|v| v.as_f64()) {
        price_tier_for_price(p).to_string()
    } else if let Some(rep) = cached_report {
        let p = rep
            .pointer("/marketplace/propertyPrice")
            .or_else(|| rep.pointer("/modules/valuation/data/estimate/value"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        price_tier_for_price(p).to_string()
    } else {
        "unknown".to_string()
    };

    let event_type = body
        .get("eventType")
        .or_else(|| body.get("event_type"))
        .or_else(|| body.get("event"))
        .and_then(|v| v.as_str())
        .unwrap_or("click")
        .trim()
        .to_ascii_lowercase();
    let is_click = match event_type.as_str() {
        "click" => true,
        "impression" => false,
        _ => {
            return Err(GuardrailError::new(
                "INVALID_EVENT_TYPE",
                "eventType must be 'click' or 'impression'.",
                400,
            ));
        }
    };

    Ok(ValidatedClickInput {
        vertical,
        partner_id,
        zip,
        price_tier,
        event_type,
        is_click,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn broker_packet_guide_does_not_trigger_false_easement_and_title_quotes_match_radar() {
        let empty_env = EnvConfig::from_map(HashMap::new());
        let mut campbell = serde_json::json!({
            "geo": { "state": "CA", "county": "Santa Clara County", "countyFips": "06085" },
            "facts": {
                "listPrice": 2_450_000,
                "titleAndLiens": {
                    "involuntaryLiens": [],
                    "easementsAndRestrictions": [],
                    "disclaimer": "Unrecorded liens and easements require Prelim."
                },
                "disclosures": {
                    "brokerPacketGuide": {
                        "sellerMustProvide": [
                            "Preliminary Title Report (Prelim) with Schedule B underlying easement documents"
                        ]
                    },
                    "statutoryChecklist": [
                        { "key": "NHD", "status": "required_at_escrow", "detail": "Natural Hazard Disclosure including easement notice" }
                    ],
                    "mlsDisclosures": { "disclosures": [] }
                }
            }
        });

        let out = build_deed_title_and_marketplace(&campbell, &empty_env);
        let flags = out.title_insurance_and_deed["deedAnalysis"]["flags"]
            .as_array()
            .expect("flags array");
        assert!(
            !flags
                .iter()
                .any(|f| f["code"] == "EASEMENT_ENCROACHMENT_EXPOSURE"),
            "brokerPacketGuide must not trigger false EASEMENT_ENCROACHMENT_EXPOSURE: {flags:?}"
        );
        assert!(flags
            .iter()
            .any(|f| f["code"] == "PRE_PRELIM_EASEMENT_SCREEN"));

        // Real MLS easement disclosure DOES trigger EASEMENT_ENCROACHMENT_EXPOSURE
        campbell["facts"]["disclosures"]["mlsDisclosures"]["disclosures"] =
            serde_json::json!(["Recorded shared driveway easement along north boundary"]);
        let out_with_easement = build_deed_title_and_marketplace(&campbell, &empty_env);
        let flags2 = out_with_easement.title_insurance_and_deed["deedAnalysis"]["flags"]
            .as_array()
            .expect("flags2 array");
        assert!(flags2
            .iter()
            .any(|f| f["code"] == "EASEMENT_ENCROACHMENT_EXPOSURE"));

        // Owner's standard title quote must match `title_legal_radar::compute_state_title_rate_filing`
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 15).unwrap();
        let tx_report = serde_json::json!({
            "geo": { "state": "TX", "county": "Travis County", "countyFips": "48453" },
            "facts": { "listPrice": 500_000 },
            "valuation": { "data": { "estimate": { "value": 500_000, "asOf": "2026-11-15" } } }
        });
        let tx_out = build_deed_title_and_marketplace(&tx_report, &empty_env);
        let tx_filing = crate::modules::title_legal_radar::compute_state_title_rate_filing(
            &tx_report["geo"],
            &tx_report["facts"],
            Some(500_000.0),
            as_of,
        );
        assert_eq!(
            tx_out.title_insurance_and_deed["titleInsurance"]["policyComparison"]
                ["ownersStandardPolicy"]["estimatedPremiumUsd"],
            tx_filing["ownerStandardUsd"]
        );
        assert_eq!(
            tx_out.title_insurance_and_deed["titleInsurance"]["rateRegime"],
            "promulgated_statutory"
        );
        assert_eq!(
            out.title_insurance_and_deed["titleInsurance"]["rateRegime"],
            "estimated_schedule"
        );
    }
}
