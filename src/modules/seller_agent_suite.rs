//! Grounded Seller Strategy & Listing/Buyer Agent Deal-Desk Engines (`sellerSuite` & `agentSuite`).
//!
//! Strictly adheres to Open Estate's Zero-Mock Data Honesty rule:
//! - Every number is derived from verified report inputs or statutory county/state schedules.
//! - Private/unrecorded figures (mortgage payoff balance, broker commission rates, buyer concessions,
//!   seller prep budgets) are NEVER invented and are explicitly surfaced as `null` user-editable inputs.
//! - ZIP seasonality requires >= 36 real monthly index observations; shorter series emit `[]` rather
//!   than a synthetic seasonal curve.

use crate::property::facts::is_synthetic_comp_value;
use crate::property::market::value_array_to_series;
use crate::registry::loan_limits::{classify_loan_tier, resolve_loan_limits_from_report_parts};
use crate::registry::rates_and_costs::{
    estimate_transfer_tax_schedule, lookup_who_pays, state_effective_property_tax_rate,
    CA_PROP13_CONSTITUTIONAL_BASE_RATE,
};
use crate::shared::{fmt_usd, round_n, unwrap_data};
use chrono::{Datelike, NaiveDate, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

pub const PORTAL_SEARCH_BRACKETS_USD: &[i64] = &[
    500_000, 750_000, 1_000_000, 1_250_000, 1_500_000, 1_750_000, 2_000_000, 2_250_000, 2_500_000,
    3_000_000, 3_500_000, 4_000_000, 5_000_000,
];

pub const IRC_121_SINGLE_EXCLUSION_USD: i64 = 250_000;
pub const IRC_121_MARRIED_EXCLUSION_USD: i64 = 500_000;
pub const FEDERAL_MAX_LTCG_PCT: f64 = 20.0;
pub const FEDERAL_NIIT_PCT: f64 = 3.8;

const CALENDAR_MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

static RE_NOMINAL_NON_ARMS_LENGTH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:interspousal|quit\s*claim|gift|revocable\s+trust|living\s+trust\s+transfer|corrective\s+deed|affidavit\s+of\s+death)\b")
        .expect("nominal transfer regex")
});

static RE_INHERITED_OR_ESTATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:probate|estate\s+sale|executor|administrator|conservator|letters\s+testamentary|inherited|devise)\b")
        .expect("probate estate regex")
});

static RE_TRUST_OR_ENTITY_VESTING: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:trust(?:ee)?|living\s+trust|family\s+trust|\bllc\b|limited\s+liability|partnership|\binc\b|\bcorp\b)\b")
        .expect("trust or entity regex")
});

static RE_PACE_LIEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:pace\s+(?:assessment|lien|program)|hero\s+(?:assessment|lien|program)|ygrene|renovate\s+america|property\s+assessed\s+clean\s+energy)\b")
        .expect("pace lien regex")
});

static RE_SOLAR_LEASE_UCC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:solar\s+lease|leased\s+solar|power\s+purchase\s+agreement|\bppa\b|solar\s+ppa|ucc[- ]?1\s+solar|sunrun|vivint\s+solar|sunnova|teslasolar\s+lease)\b")
        .expect("solar lease regex")
});

static RE_SOLAR_ANY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:solar|photovoltaic|\bpv\s+system\b|\bpv\s+array\b)\b")
        .expect("solar any regex")
});

static RE_NON_REROOF_PERMIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:photovoltaic|\bpv\b|solar|inverter|battery|storage|powerwall|ev\s+charger|skylight|hvac)\b")
        .expect("non reroof permit regex")
});

static RE_REROOF_PERMIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:re[- ]?roof(?:ing)?|roof\s+replace(?:ment)?|tear[- ]?off|shingle|comp(?:osition)?\s+roof|tile\s+roof|metal\s+roof|roofing)\b")
        .expect("reroof permit regex")
});

static RE_SQFT_CONFLICT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:unpermitted|not\s+permitted|without\s+permit|buyer\s+to\s+verify\s+sq|discrepancy|differs\s+from\s+tax|sunroom|enclosed\s+patio|garage\s+conversion|bonus\s+room|not\s+included)\b")
        .expect("sqft conflict regex")
});

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SellerAgentSuiteOutput {
    pub seller_suite: Value,
    pub agent_suite: Value,
}

/// Idempotently attaches `sellerSuite` and `agentSuite` onto a report JSON object when absent.
/// Returns `true` if the report was mutated.
pub fn attach_seller_and_agent_suites(report: &mut Value) -> bool {
    let needs_seller = report
        .get("sellerSuite")
        .map(Value::is_null)
        .unwrap_or(true);
    let needs_agent = report.get("agentSuite").map(Value::is_null).unwrap_or(true);
    if !needs_seller && !needs_agent {
        return false;
    }
    let built = build_seller_and_agent_suites(report);
    let Some(obj) = report.as_object_mut() else {
        return false;
    };
    if needs_seller {
        obj.insert("sellerSuite".to_string(), built.seller_suite);
    }
    if needs_agent {
        obj.insert("agentSuite".to_string(), built.agent_suite);
    }
    true
}

/// Builds both `sellerSuite` and `agentSuite` deterministically from a Open Estate report JSON.
pub fn build_seller_and_agent_suites(report: &Value) -> SellerAgentSuiteOutput {
    let ctx = SuiteContext::from_report(report);

    let net_sheet = build_net_sheet_model(&ctx);
    let tax_portability = build_tax_portability(&ctx);
    let cure_before_escrow = build_cure_before_escrow(report, &ctx);
    let launch_timing = build_launch_timing_and_bracket(&ctx);

    let seller_suite = json!({
        "netSheetModel": net_sheet,
        "taxPortability": tax_portability,
        "cureBeforeEscrow": cure_before_escrow,
        "launchTimingAndBracket": launch_timing,
    });

    let escrow_killer_matrix = build_escrow_killer_matrix(&ctx, &cure_before_escrow);
    let escrow_killer_summary = summarize_escrow_gates(&escrow_killer_matrix);
    let cma_workbench = build_cma_workbench(&ctx);
    let listing_lead_signals = build_listing_lead_signals(&ctx);

    let agent_suite = json!({
        "escrowKillerMatrix": escrow_killer_matrix,
        "escrowKillerSummary": escrow_killer_summary,
        "cmaWorkbench": cma_workbench,
        "listingLeadSignals": listing_lead_signals,
    });

    SellerAgentSuiteOutput {
        seller_suite,
        agent_suite,
    }
}

// =================================================================================================
// Shared Parsing & Report Extraction Helpers
// =================================================================================================

fn module_slice<'a>(report: &'a Value, key: &str) -> &'a Value {
    report
        .pointer(&format!("/modules/{key}"))
        .map(unwrap_data)
        .or_else(|| report.get(key).map(unwrap_data))
        .unwrap_or(&Value::Null)
}

fn val_f64(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    if let Some(n) = val.as_f64() {
        return n.is_finite().then_some(n);
    }
    if let Some(i) = val.as_i64() {
        return Some(i as f64);
    }
    if let Some(s) = val.as_str() {
        let clean: String = s
            .trim()
            .replace([',', '$', '%'], "")
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if clean.is_empty() || clean == "-" || clean == "." {
            return None;
        }
        return clean.parse::<f64>().ok().filter(|n| n.is_finite());
    }
    None
}

fn val_i64(v: Option<&Value>) -> Option<i64> {
    val_f64(v).map(|n| n.round() as i64)
}

fn val_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(Value::as_str)
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
    for fmt in ["%Y-%m-%d", "%m/%d/%Y", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d);
        }
    }
    if s.len() == 7 {
        let padded = format!("{s}-15");
        if let Ok(d) = NaiveDate::parse_from_str(&padded, "%Y-%m-%d") {
            return Some(d);
        }
    }
    if s.len() == 4 {
        if let Ok(yr) = s.parse::<i32>() {
            if (1850..=2050).contains(&yr) {
                return NaiveDate::from_ymd_opt(yr, 6, 15);
            }
        }
    }
    None
}

fn parse_ym_pair(ym_str: &str) -> Option<(i32, u32)> {
    let s = ym_str.trim();
    if s.len() < 7 {
        return None;
    }
    let yr = s.get(0..4)?.parse::<i32>().ok()?;
    if s.as_bytes().get(4).copied()? != b'-' {
        return None;
    }
    let mo = s.get(5..7)?.parse::<u32>().ok()?;
    if !(1900..=2100).contains(&yr) || !(1..=12).contains(&mo) {
        return None;
    }
    Some((yr, mo))
}

fn months_diff(a: (i32, u32), b: (i32, u32)) -> i32 {
    (b.0 - a.0) * 12 + (b.1 as i32 - a.1 as i32)
}

fn median_f64(vals: &[f64]) -> Option<f64> {
    let mut a: Vec<f64> = vals.iter().copied().filter(|x| x.is_finite()).collect();
    if a.is_empty() {
        return None;
    }
    a.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let mid = a.len() / 2;
    if a.len() % 2 == 1 {
        Some(a[mid])
    } else {
        Some((a[mid - 1] + a[mid]) / 2.0)
    }
}

/// Statutory top marginal state individual long-term capital gains rate (%) on primary/residential real estate.
/// Note: Washington's 7% capital-gains excise tax (`RCW 82.87.050(1)`) statutorily exempts ALL real estate.
pub fn state_top_real_estate_ltcg_rate_pct(state: &str) -> Option<f64> {
    let st = state.trim().to_ascii_uppercase();
    let rate = match st.as_str() {
        "CA" => 13.3,  // 12.3% top bracket + 1.0% Mental Health Services Act surtax (> $1M)
        "NY" => 10.9,  // NYS top statutory rate (excludes NYC resident local income tax)
        "NJ" => 10.75, // NJ Gross Income Tax top rate
        "DC" => 10.75, // District of Columbia top bracket
        "OR" => 9.9,   // Oregon top bracket
        "MN" => 9.85,  // Minnesota top rate (+ 1% net investment surtax above $1M if applicable)
        "MA" => 9.0,   // 5.0% flat + 4.0% Fair Share millionaire surtax above $1M threshold
        "VT" => 8.75,  // Vermont top rate (partial exclusion caps apply on qualifying gains)
        "HI" => 7.25,  // Hawaii HRS § 235-51(f) statutory 7.25% cap on net capital gains
        "ME" => 7.15,
        "CT" => 6.99,
        "ID" => 5.695,
        "SC" => 6.2, // 44% net capital gain deduction reduces effective top rate below ordinary 6.2%
        "WI" => 5.36, // 30% net LTCG exclusion off 7.65% top bracket
        "MT" => 5.9,
        "VA" => 5.75,
        "MD" => 5.75, // State top rate (counties levy additional 2.25%–3.20% local tax)
        "RI" => 5.99,
        "DE" => 6.6,
        "WV" => 4.82,
        "NE" => 5.2,
        "KS" => 5.58,
        "AL" => 5.0,
        "MS" => 4.4,
        "OK" => 4.75,
        "IL" => 4.95,
        "MI" => 4.25,
        "UT" => 4.55,
        "NC" => 4.25,
        "CO" => 4.4,
        "GA" => 5.19,
        "KY" => 4.0,
        "IA" => 3.8,
        "MO" => 4.7,
        "AR" => 3.9,
        "OH" => 3.5,
        "IN" => 3.0,
        "PA" => 3.07,
        "LA" => 3.0,
        "AZ" => 1.875, // 2.5% flat rate with 25% LTCG deduction on post-2011 assets
        "NM" => 3.54,  // 5.9% top rate with 40% net capital gains deduction
        "ND" => 1.5,   // 2.5% top rate with 40% LTCG exclusion
        // Zero state capital gains tax on real estate transfers
        "AK" | "FL" | "NH" | "NV" | "SD" | "TN" | "TX" | "WA" | "WY" => 0.0,
        _ => return None,
    };
    Some(rate)
}

fn estimate_standard_owner_title_premium(price: f64, rate_regime: &str) -> i64 {
    if !price.is_finite() || price <= 0.0 {
        return 0;
    }
    let raw = if rate_regime == "promulgated" {
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

#[derive(Debug, Clone)]
struct RecordedAcquisition {
    date_str: Option<String>,
    date: Option<NaiveDate>,
    price_usd: Option<i64>,
    deed_type: Option<String>,
}

struct SuiteContext<'a> {
    facts: &'a Value,
    geo: &'a Value,
    valuation: &'a Value,
    market: &'a Value,
    hazards: &'a Value,
    buyer_diligence: &'a Value,
    hoa_profile: &'a Value,
    title_deed: &'a Value,
    remodel: &'a Value,
    condition: &'a Value,
    insurance: &'a Value,
    state: String,
    county: String,
    county_fips: String,
    city: String,
    as_of_date: NaiveDate,
    benchmark_price_usd: Option<i64>,
    low_price_usd: Option<i64>,
    high_price_usd: Option<i64>,
    per_sqft_usd: Option<i64>,
    acquisition: RecordedAcquisition,
    current_assessed_usd: Option<i64>,
    recorded_annual_tax_usd: Option<i64>,
    effective_tax_rate: Option<f64>,
    combined_text: String,
}

impl<'a> SuiteContext<'a> {
    fn from_report(report: &'a Value) -> Self {
        let facts = report
            .get("facts")
            .or_else(|| report.pointer("/modules/facts"))
            .map(unwrap_data)
            .unwrap_or(&Value::Null);
        let geo = report.get("geo").map(unwrap_data).unwrap_or(&Value::Null);
        let valuation = module_slice(report, "valuation");
        let market = module_slice(report, "market");
        let hazards = module_slice(report, "hazards");
        let buyer_diligence = report
            .get("buyerDiligence")
            .or_else(|| report.pointer("/modules/buyer_diligence"))
            .map(unwrap_data)
            .unwrap_or(&Value::Null);
        let hoa_profile = report
            .get("hoaProfile")
            .map(unwrap_data)
            .unwrap_or(&Value::Null);
        let title_deed = report
            .get("titleInsuranceAndDeed")
            .or_else(|| report.pointer("/modules/titleInsuranceAndDeed"))
            .map(unwrap_data)
            .unwrap_or(&Value::Null);
        let remodel = report
            .get("remodelAndRebuild")
            .or_else(|| report.pointer("/modules/remodelAndRebuild"))
            .map(unwrap_data)
            .unwrap_or(&Value::Null);
        let condition = report
            .get("condition")
            .or_else(|| report.get("permitsAndCondition"))
            .or_else(|| report.pointer("/modules/condition"))
            .or_else(|| facts.get("condition"))
            .map(unwrap_data)
            .unwrap_or(&Value::Null);
        let insurance = report
            .get("insurance")
            .or_else(|| report.pointer("/modules/insurance"))
            .or_else(|| facts.get("insurance"))
            .map(unwrap_data)
            .unwrap_or(&Value::Null);

        let county_fips = val_str(geo.get("countyFips")).unwrap_or("").to_string();
        let state = val_str(geo.get("state"))
            .map(|s| s.to_ascii_uppercase())
            .or_else(|| {
                val_str(geo.get("stateFips"))
                    .and_then(crate::registry::us_geo::state_abbr_for_fips)
                    .map(String::from)
            })
            .or_else(|| {
                (!county_fips.is_empty())
                    .then(|| crate::registry::us_geo::state_abbr_for_fips(&county_fips))
                    .flatten()
                    .map(String::from)
            })
            .unwrap_or_default();
        let county = val_str(geo.get("county")).unwrap_or("").to_string();
        let city = val_str(geo.get("city"))
            .or_else(|| val_str(geo.get("place")))
            .or_else(|| val_str(facts.get("city")))
            .unwrap_or("")
            .to_string();

        let as_of_date = val_str(valuation.pointer("/estimate/asOf"))
            .or_else(|| val_str(facts.get("asOf")))
            .or_else(|| val_str(report.pointer("/meta/generatedAt")))
            .or_else(|| val_str(market.pointer("/zhvi/date")))
            .and_then(parse_flexible_date)
            .unwrap_or_else(|| Utc::now().date_naive());

        let benchmark_price_usd = val_i64(valuation.pointer("/estimate/value")).filter(|&v| v > 0);
        let low_price_usd = val_i64(valuation.pointer("/estimate/low")).filter(|&v| v > 0);
        let high_price_usd = val_i64(valuation.pointer("/estimate/high")).filter(|&v| v > 0);
        let per_sqft_usd = val_i64(
            valuation
                .pointer("/estimate/perSqft")
                .or_else(|| valuation.pointer("/estimate/pricePerSqft")),
        )
        .filter(|&v| v > 0);

        let acquisition = resolve_current_owner_acquisition(facts);

        // Current assessed value from real parcel roll
        let latest_tax_entry = facts
            .get("taxHistory")
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter()
                    .max_by_key(|row| val_i64(row.get("year")).unwrap_or(0))
            });
        let current_assessed_usd = latest_tax_entry
            .and_then(|row| {
                val_i64(row.get("assessment"))
                    .or_else(|| val_i64(row.get("assessed")))
                    .or_else(|| val_i64(row.get("assessedValue")))
                    .or_else(|| val_i64(row.get("netTaxable")))
            })
            .filter(|&v| v > 0)
            .or_else(|| {
                val_i64(
                    facts
                        .pointer("/officialAssessment/assessedTotal")
                        .or_else(|| facts.pointer("/officialAssessment/assessedNetTaxable")),
                )
                .filter(|&v| v > 0)
            })
            .or_else(|| val_i64(facts.get("assessedValue")).filter(|&v| v > 0))
            .or_else(|| {
                val_i64(
                    valuation
                        .pointer("/assessmentVsMarket/assessedValue")
                        .or_else(|| {
                            valuation.pointer("/ownership/closingAndSupplemental/priorAssessed")
                        }),
                )
                .filter(|&v| v > 0)
            });

        let recorded_annual_tax_usd = latest_tax_entry
            .and_then(|row| {
                val_i64(row.get("tax")).or_else(|| val_i64(row.get("statutoryAdValoremTax")))
            })
            .filter(|&v| v > 0)
            .or_else(|| {
                val_i64(
                    facts
                        .pointer("/officialAssessment/annualTax")
                        .or_else(|| facts.pointer("/officialAssessment/statutoryAdValoremTax")),
                )
                .filter(|&v| v > 0)
            })
            .or_else(|| {
                val_i64(valuation.pointer("/ownership/closingAndSupplemental/sellerLastTax"))
                    .filter(|&v| v > 0)
            });

        let effective_tax_rate = val_f64(facts.get("taxRate"))
            .filter(|&r| r > 0.0 && r < 0.15)
            .or_else(|| {
                val_f64(facts.pointer("/officialAssessment/statutoryTaxRatePct"))
                    .map(|p| p / 100.0)
                    .filter(|&r| r > 0.0 && r < 0.15)
            })
            .or_else(|| {
                val_f64(valuation.pointer("/ownership/taxRatePct"))
                    .map(|p| p / 100.0)
                    .filter(|&r| r > 0.0 && r < 0.15)
            })
            .or_else(|| {
                val_f64(market.pointer("/govTaxBenchmark/effectiveTaxRate"))
                    .filter(|&r| r > 0.0 && r < 0.15)
            })
            .or_else(|| {
                if state == "CA" {
                    Some(CA_PROP13_CONSTITUTIONAL_BASE_RATE)
                } else if !state.is_empty() {
                    Some(state_effective_property_tax_rate(&state))
                } else {
                    None
                }
            });

        let combined_text = collect_text_signals(facts, title_deed, hoa_profile);

        Self {
            facts,
            geo,
            valuation,
            market,
            hazards,
            buyer_diligence,
            hoa_profile,
            title_deed,
            remodel,
            condition,
            insurance,
            state,
            county,
            county_fips,
            city,
            as_of_date,
            benchmark_price_usd,
            low_price_usd,
            high_price_usd,
            per_sqft_usd,
            acquisition,
            current_assessed_usd,
            recorded_annual_tax_usd,
            effective_tax_rate,
            combined_text,
        }
    }
}

fn collect_text_signals(facts: &Value, title_deed: &Value, hoa_profile: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for k in [
        "remarks",
        "publicRemarks",
        "description",
        "renovations",
        "sqftNote",
        "permitNotes",
        "zoningNote",
        "taxNote",
        "legal",
    ] {
        if let Some(s) = val_str(facts.get(k)) {
            parts.push(s.to_string());
        }
    }
    if let Some(arr) = facts.get("features").and_then(Value::as_array) {
        for item in arr.iter().take(40) {
            if let Some(s) = val_str(Some(item)) {
                parts.push(s.to_string());
            }
        }
    } else if let Some(s) = val_str(facts.get("features")) {
        parts.push(s.to_string());
    }
    for sub in ["titleAndLiens", "buildingRegister", "hoa", "foreclosure"] {
        if let Some(obj) = facts.get(sub).filter(|v| v.is_object()) {
            if let Ok(s) = serde_json::to_string(obj) {
                parts.push(s.chars().take(3000).collect());
            }
        }
    }
    if title_deed.is_object() {
        if let Ok(s) = serde_json::to_string(title_deed) {
            parts.push(s.chars().take(2000).collect());
        }
    }
    if hoa_profile.is_object() {
        if let Ok(s) = serde_json::to_string(hoa_profile) {
            parts.push(s.chars().take(1500).collect());
        }
    }
    parts.join(" | ")
}

/// Resolves the current owner's arm's-length acquisition date & price from `facts.lastSale`,
/// `facts.saleHistory`, and `facts.titleAndLiens.transfers`.
fn resolve_current_owner_acquisition(facts: &Value) -> RecordedAcquisition {
    #[derive(Clone)]
    struct Candidate {
        date_str: String,
        date: NaiveDate,
        price: Option<i64>,
        deed_type: String,
        is_nominal: bool,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut push_cand =
        |dt_raw: Option<&str>, price_opt: Option<i64>, deed: &str, note: &str, event: &str| {
            let Some(raw) = dt_raw else {
                return;
            };
            let Some(date) = parse_flexible_date(raw) else {
                return;
            };
            let ev_lc = event.to_ascii_lowercase();
            let is_sale_transfer = ev_lc.is_empty()
                || ev_lc.contains("sold")
                || ev_lc.contains("sale")
                || ev_lc.contains("close")
                || ev_lc.contains("transfer")
                || ev_lc.contains("deed");
            if !is_sale_transfer {
                return;
            }
            let norm_dt = if raw.trim().len() >= 10 {
                date.format("%Y-%m-%d").to_string()
            } else {
                raw.trim().to_string()
            };
            let clean_price = price_opt.filter(|&p| p > 1_000);
            let blob = format!("{deed} {note} {event}");
            let is_nominal = clean_price.is_none() && RE_NOMINAL_NON_ARMS_LENGTH.is_match(&blob);
            if let Some(existing) = candidates.iter_mut().find(|c| c.date == date) {
                if existing.price.is_none() && clean_price.is_some() {
                    existing.price = clean_price;
                    existing.is_nominal = false;
                }
                if existing.deed_type.is_empty() && !deed.trim().is_empty() {
                    existing.deed_type = deed.trim().to_string();
                }
            } else {
                candidates.push(Candidate {
                    date_str: norm_dt,
                    date,
                    price: clean_price,
                    deed_type: deed.trim().to_string(),
                    is_nominal,
                });
            }
        };

    if let Some(ls) = facts.get("lastSale").filter(|v| v.is_object()) {
        let ls_dt = val_str(ls.get("date"));
        if ls_dt.and_then(parse_flexible_date).is_some() {
            push_cand(
                ls_dt,
                val_i64(ls.get("price")),
                val_str(ls.get("deedType")).unwrap_or(""),
                val_str(ls.get("note")).unwrap_or(""),
                "Sold",
            );
        } else if let Some(only_price) = val_i64(ls.get("price")).filter(|&p| p > 1_000) {
            return RecordedAcquisition {
                date_str: None,
                date: None,
                price_usd: Some(only_price),
                deed_type: val_str(ls.get("deedType")).map(String::from),
            };
        }
    }

    if let Some(arr) = facts.get("saleHistory").and_then(Value::as_array) {
        for row in arr {
            push_cand(
                val_str(row.get("date")),
                val_i64(row.get("price")),
                val_str(row.get("deedType")).unwrap_or(""),
                val_str(row.get("note")).unwrap_or(""),
                val_str(row.get("event")).unwrap_or(""),
            );
        }
    }

    if let Some(arr) = facts
        .pointer("/titleAndLiens/transfers")
        .and_then(Value::as_array)
    {
        for row in arr {
            push_cand(
                val_str(row.get("date")),
                val_i64(row.get("price")),
                val_str(row.get("deedType")).unwrap_or(""),
                val_str(row.get("note")).unwrap_or(""),
                val_str(row.get("event")).unwrap_or("transfer"),
            );
        }
    }

    candidates.sort_by(|a, b| b.date.cmp(&a.date));

    // Prefer the most recent arm's-length priced acquisition (skipping subsequent $0 revocable-trust transfers)
    if let Some(arms_len) = candidates
        .iter()
        .find(|c| c.price.is_some() && !c.is_nominal)
    {
        return RecordedAcquisition {
            date_str: Some(arms_len.date_str.clone()),
            date: Some(arms_len.date),
            price_usd: arms_len.price,
            deed_type: (!arms_len.deed_type.is_empty()).then(|| arms_len.deed_type.clone()),
        };
    }

    if let Some(latest) = candidates.first() {
        return RecordedAcquisition {
            date_str: Some(latest.date_str.clone()),
            date: Some(latest.date),
            price_usd: latest.price,
            deed_type: (!latest.deed_type.is_empty()).then(|| latest.deed_type.clone()),
        };
    }

    RecordedAcquisition {
        date_str: None,
        date: None,
        price_usd: None,
        deed_type: None,
    }
}

fn collect_deduped_permits(ctx: &SuiteContext) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    let sources = [
        ctx.facts
            .pointer("/disclosures/permits")
            .and_then(Value::as_array),
        ctx.facts.get("permits").and_then(Value::as_array),
        ctx.facts
            .pointer("/buildingRegister/permits")
            .and_then(Value::as_array),
        ctx.condition.get("permits").and_then(Value::as_array),
    ];

    for arr in sources.into_iter().flatten() {
        for p in arr {
            let num = val_str(
                p.get("permitNumber")
                    .or_else(|| p.get("number"))
                    .or_else(|| p.get("id")),
            )
            .unwrap_or("");
            let dt = val_str(
                p.get("date")
                    .or_else(|| p.get("issueDate"))
                    .or_else(|| p.get("issuedDate"))
                    .or_else(|| p.get("filedDate")),
            )
            .unwrap_or("");
            let desc = val_str(p.get("description").or_else(|| p.get("summary"))).unwrap_or("");
            let ptype = val_str(p.get("type").or_else(|| p.get("category"))).unwrap_or("");
            if num.is_empty() && dt.is_empty() && desc.is_empty() && ptype.is_empty() {
                continue;
            }
            let dedup_key = if !num.is_empty() {
                format!("num:{}", num.to_ascii_lowercase())
            } else {
                format!(
                    "sig:{}:{}:{}",
                    dt,
                    ptype.to_ascii_lowercase(),
                    desc.chars()
                        .take(48)
                        .collect::<String>()
                        .to_ascii_lowercase()
                )
            };
            if seen.insert(dedup_key) {
                out.push(p.clone());
            }
        }
    }
    out
}

fn extract_permit_job_value(p: &Value) -> Option<f64> {
    for k in [
        "jobValue",
        "jobValueUSD",
        "jobValueUsd",
        "valuation",
        "declaredValuation",
        "estimatedCost",
        "cost",
    ] {
        if let Some(v) = val_f64(p.get(k)).filter(|&x| x > 0.0) {
            return Some(v);
        }
    }
    None
}

// =================================================================================================
// Part 1.1 — `sellerSuite.netSheetModel` (Seller Net-Sheet + IRC §121 + Custom Closing Costs)
// =================================================================================================

fn build_net_sheet_model(ctx: &SuiteContext) -> Value {
    let benchmark_price = ctx.benchmark_price_usd;
    let low_price = ctx.low_price_usd;
    let high_price = ctx.high_price_usd;

    let purchase_price = ctx.acquisition.price_usd;
    let purchase_date_str = ctx.acquisition.date_str.clone();

    // Sum documented permitted improvements strictly AFTER `recordedPurchaseDate` when job values exist
    let documented_improvements_usd: Option<i64> = ctx.acquisition.date.and_then(|acq_dt| {
        let permits = collect_deduped_permits(ctx);
        let mut sum = 0.0_f64;
        let mut found_priced = false;
        for p in &permits {
            let p_dt = val_str(
                p.get("date")
                    .or_else(|| p.get("issueDate"))
                    .or_else(|| p.get("issuedDate"))
                    .or_else(|| p.get("finalDate")),
            )
            .and_then(parse_flexible_date);
            if let Some(dt) = p_dt {
                if dt > acq_dt {
                    if let Some(jv) = extract_permit_job_value(p) {
                        sum += jv;
                        found_priced = true;
                    }
                }
            }
        }
        found_priced.then(|| sum.round() as i64)
    });

    let estimated_adjusted_basis_usd: Option<i64> =
        purchase_price.map(|pp| pp + documented_improvements_usd.unwrap_or(0));

    // Seller Transfer Tax from `buyerDiligence.cashToClose.transferTax` or exact statutory schedule
    let (
        seller_transfer_tax_usd,
        transfer_tax_rate_pct,
        transfer_tax_jurisdiction,
        transfer_tax_custom_note,
        transfer_tax_breakdown,
    ): (
        Option<i64>,
        Option<f64>,
        Option<String>,
        Option<String>,
        Vec<String>,
    ) = if let Some(price) = benchmark_price {
        let bd_tt = ctx.buyer_diligence.pointer("/cashToClose/transferTax");
        let bd_price_matches =
            val_i64(ctx.buyer_diligence.pointer("/cashToClose/purchasePriceUsd"))
                .map(|bp| (bp - price).abs() <= 1_000)
                .unwrap_or(false);
        if let (Some(tt_obj), true) = (bd_tt.filter(|v| v.is_object()), bd_price_matches) {
            let seller_sh = val_i64(tt_obj.get("sellerEstimatedShareUsd"));
            let rate = val_f64(tt_obj.get("effectiveRatePct"));
            let jur = val_str(tt_obj.get("jurisdiction")).map(String::from);
            let note = val_str(tt_obj.get("payerCustomNote")).map(String::from);
            let bdown = tt_obj
                .get("breakdown")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| val_str(Some(x)).map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (seller_sh, rate, jur, note, bdown)
        } else if !ctx.state.is_empty() {
            let tt = estimate_transfer_tax_schedule(
                &ctx.state,
                &ctx.county,
                &ctx.county_fips,
                &ctx.city,
                price as f64,
                price as f64 * 0.80,
            );
            (
                Some(tt.seller_share_usd.round() as i64),
                Some(tt.effective_rate_pct),
                Some(tt.jurisdiction),
                Some(tt.payer_custom_note),
                tt.breakdown,
            )
        } else {
            (None, None, None, None, Vec::new())
        }
    } else {
        (None, None, None, None, Vec::new())
    };

    // Seller Title & Escrow Benchmark from county/state `WhoPaysRule` ($0 Owner's Title when buyer pays!)
    let (
        seller_title_escrow_usd,
        seller_owners_title_usd,
        seller_escrow_fee_usd,
        owners_policy_payer,
        escrow_fee_payer,
        title_custom_region,
        title_custom_note,
    ): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = if let Some(price) = benchmark_price {
        let has_geo_or_td = !ctx.state.is_empty()
            || !ctx.county_fips.is_empty()
            || ctx
                .title_deed
                .pointer("/titleInsurance/customaryWhoPays/matchedRegion")
                .is_some();
        if has_geo_or_td {
            let rule = lookup_who_pays(ctx.geo);
            let td_reg = ctx
                .title_deed
                .pointer("/titleInsurance/customaryWhoPays/matchedRegion");
            let own_payer = td_reg
                .and_then(|r| val_str(r.get("ownersPolicyPayer")))
                .unwrap_or(rule.owners_policy_payer);
            let esc_payer = td_reg
                .and_then(|r| val_str(r.get("escrowFeePayer")))
                .unwrap_or(rule.escrow_fee_payer);
            let rate_regime = td_reg
                .and_then(|r| val_str(r.get("rateRegime")))
                .unwrap_or(rule.rate_regime);
            let closing_model = td_reg
                .and_then(|r| val_str(r.get("closingModel")))
                .unwrap_or(rule.closing_model);
            let reg_name = td_reg
                .and_then(|r| val_str(r.get("regionName")))
                .unwrap_or(rule.region_name);
            let custom_note = td_reg
                .and_then(|r| val_str(r.get("customNote")))
                .unwrap_or(rule.custom_note);

            let std_owner_prem = val_i64(ctx.title_deed.pointer(
                "/titleInsurance/policyComparison/ownersStandardPolicy/estimatedPremiumUsd",
            ))
            .filter(|&p| p > 0)
            .unwrap_or_else(|| {
                crate::modules::title_legal_radar::compute_state_title_rate_filing(
                    ctx.geo,
                    ctx.facts,
                    Some(price as f64),
                    ctx.as_of_date,
                )["ownerStandardUsd"]
                    .as_i64()
                    .filter(|&p| p > 0)
                    .unwrap_or_else(|| estimate_standard_owner_title_premium(price as f64, rate_regime))
            });

            let seller_owner_pol: Option<i64> = match own_payer {
                "buyer"
                | "closing_attorney"
                | "buyer_attorney_or_title"
                | "buyer_and_lender_attorneys" => Some(0),
                "seller" => Some(std_owner_prem),
                "split_50_50" => Some((std_owner_prem as f64 * 0.50).round() as i64),
                // Ambiguous statewide/negotiable customs must remain null instead of guessing 50/50.
                "varies_by_county" | "negotiable" | "buyer_or_split" => None,
                _ => None,
            };

            let is_attorney = closing_model.to_ascii_lowercase().contains("attorney");
            let full_escrow = if is_attorney { 1_350_i64 } else { 1_150_i64 };
            let seller_escrow: Option<i64> = match esc_payer {
                "buyer"
                | "closing_attorney"
                | "buyer_attorney_or_title"
                | "buyer_and_lender_attorneys" => Some(0),
                "seller" => Some(full_escrow),
                "split_50_50" => Some((full_escrow as f64 * 0.50).round() as i64),
                "buyer_or_split" | "varies_by_county" | "negotiable" => None,
                _ => None,
            };

            let combined = match (seller_owner_pol, seller_escrow) {
                (Some(o), Some(e)) => Some(o + e),
                _ => None,
            };

            (
                combined,
                seller_owner_pol,
                seller_escrow,
                Some(own_payer.to_string()),
                Some(esc_payer.to_string()),
                Some(reg_name.to_string()),
                Some(custom_note.to_string()),
            )
        } else {
            (None, None, None, None, None, None, None)
        }
    } else {
        (None, None, None, None, None, None, None)
    };

    // Real Annual & Daily Prorated Property Tax
    let (annual_property_tax_usd, property_tax_basis): (Option<i64>, Option<&'static str>) =
        if let Some(rec_tax) = ctx.recorded_annual_tax_usd {
            (Some(rec_tax), Some("recorded_parcel_tax_roll"))
        } else if let (Some(ass_v), Some(tr)) = (ctx.current_assessed_usd, ctx.effective_tax_rate) {
            (
                Some((ass_v as f64 * tr).round() as i64),
                Some("assessed_value_times_tax_rate"),
            )
        } else if let Some(mo_tax) =
            val_f64(ctx.valuation.pointer("/ownership/monthly/propertyTax")).filter(|&m| m > 0.0)
        {
            (
                Some((mo_tax * 12.0).round() as i64),
                Some("valuation_ownership_tax_estimate"),
            )
        } else if let (Some(price), Some(explicit_rate)) = (
            benchmark_price,
            val_f64(ctx.facts.get("taxRate")).filter(|&r| r > 0.0),
        ) {
            (
                Some((price as f64 * explicit_rate).round() as i64),
                Some("benchmark_value_times_parcel_tax_rate"),
            )
        } else {
            (None, None)
        };
    let daily_prorated_tax_usd = annual_property_tax_usd.map(|ann| round_n(ann as f64 / 365.0, 2));

    // IRC §121 Primary-Residence Capital Gains Exclusion
    let ownership_years_held = ctx.acquisition.date.map(|acq_dt| {
        let days = (ctx.as_of_date - acq_dt).num_days().max(0);
        round_n(days as f64 / 365.25, 2)
    });
    let meets_two_of_five = ownership_years_held.map(|yrs| yrs >= 2.0);
    let state_top_cg_pct = state_top_real_estate_ltcg_rate_pct(&ctx.state);
    let combined_top_rate_pct =
        state_top_cg_pct.map(|st_r| round_n(FEDERAL_MAX_LTCG_PCT + FEDERAL_NIIT_PCT + st_r, 2));

    let estimated_gross_gain_usd: Option<i64> =
        match (benchmark_price, estimated_adjusted_basis_usd) {
            (Some(sale_p), Some(basis)) => Some(sale_p - basis),
            _ => None,
        };

    let (taxable_single_usd, taxable_married_usd): (Option<i64>, Option<i64>) =
        if let Some(gain) = estimated_gross_gain_usd {
            if gain <= 0 {
                (Some(0), Some(0))
            } else if meets_two_of_five == Some(false) {
                // Held < 2.0 years: ineligible for full §121 exclusion unless a §121(c) unforeseen/job/health prorated safe harbor applies
                (Some(gain), Some(gain))
            } else {
                (
                    Some((gain - IRC_121_SINGLE_EXCLUSION_USD).max(0)),
                    Some((gain - IRC_121_MARRIED_EXCLUSION_USD).max(0)),
                )
            }
        } else {
            (None, None)
        };

    let est_tax_single_usd = match (taxable_single_usd, combined_top_rate_pct) {
        (Some(tg), Some(rate)) => Some((tg as f64 * (rate / 100.0)).round() as i64),
        _ => None,
    };
    let est_tax_married_usd = match (taxable_married_usd, combined_top_rate_pct) {
        (Some(tg), Some(rate)) => Some((tg as f64 * (rate / 100.0)).round() as i64),
        _ => None,
    };

    let known_statutory_closing_usd = match (seller_transfer_tax_usd, seller_title_escrow_usd) {
        (Some(tt), Some(te)) => Some(tt + te),
        (Some(tt), None) => Some(tt),
        (None, Some(te)) => Some(te),
        (None, None) => None,
    };
    let pre_debt_pre_commission_proceeds_usd = match (benchmark_price, known_statutory_closing_usd)
    {
        (Some(p), Some(sc)) => Some(p - sc),
        _ => None,
    };

    let user_editable_fields = json!([
        {
            "key": "mortgagePayoffUsd",
            "label": "Existing Mortgage / HELOC Payoff ($)",
            "unit": "usd",
            "value": Value::Null,
            "inPublicRecords": false,
            "source": "user_input_required",
            "hint": "Amortized loan principal and HELOC draws are private borrower records; enter exact lender payoff demand."
        },
        {
            "key": "listingBrokerFeePct",
            "label": "Listing Broker Compensation (%)",
            "unit": "pct",
            "value": Value::Null,
            "inPublicRecords": false,
            "source": "user_input_required",
            "hint": "Real estate commissions are 100% negotiable by written listing agreement and are never fixed by law."
        },
        {
            "key": "buyerBrokerConcessionPct",
            "label": "Optional Buyer-Agent / Closing Concession (%)",
            "unit": "pct",
            "value": Value::Null,
            "inPublicRecords": false,
            "source": "user_input_required",
            "hint": "Post-NAR settlement practice: buyer-broker compensation or 2-1 rate-buydown credit is negotiated offer-by-offer."
        },
        {
            "key": "prepAndCreditBudgetUsd",
            "label": "Pre-Listing Prep, Staging & Repair Credit Budget ($)",
            "unit": "usd",
            "value": Value::Null,
            "inPublicRecords": false,
            "source": "user_input_required",
            "hint": "Enter planned painting, staging, inspection cure, or buyer escrow repair credits."
        }
    ]);

    json!({
        "benchmarkSalePriceUsd": benchmark_price,
        "lowSalePriceUsd": low_price,
        "highSalePriceUsd": high_price,
        "recordedPurchasePriceUsd": purchase_price,
        "recordedPurchaseDate": purchase_date_str,
        "documentedPermittedImprovementsUsd": documented_improvements_usd,
        "estimatedAdjustedBasisUsd": estimated_adjusted_basis_usd,
        "sellerTransferTaxUsd": seller_transfer_tax_usd,
        "transferTaxRatePct": transfer_tax_rate_pct,
        "transferTaxJurisdiction": transfer_tax_jurisdiction,
        "transferTaxCustomNote": transfer_tax_custom_note,
        "transferTaxBreakdown": transfer_tax_breakdown,
        "sellerTitleAndEscrowBenchmarkUsd": seller_title_escrow_usd,
        "sellerOwnersTitleBenchmarkUsd": seller_owners_title_usd,
        "sellerEscrowSettlementBenchmarkUsd": seller_escrow_fee_usd,
        "ownersTitlePayerCustom": owners_policy_payer,
        "escrowFeePayerCustom": escrow_fee_payer,
        "titleCustomRegion": title_custom_region,
        "titleCustomNote": title_custom_note,
        "knownStatutoryClosingCostsUsd": known_statutory_closing_usd,
        "preDebtPreCommissionProceedsUsd": pre_debt_pre_commission_proceeds_usd,
        "annualPropertyTaxUsd": annual_property_tax_usd,
        "annualPropertyTaxBasis": property_tax_basis,
        "dailyProratedTaxUsd": daily_prorated_tax_usd,
        "mortgagePayoffUsd": Value::Null,
        "listingBrokerFeePct": Value::Null,
        "buyerBrokerConcessionPct": Value::Null,
        "prepAndCreditBudgetUsd": Value::Null,
        "irc121Exclusion": {
            "singleExclusionUsd": IRC_121_SINGLE_EXCLUSION_USD,
            "marriedExclusionUsd": IRC_121_MARRIED_EXCLUSION_USD,
            "ownershipYearsHeld": ownership_years_held,
            "meetsTwoOfFiveYearTenure": meets_two_of_five,
            "estimatedGrossGainUsd": estimated_gross_gain_usd,
            "taxableGainSingleUsd": taxable_single_usd,
            "taxableGainMarriedUsd": taxable_married_usd,
            "estimatedMaxTaxSingleUsd": est_tax_single_usd,
            "estimatedMaxTaxMarriedUsd": est_tax_married_usd,
            "federalMaxLtCgPct": FEDERAL_MAX_LTCG_PCT,
            "niitPct": FEDERAL_NIIT_PCT,
            "stateTopCgPct": state_top_cg_pct,
            "combinedTopMarginalRatePct": combined_top_rate_pct,
            "disclaimer": "Educational IRC §121 primary-residence estimate; consult a CPA or tax attorney regarding 2-of-5-year occupancy, selling-expense basis adjustments, ADU/rental depreciation recapture (IRC §1250), partial exclusions (IRC §121(c)), and state conformity."
        },
        "userEditableFields": user_editable_fields,
    })
}

// =================================================================================================
// Part 1.2 — `sellerSuite.taxPortability` (CA Prop 19 / FL Save Our Homes / TX Over-65)
// =================================================================================================

fn build_tax_portability(ctx: &SuiteContext) -> Value {
    let assessed = ctx.current_assessed_usd;
    let market_val = ctx.benchmark_price_usd;
    let tax_rate = ctx.effective_tax_rate;
    let tax_rate_pct = tax_rate.map(|r| round_n(r * 100.0, 4));

    let current_annual_tax = ctx
        .recorded_annual_tax_usd
        .or_else(|| match (assessed, tax_rate) {
            (Some(av), Some(tr)) => Some((av as f64 * tr).round() as i64),
            _ => None,
        });
    let unported_new_buyer_tax = match (market_val, tax_rate) {
        (Some(mv), Some(tr)) => Some((mv as f64 * tr).round() as i64),
        _ => None,
    };

    match ctx.state.as_str() {
        "CA" => {
            let diff_usd = match (market_val, assessed) {
                (Some(mv), Some(av)) => Some((mv - av).max(0)),
                _ => None,
            };
            let annual_prop19_savings = match (diff_usd, tax_rate) {
                (Some(d), Some(tr)) => Some((d as f64 * tr).round() as i64),
                _ => None,
            };
            let prop8_over_assessed = match (assessed, market_val) {
                (Some(av), Some(mv)) => Some(av > mv),
                _ => None,
            };
            let summary_note = match (assessed, market_val, annual_prop19_savings) {
                (Some(av), Some(mv), Some(sav)) if av < mv => format!(
                    "Current Prop 13 roll ({}) sits {} below benchmark market value ({}). Under CA Prop 19, eligible 55+, severely disabled, or wildfire/disaster-displaced sellers can port this base-year value anywhere in California within 2 years — saving ~{}/yr vs. an unported purchase of an equal-or-lesser replacement home.",
                    fmt_usd(av as f64),
                    fmt_usd((mv - av) as f64),
                    fmt_usd(mv as f64),
                    fmt_usd(sav as f64)
                ),
                (Some(av), Some(mv), _) if av >= mv => format!(
                    "Current Prop 13 roll ({}) is already at or above benchmark market value ({}), so a Prop 19 base-year transfer yields $0 immediate tax reduction (if staying, check County Assessor Prop 8 decline-in-value review instead).",
                    fmt_usd(av as f64),
                    fmt_usd(mv as f64)
                ),
                _ => "California Prop 19 allows eligible 55+, severely disabled, or disaster-victim homeowners to transfer their Prop 13 factored base-year value to a replacement CA primary residence within 2 years; supply parcel assessed roll & market value to quantify exact annual savings.".to_string(),
            };

            json!({
                "applicable": true,
                "state": "CA",
                "programCode": "CA_PROP_19",
                "programName": "California Proposition 19 Base-Year Value Transfer",
                "statute": "Cal. Const. art. XIIIA, § 2.1 · Cal. Rev. & Tax. Code § 69.6",
                "filingForm": "BOE-19-B (Age 55+) / BOE-19-D (Disabled) / BOE-19-P (Disaster)",
                "replacementWindowYears": 2,
                "lifetimeTransferLimit": 3,
                "currentAssessedValueUsd": assessed,
                "benchmarkSalePriceUsd": market_val,
                "effectiveTaxRatePct": tax_rate_pct,
                "assessmentDifferentialUsd": diff_usd,
                "portableCapUsd": Value::Null,
                "portableAssessmentReductionUsd": diff_usd,
                "currentAnnualTaxUsd": current_annual_tax,
                "unportedNewBuyerAnnualTaxUsd": unported_new_buyer_tax,
                "estimatedAnnualProp19TaxSavingsUsd": annual_prop19_savings,
                "estimatedAnnualTaxSavingsUsd": annual_prop19_savings,
                "estimated10YrCumulativeSavingsUsd": annual_prop19_savings.map(|s| s * 10),
                "isOverAssessedProp8Candidate": prop8_over_assessed,
                "upwardAdjustmentFormulaNote": "Equal-or-lesser replacement home: 100% of current Prop 13 factored base-year value transfers (adjusted by 100% if bought prior to sale, 105% within 1 year after sale, or 110% within 2 years after sale). Pricier replacement home: New Taxable Value = Transferred Factored Base-Year Value + (Replacement Purchase Price − Adjusted Sale Price Threshold).",
                "summary": summary_note,
            })
        }
        "FL" => {
            let cap_usd: i64 = 500_000;
            let raw_diff = match (market_val, assessed) {
                (Some(mv), Some(av)) => Some((mv - av).max(0)),
                _ => None,
            };
            let portable_red = raw_diff.map(|d| d.min(cap_usd));
            let annual_soh_savings = match (portable_red, tax_rate) {
                (Some(pr), Some(tr)) => Some((pr as f64 * tr).round() as i64),
                _ => None,
            };
            let summary_note = match (assessed, market_val, portable_red, annual_soh_savings) {
                (Some(av), Some(mv), Some(pr), Some(sav)) if pr > 0 => format!(
                    "Save Our Homes (SOH) assessed value ({}) vs. market value ({}) creates a {} portable cap differential (statutory max $500,000), saving ~{}/yr on an equal-or-higher-value Florida replacement homestead.",
                    fmt_usd(av as f64),
                    fmt_usd(mv as f64),
                    fmt_usd(pr as f64),
                    fmt_usd(sav as f64)
                ),
                _ => "Florida Save Our Homes (SOH) portability allows homesteaded sellers to transfer up to $500,000 of accrued Just-minus-Assessed value differential to a new FL homestead within 3 tax years (Form DR-501T).".to_string(),
            };

            json!({
                "applicable": true,
                "state": "FL",
                "programCode": "FL_SAVE_OUR_HOMES",
                "programName": "Florida Save Our Homes (SOH) Homestead Portability",
                "statute": "Fla. Const. art. VII, § 4(d)(8) · Fla. Stat. § 193.155(8)",
                "filingForm": "Florida DOR Form DR-501T (filed with DR-501 Homestead Application by March 1)",
                "replacementWindowYears": 3,
                "lifetimeTransferLimit": Value::Null,
                "currentAssessedValueUsd": assessed,
                "benchmarkSalePriceUsd": market_val,
                "effectiveTaxRatePct": tax_rate_pct,
                "assessmentDifferentialUsd": raw_diff,
                "portableCapUsd": cap_usd,
                "portableAssessmentReductionUsd": portable_red,
                "currentAnnualTaxUsd": current_annual_tax,
                "unportedNewBuyerAnnualTaxUsd": unported_new_buyer_tax,
                "estimatedAnnualProp19TaxSavingsUsd": Value::Null,
                "estimatedAnnualTaxSavingsUsd": annual_soh_savings,
                "estimated10YrCumulativeSavingsUsd": annual_soh_savings.map(|s| s * 10),
                "upwardAdjustmentFormulaNote": "Upsizing (new Just Value >= prior Just Value): transfer dollar-for-dollar differential up to $500,000. Downsizing (new Just Value < prior Just Value): transfer proportional differential = (New Just Value ÷ Prior Just Value) × Prior SOH Differential (capped at $500,000).",
                "summary": summary_note,
            })
        }
        "TX" => {
            let raw_diff = match (market_val, assessed) {
                (Some(mv), Some(av)) => Some((mv - av).max(0)),
                _ => None,
            };
            // Texas school-district (ISD) M&O + I&S typically accounts for ~48% of combined TX property tax rate
            let isd_share = 0.48_f64;
            let annual_tx_ceiling_savings = match (raw_diff, tax_rate) {
                (Some(d), Some(tr)) => Some((d as f64 * tr * isd_share).round() as i64),
                _ => None,
            };
            let summary_note = match (assessed, market_val, annual_tx_ceiling_savings) {
                (Some(av), Some(mv), Some(sav)) if sav > 0 => format!(
                    "Texas Over-65 / Disabled homeowners can transfer their frozen ISD school-tax ceiling percentage from the prior assessed basis ({}) to a replacement Texas homestead (vs. {} market value), preserving ~{}/yr in estimated ISD ceiling relief.",
                    fmt_usd(av as f64),
                    fmt_usd(mv as f64),
                    fmt_usd(sav as f64)
                ),
                _ => "Texas Tax Code § 11.26(g) lets homeowners age 65+ or disabled transfer their school-district (ISD) tax ceiling percentage to any replacement Texas homestead using a Tax Ceiling Certificate from the prior Appraisal District.".to_string(),
            };

            json!({
                "applicable": true,
                "state": "TX",
                "programCode": "TX_OVER65_ISD_CEILING",
                "programName": "Texas Over-65 / Disabled School-Tax Ceiling Portability",
                "statute": "Tex. Const. art. VIII, § 1-b(d) · Tex. Tax Code § 11.26(g)",
                "filingForm": "Comptroller Form 50-114 + Prior CAD Tax Ceiling Transfer Certificate",
                "replacementWindowYears": Value::Null,
                "lifetimeTransferLimit": Value::Null,
                "currentAssessedValueUsd": assessed,
                "benchmarkSalePriceUsd": market_val,
                "effectiveTaxRatePct": tax_rate_pct,
                "assessmentDifferentialUsd": raw_diff,
                "portableCapUsd": Value::Null,
                "portableAssessmentReductionUsd": raw_diff,
                "currentAnnualTaxUsd": current_annual_tax,
                "unportedNewBuyerAnnualTaxUsd": unported_new_buyer_tax,
                "estimatedAnnualProp19TaxSavingsUsd": Value::Null,
                "estimatedAnnualTaxSavingsUsd": annual_tx_ceiling_savings,
                "estimated10YrCumulativeSavingsUsd": annual_tx_ceiling_savings.map(|s| s * 10),
                "upwardAdjustmentFormulaNote": "New ISD school-tax ceiling = (Unfrozen ISD Tax on Replacement Homestead) × (Prior Frozen ISD Tax Ceiling ÷ Unfrozen ISD Tax on Sold Homestead in final year), plus the $100,000 general + $10,000 Over-65/Disabled ISD homestead exemptions.",
                "summary": summary_note,
            })
        }
        other => json!({
            "applicable": false,
            "state": if other.is_empty() { Value::Null } else { json!(other) },
            "programCode": Value::Null,
            "programName": Value::Null,
            "statute": Value::Null,
            "currentAssessedValueUsd": assessed,
            "benchmarkSalePriceUsd": market_val,
            "effectiveTaxRatePct": tax_rate_pct,
            "currentAnnualTaxUsd": current_annual_tax,
            "unportedNewBuyerAnnualTaxUsd": unported_new_buyer_tax,
            "estimatedAnnualProp19TaxSavingsUsd": Value::Null,
            "estimatedAnnualTaxSavingsUsd": Value::Null,
            "upwardAdjustmentFormulaNote": Value::Null,
            "summary": if other.is_empty() {
                "State jurisdiction unavailable; property-tax portability screening skipped."
            } else {
                "No statewide Prop 19 / Save Our Homes style assessment-cap portability statute applies; check local senior/veteran homestead exemptions in replacement county."
            },
        }),
    }
}

// =================================================================================================
// Part 1.3 — `sellerSuite.cureBeforeEscrow` (Permits, Code Enforcement, POS Mandates, Repair/Credit)
// =================================================================================================

fn is_permit_closed_or_finaled(p: &Value) -> bool {
    if val_str(
        p.get("finalDate")
            .or_else(|| p.get("closedDate"))
            .or_else(|| p.get("completedDate")),
    )
    .is_some()
    {
        let st_lc = val_str(p.get("status")).unwrap_or("").to_ascii_lowercase();
        if !st_lc.contains("expired") && !st_lc.contains("revoked") && !st_lc.contains("cancel") {
            return true;
        }
    }
    let st = val_str(p.get("status")).unwrap_or("").to_ascii_lowercase();
    if st.is_empty() {
        // Historical recorded permit without open/expired flag is not treated as an active violation
        return true;
    }
    let closed_tokens = [
        "final",
        "complete",
        "closed",
        "co issued",
        "cert of occupancy",
        "certificate of occupancy",
        "done",
        "passed",
        "approved & final",
    ];
    if closed_tokens.iter().any(|tok| st.contains(tok))
        && !st.contains("unfinal")
        && !st.contains("not final")
        && !st.contains("incomplete")
    {
        return true;
    }
    false
}

fn is_east_bay_psl_city_or_county(state: &str, county_fips: &str, city: &str) -> bool {
    if state != "CA" {
        return false;
    }
    if matches!(county_fips, "06001" | "06013") {
        return true;
    }
    let city_lc = city.to_ascii_lowercase();
    [
        "oakland",
        "berkeley",
        "alameda",
        "albany",
        "emeryville",
        "piedmont",
        "el cerrito",
        "richmond",
        "kensington",
        "san leandro",
        "castro valley",
        "hayward",
        "walnut creek",
        "orinda",
        "moraga",
        "lafayette",
    ]
    .iter()
    .any(|c| city_lc.contains(c))
}

fn build_cure_before_escrow(report: &Value, ctx: &SuiteContext) -> Value {
    // 1. Open / Unfinaled / Expired Permits
    let all_permits = collect_deduped_permits(ctx);
    let mut open_permits: Vec<Value> = Vec::new();
    for p in &all_permits {
        if !is_permit_closed_or_finaled(p) {
            let status = val_str(p.get("status")).unwrap_or("Unfinaled");
            let st_lc = status.to_ascii_lowercase();
            let severity = if st_lc.contains("expired")
                || st_lc.contains("revoked")
                || st_lc.contains("stop")
                || st_lc.contains("violation")
            {
                "expired_or_stop_work"
            } else {
                "open_or_in_review"
            };
            let num = val_str(
                p.get("permitNumber")
                    .or_else(|| p.get("number"))
                    .or_else(|| p.get("id")),
            );
            let dt = val_str(
                p.get("date")
                    .or_else(|| p.get("issueDate"))
                    .or_else(|| p.get("issuedDate")),
            );
            let ptype = val_str(p.get("type").or_else(|| p.get("category")));
            let desc = val_str(p.get("description").or_else(|| p.get("summary")));
            open_permits.push(json!({
                "permitNumber": num,
                "date": dt,
                "type": ptype,
                "status": status,
                "severity": severity,
                "jobValueUsd": extract_permit_job_value(p).map(|v| v.round() as i64),
                "description": desc,
                "sellerAction": if severity == "expired_or_stop_work" {
                    "Schedule city building-inspector reactivation/final sign-off or disclose explicitly in seller TDS/SPQ before going active."
                } else {
                    "Request final municipal inspection sign-off or attach full permit status packet to pre-listing disclosures so buyers do not demand escrow holdbacks."
                },
            }));
        }
    }

    // 2. Code Enforcement
    let ce_block = ctx
        .facts
        .pointer("/disclosures/codeEnforcement")
        .filter(|v| !v.is_null())
        .or_else(|| ctx.facts.get("codeEnforcement").filter(|v| !v.is_null()));

    let (ce_open_count, ce_closed_count, ce_items): (Option<i64>, Option<i64>, Vec<Value>) =
        if let Some(ce) = ce_block {
            if let Some(arr) = ce.as_array() {
                let open_n = arr
                    .iter()
                    .filter(|it| {
                        let st = val_str(it.get("status"))
                            .unwrap_or("open")
                            .to_ascii_lowercase();
                        !st.contains("closed") && !st.contains("resolved") && !st.contains("abated")
                    })
                    .count() as i64;
                let closed_n = (arr.len() as i64 - open_n).max(0);
                (Some(open_n), Some(closed_n), arr.clone())
            } else if ce.is_object() {
                let items = ce
                    .get("items")
                    .or_else(|| ce.get("cases"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let open_n = val_i64(ce.get("openCount")).unwrap_or_else(|| {
                    items
                        .iter()
                        .filter(|it| {
                            let st = val_str(it.get("status"))
                                .unwrap_or("open")
                                .to_ascii_lowercase();
                            !st.contains("closed") && !st.contains("resolved")
                        })
                        .count() as i64
                });
                let closed_n = val_i64(ce.get("closedCount")).unwrap_or(0);
                (Some(open_n), Some(closed_n), items)
            } else {
                (None, None, Vec::new())
            }
        } else {
            (None, None, Vec::new())
        };

    // 3. Point-of-Sale Mandates (merge `buyerDiligence` + seller-specific ordinances)
    let mut mandates: Vec<Value> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut push_mandate = |m: Value| {
        let k = val_str(m.get("key"))
            .or_else(|| val_str(m.get("title")))
            .unwrap_or("")
            .to_ascii_lowercase();
        if !k.is_empty() && seen_keys.insert(k) {
            mandates.push(m);
        }
    };

    for ptr in [
        "/utilitiesAndMandates/pointOfSaleMandates/mandates",
        "/pointOfSaleMandates/mandates",
        "/pointOfSaleMandates",
    ] {
        if let Some(arr) = ctx.buyer_diligence.pointer(ptr).and_then(Value::as_array) {
            for m in arr {
                push_mandate(m.clone());
            }
        }
    }

    let year_built = val_i64(ctx.facts.get("yearBuilt")).filter(|&y| y > 1700);
    let in_high_ca_fhsz = ctx.state == "CA"
        && (ctx
            .hazards
            .pointer("/wildfire/california/inFhsz")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || val_i64(ctx.hazards.pointer("/wildfire/california/fhszClass"))
                .map(|c| c >= 2)
                .unwrap_or(false)
            || val_str(ctx.hazards.pointer("/wildfire/california/fhsz"))
                .map(|s| {
                    let lc = s.to_ascii_lowercase();
                    (lc.contains("high") || lc.contains("very high")) && !lc.contains("not in")
                })
                .unwrap_or(false)
            || ctx
                .facts
                .pointer("/disclosures/nhdZones")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter().any(|z| {
                        let k = val_str(z.get("key")).unwrap_or("");
                        let st = val_str(z.get("status")).unwrap_or("");
                        (k.contains("Fire") || k.contains("fire")) && st.eq_ignore_ascii_case("yes")
                    })
                })
                .unwrap_or(false));

    if in_high_ca_fhsz {
        push_mandate(json!({
            "key": "ca_ab38_defensible_space",
            "title": "CA AB-38 Wildfire Defensible-Space Inspection & Home-Hardening Disclosure",
            "authority": "CA Civil Code §§ 1102.19 & 1102.6f (PRC § 4291)",
            "requiredAtSale": true,
            "estCostRangeUsd": "$0–$200 CAL FIRE / Fire Dept inspection (+ brush clearance if needed)",
            "detail": "Properties in a High or Very High Fire Hazard Severity Zone require documented CAL FIRE / local fire-agency AB-38 defensible-space compliance within 6 months prior to close (or written buyer compliance agreement) plus Home-Hardening disclosure on pre-2010 homes.",
        }));
    }

    if is_east_bay_psl_city_or_county(&ctx.state, &ctx.county_fips, &ctx.city) {
        push_mandate(json!({
            "key": "east_bay_sewer_lateral",
            "title": "East Bay EBMUD Regional Private Sewer Lateral (PSL) Compliance Certificate",
            "authority": "EBMUD Regional PSL Ordinance (Alameda & Contra Costa Counties)",
            "requiredAtSale": true,
            "estCostRangeUsd": "$500 verification test to $4,000–$18,000+ trenchless lateral replacement",
            "detail": "Title transfer triggers mandatory Private Sewer Lateral pressure testing and EBMUD Compliance Certificate prior to close (or a $4,500 refundable EBMUD time-extension deposit).",
        }));
    }

    if ctx.state == "CA" {
        push_mandate(json!({
            "key": "ca_water_heater_seismic_strap",
            "title": "CA Water-Heater Double Seismic Strapping & Smoke/CO Alarm Certification",
            "authority": "CA Health & Safety Code §§ 19211, 13113.8 & 17926",
            "requiredAtSale": true,
            "estCostRangeUsd": "$150–$450 straps & 10-year sealed alarms",
            "detail": "California sellers must certify in writing that tank water heaters are double-strapped in the upper/lower thirds to wall studs and that compliant 10-year sealed smoke alarms and carbon-monoxide detectors are installed before appraiser walkthrough.",
        }));
        if year_built.map(|y| y <= 1994).unwrap_or(false) {
            push_mandate(json!({
                "key": "ca_low_flow_plumbing",
                "title": "CA Water-Conserving Plumbing Fixture Point-of-Sale Disclosure",
                "authority": "CA Civil Code § 1101.4",
                "requiredAtSale": true,
                "estCostRangeUsd": "$200–$1,200 fixture upgrade",
                "detail": "Pre-1994 California single-family residences require seller disclosure regarding compliant low-flow toilets (<=1.6 gpf), showerheads (<=2.5 gpm), and interior faucets.",
            }));
        }
    }

    if year_built.map(|y| y < 1978).unwrap_or(false) {
        push_mandate(json!({
            "key": "federal_lead_paint",
            "title": "Federal Pre-1978 Lead-Based Paint Disclosure & EPA Pamphlet",
            "authority": "42 U.S.C. § 4852d (24 CFR Part 35 / 40 CFR Part 745)",
            "requiredAtSale": true,
            "estCostRangeUsd": "$0 mandatory disclosure",
            "detail": "Sellers of pre-1978 residential housing must deliver the EPA Protect Your Family From Lead in Your Home pamphlet, disclose known lead hazards, and grant (or obtain signed waiver of) a 10-day inspection period.",
        }));
    }

    let pt_lc = val_str(ctx.facts.get("propertyType"))
        .unwrap_or("")
        .to_ascii_lowercase();
    let has_hoa = val_f64(ctx.facts.get("hoaMonthly"))
        .map(|m| m > 0.0)
        .unwrap_or(false)
        || ctx
            .hoa_profile
            .get("hasHoa")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || pt_lc.contains("condo")
        || pt_lc.contains("town")
        || pt_lc.contains("co-op");

    if has_hoa {
        push_mandate(json!({
            "key": "hoa_resale_estoppel_packet",
            "title": "HOA Resale Disclosure Packet & Estoppel / Demand Certificate Lead-Time",
            "authority": match ctx.state.as_str() {
                "CA" => "CA Civil Code §§ 4525 & 4530 (10-day statutory HOA delivery)",
                "FL" => "Fla. Stat. §§ 718.116(8) & 720.30851 (10-business-day estoppel)",
                "TX" => "Tex. Prop. Code § 207.003 (Resale Certificate)",
                "WA" => "RCW 64.90.640 (WUCIOA Resale Certificate)",
                _ => "State Common-Interest Resale Certificate Statute",
            },
            "requiredAtSale": true,
            "estCostRangeUsd": "$250–$650 HOA document & demand preparation fee",
            "detail": "Order the official HOA Resale Package, Reserve Study, Master Insurance EOI, and Estoppel Demand BEFORE going live on MLS; management companies take 7–14 days and late delivery restarts buyer rescission/contingency clocks.",
        }));
    }

    // 4. Repair vs. Credit Ranking (grounded in `remodelAndRebuild` + `condition.systems`)
    let mut ranking: Vec<Value> = Vec::new();
    let computed_remodel = if ctx.remodel.get("remodelProjects").is_none()
        && val_f64(ctx.facts.get("sqft"))
            .filter(|&s| s > 0.0)
            .is_some()
    {
        Some(crate::modules::remodel_rebuild::build_remodel_and_rebuild(report).data)
    } else {
        None
    };
    let remodel_src = computed_remodel.as_ref().unwrap_or(ctx.remodel);

    if let Some(projects) = remodel_src.get("remodelProjects").and_then(Value::as_array) {
        for proj in projects {
            let Some(recoup_pct) = val_f64(proj.get("costRecoupedPct")) else {
                continue;
            };
            let Some(typ_usd) = val_i64(proj.get("typicalUsd")) else {
                continue;
            };
            let id = val_str(proj.get("id")).unwrap_or("project");
            // Focus pre-sale triage on actionable pre-listing refresh vs major over-improvements
            if matches!(id, "home-addition-400" | "garage-adu" | "detached-adu") {
                continue;
            }
            let resale_added = val_i64(proj.get("resaleValueAddedUsd"))
                .unwrap_or_else(|| (typ_usd as f64 * (recoup_pct / 100.0)).round() as i64);
            let (recommendation, rationale) = if recoup_pct >= 95.0 {
                (
                    "prep_before_listing",
                    format!(
                        "High resale ROI ({:.1}% cost recouped): completing before photos/launch broadens turnkey buyer pool and defends top-of-range pricing.",
                        recoup_pct
                    ),
                )
            } else if recoup_pct >= 85.0 && typ_usd <= 15_000 {
                (
                    "prep_before_listing",
                    format!(
                        "Fast cosmetic refresh ({:.1}% recouped, {}) that eliminates first-impression buyer objections without permit delays.",
                        recoup_pct,
                        proj.get("timelineWeeks")
                            .and_then(Value::as_str)
                            .unwrap_or("1 wk")
                    ),
                )
            } else if recoup_pct >= 65.0 {
                (
                    "price_in_or_offer_escrow_credit",
                    format!(
                        "Moderate recoup ({:.1}%): avoid multi-week contractor/permit holding carry unless system is actively failing; price transparently or hold back as negotiation credit.",
                        recoup_pct
                    ),
                )
            } else {
                (
                    "skip_major_remodel_sell_as_is",
                    format!(
                        "Low pre-sale recoup ({:.1}%): buyers prefer choosing their own custom finishes — do not undertake a gut remodel purely to sell.",
                        recoup_pct
                    ),
                )
            };

            ranking.push(json!({
                "id": id,
                "item": proj.get("name"),
                "source": "remodelAndRebuild",
                "estimatedCostUsd": typ_usd,
                "estimatedResaleValueAddedUsd": resale_added,
                "costRecoupedPct": recoup_pct,
                "timeline": proj.get("timelineWeeks"),
                "recommendation": recommendation,
                "rationale": rationale,
            }));
        }
    }

    // Append real deferred-maintenance / end-of-life systems from `condition` when present
    let cond_systems_iter: Vec<&Value> =
        if let Some(arr) = ctx.condition.get("systemsList").and_then(Value::as_array) {
            arr.iter().collect()
        } else if let Some(map) = ctx.condition.get("systems").and_then(Value::as_object) {
            map.values().collect()
        } else {
            Vec::new()
        };

    for sys in cond_systems_iter {
        let rul = val_i64(sys.get("remainingUsefulLifeYears"));
        let prob = val_f64(sys.get("replacementProb5YrPct")).unwrap_or(0.0);
        let risk = val_str(sys.get("riskLevel")).unwrap_or("");
        if rul.map(|r| r <= 4).unwrap_or(false) || prob >= 60.0 || risk == "high" {
            let exp_cost = val_i64(sys.pointer("/replacementCostRangeUSD/expected"));
            let sys_key = val_str(sys.get("key")).unwrap_or("system");
            ranking.push(json!({
                "id": format!("deferred-{sys_key}"),
                "item": sys.get("system"),
                "source": "condition.systems",
                "estimatedCostUsd": exp_cost,
                "estimatedResaleValueAddedUsd": Value::Null,
                "costRecoupedPct": Value::Null,
                "remainingUsefulLifeYears": rul,
                "timeline": Value::Null,
                "recommendation": "price_in_or_offer_escrow_credit",
                "rationale": format!(
                    "System near end of useful life ({} yrs remaining, {:.0}% 5-yr replacement odds): obtain a pre-listing specialist inspection + written contractor bid so buyers cannot inflate repair credit demands in escrow.",
                    rul.unwrap_or(0),
                    prob
                ),
            }));
        }
    }

    // Sort: `prep_before_listing` highest recoup first, then `price_in_or_offer_escrow_credit`, then `skip_major_remodel_sell_as_is`
    let rank_bucket = |rec: &str| -> u8 {
        match rec {
            "prep_before_listing" => 0,
            "price_in_or_offer_escrow_credit" => 1,
            _ => 2,
        }
    };
    ranking.sort_by(|a, b| {
        let ba = rank_bucket(val_str(a.get("recommendation")).unwrap_or(""));
        let bb = rank_bucket(val_str(b.get("recommendation")).unwrap_or(""));
        ba.cmp(&bb).then_with(|| {
            let ra = val_f64(a.get("costRecoupedPct")).unwrap_or(50.0);
            let rb = val_f64(b.get("costRecoupedPct")).unwrap_or(50.0);
            rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    json!({
        "openOrUnfinaledPermitsCount": open_permits.len(),
        "openOrUnfinaledPermits": open_permits,
        "codeEnforcementOpenCount": ce_open_count,
        "codeEnforcementClosedCount": ce_closed_count,
        "codeEnforcementItems": ce_items,
        "pointOfSaleMandates": mandates,
        "repairVsCreditRanking": ranking,
    })
}

// =================================================================================================
// Part 1.4 — `sellerSuite.launchTimingAndBracket` (12-Month ZIP Seasonality & Bracket Cliff)
// =================================================================================================

fn extract_monthly_market_series(market: &Value) -> (Vec<(String, f64)>, Option<&'static str>) {
    let candidates: [(&str, &'static str); 6] = [
        ("/zhvi/series", "modules.market.zhvi.series"),
        ("/series/zipSfr", "modules.market.series.zipSfr"),
        ("/series/zip", "modules.market.series.zip"),
        ("/zori/series", "modules.market.zori.series"),
        ("/series/zori", "modules.market.series.zori"),
        ("/series/metro", "modules.market.series.metro"),
    ];
    for (ptr, label) in candidates {
        let raw = value_array_to_series(market.pointer(ptr));
        if raw.len() >= 36 {
            return (raw, Some(label));
        }
    }
    // Check if a shorter series exists so we report honest sample count while still refusing synthetic curves
    for (ptr, label) in candidates {
        let raw = value_array_to_series(market.pointer(ptr));
        if !raw.is_empty() {
            return (raw, Some(label));
        }
    }
    (Vec::new(), None)
}

fn build_launch_timing_and_bracket(ctx: &SuiteContext) -> Value {
    // 1. 12-Month Real Historical Seasonality Profile (>= 36 monthly points required; ZERO synthetic curve)
    let (raw_series, series_src) = extract_monthly_market_series(ctx.market);
    let mut parsed_pts: Vec<((i32, u32), String, f64)> = raw_series
        .into_iter()
        .filter(|(_, v)| v.is_finite() && *v > 0.0)
        .filter_map(|(d, v)| {
            let ym = parse_ym_pair(&d)?;
            Some((ym, format!("{:04}-{:02}", ym.0, ym.1), v))
        })
        .collect();
    parsed_pts.sort_by(|a, b| a.0.cmp(&b.0));
    parsed_pts.dedup_by(|a, b| a.0 == b.0);

    let sample_months = parsed_pts.len();
    let mut by_month_pct: [Vec<f64>; 12] = Default::default();
    let mut by_month_log: [Vec<f64>; 12] = Default::default();
    let mut all_mom_pcts: Vec<f64> = Vec::new();

    if sample_months >= 36 {
        for pair in parsed_pts.windows(2) {
            let prev = &pair[0];
            let cur = &pair[1];
            if months_diff(prev.0, cur.0) == 1 && prev.2 > 0.0 {
                let pct = ((cur.2 / prev.2) - 1.0) * 100.0;
                let log_pct = (cur.2 / prev.2).ln() * 100.0;
                let m_idx = (cur.0 .1 - 1) as usize;
                by_month_pct[m_idx].push(pct);
                by_month_log[m_idx].push(log_pct);
                all_mom_pcts.push(pct);
            }
        }
    }

    let all_months_populated = sample_months >= 36
        && all_mom_pcts.len() >= 35
        && by_month_pct.iter().all(|v| !v.is_empty());

    let (seasonality_available, strongest_months, softest_months, monthly_index, seasonality_note) =
        if all_months_populated {
            let overall_mean = all_mom_pcts.iter().sum::<f64>() / (all_mom_pcts.len() as f64);
            let mut month_rows: Vec<(usize, f64, Value)> = Vec::with_capacity(12);
            for m_idx in 0..12 {
                let vals = &by_month_pct[m_idx];
                let log_vals = &by_month_log[m_idx];
                let n = vals.len();
                let mean_pct = vals.iter().sum::<f64>() / (n as f64);
                let mean_log = log_vals.iter().sum::<f64>() / (n as f64);
                let med_pct = median_f64(vals).unwrap_or(mean_pct);
                let pos_share =
                    (vals.iter().filter(|&&x| x > 0.0).count() as f64 / (n as f64)) * 100.0;
                let rel_lift = mean_pct - overall_mean;
                // Normalized index centered at 100.0 strictly from real de-trended monthly returns
                let idx_val = round_n(100.0 + rel_lift * 10.0, 2);
                let m_name = CALENDAR_MONTH_NAMES[m_idx];
                month_rows.push((
                    m_idx,
                    mean_pct,
                    json!({
                        "month": m_idx + 1,
                        "monthName": m_name,
                        "monthShort": &m_name[..3],
                        "observations": n,
                        "avgMomPct": round_n(mean_pct, 3),
                        "avgMomLogPct": round_n(mean_log, 3),
                        "medianMomPct": round_n(med_pct, 3),
                        "seasonalLiftVsTrendPct": round_n(rel_lift, 3),
                        "positiveSharePct": round_n(pos_share, 1),
                        "seasonalityIndex": idx_val,
                    }),
                ));
            }

            let mut ranked = month_rows.clone();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let strongest: Vec<String> = ranked
                .iter()
                .take(3)
                .map(|(i, _, _)| CALENDAR_MONTH_NAMES[*i].to_string())
                .collect();
            let softest: Vec<String> = ranked
                .iter()
                .rev()
                .take(3)
                .map(|(i, _, _)| CALENDAR_MONTH_NAMES[*i].to_string())
                .collect();

            let first_ym = parsed_pts.first().map(|p| p.1.as_str()).unwrap_or("");
            let last_ym = parsed_pts.last().map(|p| p.1.as_str()).unwrap_or("");
            let note = format!(
            "Computed from {} real monthly ZIP index observations ({first_ym} to {last_ym}): strongest price momentum occurs in {} (note: recorded closings lag MLS list launch by ~30–45 days, so list 3–5 weeks ahead of peak closing months).",
            sample_months,
            strongest.join(", ")
        );
            let series_json: Vec<Value> = month_rows.into_iter().map(|(_, _, j)| j).collect();
            (true, strongest, softest, series_json, Some(note))
        } else {
            let note = if sample_months > 0 {
                Some(format!(
                "Only {sample_months} monthly observations available (< 36-month minimum); seasonality curve omitted rather than synthesized."
            ))
            } else {
                None
            };
            (false, Vec::new(), Vec::new(), Vec::new(), note)
        };

    // 2. Portal Search-Bracket Cliff Analyzer
    let bracket_analysis = if let Some(price) = ctx.benchmark_price_usd {
        let p_f = price as f64;
        let mut cliff_match: Option<(i64, f64)> = None;
        for &b in PORTAL_SEARCH_BRACKETS_USD {
            if price > b {
                let pct_above = ((p_f - b as f64) / b as f64) * 100.0;
                if (0.1..=5.0).contains(&pct_above) {
                    cliff_match = Some((b, round_n(pct_above, 2)));
                    break;
                }
            }
        }

        let lower_bracket = PORTAL_SEARCH_BRACKETS_USD
            .iter()
            .copied()
            .rfind(|&b| b <= price);
        let upper_bracket = PORTAL_SEARCH_BRACKETS_USD
            .iter()
            .copied()
            .find(|&b| b > price);

        if let Some((b, pct_above)) = cliff_match {
            let delta_usd = price - b;
            json!({
                "benchmarkSalePriceUsd": price,
                "cliffFlagged": true,
                "nearestBracketUsd": b,
                "pctAboveBracket": pct_above,
                "dollarAboveBracketUsd": delta_usd,
                "recommendedBracketListPriceUsd": b,
                "lowerBracketUsd": lower_bracket,
                "upperBracketUsd": upper_bracket,
                "strategicPricingNote": format!(
                    "Benchmark value ({}) sits +{:.2}% ({}) above the major {} portal search filter cutoff. Listing at {} instead of {} unlocks dual-bracket exposure — surfacing your home to BOTH buyers capping saved alerts at '{}' AND buyers searching '{}+'.",
                    fmt_usd(p_f),
                    pct_above,
                    fmt_usd(delta_usd as f64),
                    fmt_usd(b as f64),
                    fmt_usd(b as f64),
                    fmt_usd(p_f),
                    fmt_usd(b as f64),
                    fmt_usd(b as f64),
                ),
            })
        } else {
            let note = match (lower_bracket, upper_bracket) {
                (Some(lb), Some(_ub)) if lb == price => format!(
                    "Benchmark value ({}) lands directly on the {} portal filter boundary, already capturing dual-bracket buyer searches (<= {} and {}+).",
                    fmt_usd(p_f),
                    fmt_usd(lb as f64),
                    fmt_usd(lb as f64),
                    fmt_usd(lb as f64),
                ),
                (Some(lb), Some(ub)) => format!(
                    "Benchmark value ({}) sits mid-channel between the {} and {} portal search brackets (outside the 0.1%–5.0% bracket-cliff zone); price below {} to stay inside the current search tier.",
                    fmt_usd(p_f),
                    fmt_usd(lb as f64),
                    fmt_usd(ub as f64),
                    fmt_usd(ub as f64),
                ),
                (Some(lb), None) => format!(
                    "Benchmark value ({}) sits above the top standard {} portal bracket (> +5.0% above cutoff).",
                    fmt_usd(p_f),
                    fmt_usd(lb as f64),
                ),
                (None, Some(ub)) => format!(
                    "Benchmark value ({}) sits below the entry {} portal search cutoff.",
                    fmt_usd(p_f),
                    fmt_usd(ub as f64),
                ),
                (None, None) => String::new(),
            };
            json!({
                "benchmarkSalePriceUsd": price,
                "cliffFlagged": false,
                "nearestBracketUsd": Value::Null,
                "pctAboveBracket": Value::Null,
                "dollarAboveBracketUsd": Value::Null,
                "recommendedBracketListPriceUsd": Value::Null,
                "lowerBracketUsd": lower_bracket,
                "upperBracketUsd": upper_bracket,
                "strategicPricingNote": note,
            })
        }
    } else {
        json!({
            "benchmarkSalePriceUsd": Value::Null,
            "cliffFlagged": false,
            "nearestBracketUsd": Value::Null,
            "pctAboveBracket": Value::Null,
            "dollarAboveBracketUsd": Value::Null,
            "recommendedBracketListPriceUsd": Value::Null,
            "lowerBracketUsd": Value::Null,
            "upperBracketUsd": Value::Null,
            "strategicPricingNote": Value::Null,
        })
    };

    json!({
        "seasonalityAvailable": seasonality_available,
        "seriesSource": if seasonality_available { json!(series_src) } else { Value::Null },
        "sampleMonthsCount": sample_months,
        "minimumMonthsRequired": 36,
        "strongestListingMonths": strongest_months,
        "softestMonths": softest_months,
        "monthlySeasonalityIndex": monthly_index,
        "seasonalityMethodologyNote": seasonality_note,
        "searchBracketCliff": bracket_analysis,
        "nearestBracketUsd": bracket_analysis.get("nearestBracketUsd").cloned().unwrap_or(Value::Null),
        "pctAboveBracket": bracket_analysis.get("pctAboveBracket").cloned().unwrap_or(Value::Null),
        "strategicPricingNote": bracket_analysis.get("strategicPricingNote").cloned().unwrap_or(Value::Null),
    })
}

// =================================================================================================
// Part 2.1 — `agentSuite.escrowKillerMatrix` (8-Gate Lender-Overlay & Deal-Breaker Triage)
// =================================================================================================

fn summarize_escrow_gates(gates: &[Value]) -> Value {
    let mut blocker = 0_i64;
    let mut watch = 0_i64;
    let mut verify = 0_i64;
    let mut clear = 0_i64;
    for g in gates {
        match val_str(g.get("status")).unwrap_or("verify") {
            "blocker" => blocker += 1,
            "watch" => watch += 1,
            "clear" => clear += 1,
            _ => verify += 1,
        }
    }
    json!({
        "totalGates": gates.len(),
        "blockerCount": blocker,
        "watchCount": watch,
        "verifyCount": verify,
        "clearCount": clear,
    })
}

/// Returns `true` only for actual roof-replacement permits, excluding Solar PV (`"Roof Mounted PV"`),
/// battery/inverter, skylight, or HVAC permits even if an upstream category was mistagged.
fn is_verified_reroof_permit(p: &Value) -> bool {
    let cat = val_str(p.get("category")).unwrap_or("");
    let raw_type = val_str(p.get("rawType").or_else(|| p.get("type"))).unwrap_or("");
    let desc = val_str(p.get("description")).unwrap_or("");
    let work_type = val_str(p.get("workType")).unwrap_or("");
    let blob = format!("{cat} {raw_type} {desc} {work_type}");
    if cat.eq_ignore_ascii_case("solar") || RE_NON_REROOF_PERMIT.is_match(&blob) {
        return false;
    }
    if p.get("isParentLot").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    cat.eq_ignore_ascii_case("reroof") || RE_REROOF_PERMIT.is_match(&blob)
}

fn build_escrow_killer_matrix(ctx: &SuiteContext, cure_before_escrow: &Value) -> Vec<Value> {
    let mut gates: Vec<Value> = Vec::with_capacity(8);

    // Gate 1: FHA 90/180-Day Anti-Flipping (24 CFR § 203.37a)
    let hold_days = val_i64(
        ctx.title_deed
            .pointer("/deedAnalysis/fhaAntiFlipping/sellerHoldDays"),
    )
    .or_else(|| {
        ctx.acquisition
            .date
            .map(|d| (ctx.as_of_date - d).num_days())
            .filter(|&days| days >= 0)
    });
    let markup_pct: Option<f64> = match (ctx.benchmark_price_usd, ctx.acquisition.price_usd) {
        (Some(cur_p), Some(acq_p)) if acq_p > 0 => {
            Some(round_n(((cur_p - acq_p) as f64 / acq_p as f64) * 100.0, 1))
        }
        _ => None,
    };
    let (fha_status, fha_evidence, fha_action) = match hold_days {
        Some(days) if days <= 90 => (
            "blocker",
            format!(
                "Seller acquired title {days} days ago (on {}). Under 24 CFR § 203.37a(b)(2), FHA-insured financing is strictly ineligible for contracts signed within 90 days of seller acquisition unless a statutory REO/inheritance/relocation exception applies.",
                ctx.acquisition.date_str.as_deref().unwrap_or("recent transfer")
            ),
            "Screen buyer pre-approvals for Conventional/VA/Cash only until Day 91 after deed recording, or verify a 24 CFR § 203.37a(c) exception with the DE underwriter upfront.".to_string(),
        ),
        Some(days) if (91..=180).contains(&days) => {
            let needs_2nd = markup_pct.map(|m| m >= 100.0).unwrap_or(false);
            (
                "watch",
                format!(
                    "Seller hold is {days} days (91–180 day FHA anti-flipping window{}). {}",
                    markup_pct
                        .map(|m| format!(", {:+.1}% vs. recorded purchase", m))
                        .unwrap_or_default(),
                    if needs_2nd {
                        "Markup >= 100% triggers mandatory 2nd independent FHA appraisal (buyer cannot be charged for 2nd appraisal)."
                    } else {
                        "FHA underwriters may still request rehab receipts/permits if resale price rises >= 100% over seller acquisition."
                    }
                ),
                "Prepare itemized contractor invoices, before/after photos, and building permits for the lender's appraisal packet; budget +7–10 days if an FHA offer is accepted.".to_string(),
            )
        }
        Some(days) => (
            "clear",
            format!(
                "Current ownership hold is {days} days (~{:.1} yrs since {}), well past the 90/180-day FHA anti-flipping window.",
                days as f64 / 365.25,
                ctx.acquisition.date_str.as_deref().unwrap_or("last sale")
            ),
            "No FHA anti-flipping seasoning restriction.".to_string(),
        ),
        None => (
            "verify",
            "No recorded seller acquisition date in public property records; FHA 90/180-day seasoning cannot be confirmed pre-Prelim.".to_string(),
            "Verify the exact deed recording date on Schedule A of the Preliminary Title Report before accepting an FHA-financed offer.".to_string(),
        ),
    };
    gates.push(json!({
        "id": "fha_anti_flipping",
        "category": "Federal Mortgage Seasoning",
        "title": "FHA 90 / 180-Day Anti-Flipping Rule",
        "status": fha_status,
        "daysSinceLastSale": hold_days,
        "evidence": fha_evidence,
        "agentAction": fha_action,
    }));

    // Gate 2: Loan Tier & Appraisal Overlay (Conforming vs High-Balance vs Jumbo)
    let price_for_loan = ctx
        .benchmark_price_usd
        .or_else(|| val_i64(ctx.facts.get("listPrice")).filter(|&p| p > 0));
    let has_geo_for_limits = !ctx.state.is_empty()
        || !ctx.county_fips.is_empty()
        || ctx.valuation.get("loanPrograms").is_some();
    let (lt_status, lt_evidence, lt_action) = match (price_for_loan, has_geo_for_limits) {
        (Some(price), true) => {
            let limits =
                resolve_loan_limits_from_report_parts(ctx.geo, ctx.facts, Some(ctx.valuation));
            let loan_80 = (price as f64 * 0.80).round() as i64;
            let loan_90 = (price as f64 * 0.90).round() as i64;
            let tier_80 = classify_loan_tier(loan_80, &limits);
            let tier_90 = classify_loan_tier(loan_90, &limits);
            if loan_80 > limits.conforming_limit {
                (
                    "watch",
                    format!(
                        "At 80% LTV ({}) and 90% LTV ({}), loan amounts exceed the county FHFA conforming ceiling ({}) -> {}. FHA ceiling is {}.",
                        fmt_usd(loan_80 as f64),
                        fmt_usd(loan_90 as f64),
                        fmt_usd(limits.conforming_limit as f64),
                        tier_80,
                        fmt_usd(limits.fha_limit as f64),
                    ),
                    format!(
                        "Cross-qualify financed offers for Jumbo reserve rules (typically 6–12+ mos post-close reserves, >=700–720 FICO, <=43% DTI, and 2nd appraisal above $2M–$3M loan amounts); buyers using conforming debt need >= {} down.",
                        fmt_usd((price - limits.conforming_limit).max(0) as f64)
                    ),
                )
            } else if loan_80 > limits.baseline_limit || loan_90 > limits.conforming_limit {
                (
                    "watch",
                    format!(
                        "80% LTV loan ({}) is {} and 90% LTV loan ({}) is {} (baseline limit {}, county ceiling {}).",
                        fmt_usd(loan_80 as f64),
                        tier_80,
                        fmt_usd(loan_90 as f64),
                        tier_90,
                        fmt_usd(limits.baseline_limit as f64),
                        fmt_usd(limits.conforming_limit as f64),
                    ),
                    "Expect FHFA High-Balance LLPAs (+0.50 to +1.00 pts) and verify low-down-payment offers (<20% down) do not accidentally cross above the county conforming cap.".to_string(),
                )
            } else {
                (
                    "clear",
                    format!(
                        "Both 80% LTV ({}) and 90% LTV ({}) fit cleanly inside the standard FHFA baseline conforming limit ({}) and county FHA limit ({}).",
                        fmt_usd(loan_80 as f64),
                        fmt_usd(loan_90 as f64),
                        fmt_usd(limits.baseline_limit as f64),
                        fmt_usd(limits.fha_limit as f64),
                    ),
                    "Eligible for standard Fannie/Freddie Automated Underwriting (DU/LP), PIW appraisal waivers where qualified, and FHA/VA programs.".to_string(),
                )
            }
        }
        _ => (
            "verify",
            "Property benchmark value or county FIPS not supplied; conforming vs. Jumbo loan tier not yet classified.".to_string(),
            "Verify purchase price against 2026 county FHFA conforming and FHA loan limits once valuation/list price is set.".to_string(),
        ),
    };
    gates.push(json!({
        "id": "loan_tier_appraisal_overlay",
        "category": "Mortgage Underwriting",
        "title": "Loan Tier & Appraisal Overlay (Conforming / High-Balance / Jumbo)",
        "status": lt_status,
        "evidence": lt_evidence,
        "agentAction": lt_action,
    }));

    // Gate 3: Condo / PUD Warrantability (Fannie/Freddie Project Eligibility)
    let raw_pt = val_str(ctx.facts.get("propertyType")).unwrap_or("");
    let pt_lc = raw_pt.to_ascii_lowercase();
    let is_condo_coop =
        pt_lc.contains("condo") || pt_lc.contains("co-op") || pt_lc.contains("coop");
    let is_townhome_or_pud = pt_lc.contains("town") || pt_lc.contains("pud");
    let hoa_mo = val_f64(ctx.facts.get("hoaMonthly"));
    let hoa_litigation = ctx
        .hoa_profile
        .pointer("/fundingAndGovernance/boardGovernance/litigationWatch")
        .and_then(Value::as_bool)
        .or_else(|| {
            ctx.facts
                .pointer("/hoa/hasLitigation")
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    let reserve_alloc = val_f64(
        ctx.hoa_profile
            .pointer("/fundingAndGovernance/reserveStatus/reserveAllocationPct")
            .or_else(|| ctx.facts.pointer("/hoa/reserveAllocationPct")),
    );
    let delinq_pct = val_f64(
        ctx.hoa_profile
            .pointer("/fundingAndGovernance/reserveStatus/delinquencyRatePct")
            .or_else(|| ctx.facts.pointer("/hoa/delinquencyRatePct")),
    );
    let has_special_assess = ctx
        .hoa_profile
        .get("specialAssessments")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false)
        || ctx
            .facts
            .pointer("/hoa/specialAssessments")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);

    let (cw_status, cw_evidence, cw_action) = if hoa_litigation
        || (is_condo_coop && reserve_alloc.map(|r| r < 10.0).unwrap_or(false))
        || delinq_pct.map(|d| d > 15.0).unwrap_or(false)
    {
        (
            "blocker",
            format!(
                "Fannie/Freddie non-warrantable project trigger detected (litigation: {}, reserve budget share: {}, 60d delinquency: {}).",
                if hoa_litigation { "Yes" } else { "No" },
                reserve_alloc
                    .map(|r| format!("{r:.1}% (<10% min)"))
                    .unwrap_or_else(|| "unverified".into()),
                delinq_pct
                    .map(|d| format!("{d:.1}% (>15% max)"))
                    .unwrap_or_else(|| "unverified".into()),
            ),
            "Conventional agency financing is likely blocked (Fannie Mae Condo Project Manager unavailable); line up a Non-QM / portfolio condo lender or target cash buyers upfront.".to_string(),
        )
    } else if is_condo_coop || (is_townhome_or_pud && has_special_assess) {
        (
            "verify",
            format!(
                "Attached {} project{}: Fannie/Freddie Full Review requires >=10% annual budget reserve allocation, <=15% 60-day dues delinquency, no structural defect litigation, and SB-326 / SIRS compliance.",
                if raw_pt.is_empty() { "condo/townhome" } else { raw_pt },
                if has_special_assess { " with special-assessment signal" } else { "" }
            ),
            "Pre-order Form 1076 Condo Project Questionnaire, current HOA Budget, Reserve Study, Master Insurance EOI, and FHA/VA condo approval check before accepting low-down-payment offers.".to_string(),
        )
    } else if !raw_pt.is_empty()
        && (pt_lc.contains("single") || pt_lc.contains("detached") || pt_lc.contains("sfr"))
        && hoa_mo.unwrap_or(0.0) <= 0.0
    {
        (
            "clear",
            format!(
                "Detached fee-simple property (`{raw_pt}`) with $0 mandatory HOA dues reported — no condo/PUD project warrantability overlay."
            ),
            "Confirm Schedule B of Prelim shows no unrecorded master-association covenants.".to_string(),
        )
    } else if hoa_mo.map(|m| m > 0.0).unwrap_or(false) || is_townhome_or_pud {
        (
            "verify",
            format!(
                "HOA / PUD governance detected ({} monthly dues); verify PUD vs. condominium legal description on deed.",
                hoa_mo
                    .map(fmt_usd)
                    .unwrap_or_else(|| "unreported".to_string())
            ),
            "Check legal description: fee-simple PUD lots bypass full Fannie condo review, whereas airspace condo-mapped townhomes require full Form 1076 project underwriting.".to_string(),
        )
    } else {
        (
            "verify",
            "Property type / HOA status not supplied in public records; confirm fee-simple vs. condo/PUD legal regime.".to_string(),
            "Verify parcel legal description and HOA status on Preliminary Title Report.".to_string(),
        )
    };
    gates.push(json!({
        "id": "condo_pud_warrantability",
        "category": "HOA & Project Eligibility",
        "title": "Condo / PUD Warrantability (Fannie / Freddie B4-2.1)",
        "status": cw_status,
        "evidence": cw_evidence,
        "agentAction": cw_action,
    }));

    // Gate 4: Mello-Roos / Special Assessment DTI Impact
    let std_block = ctx
        .buyer_diligence
        .pointer("/ongoingCosts/specialTaxDistricts");
    let confirmed_special = val_i64(std_block.and_then(|b| b.get("estimatedAnnualSpecialTaxUsd")))
        .filter(|&x| x > 0)
        .or_else(|| val_i64(ctx.facts.get("melloRoosAnnual")).filter(|&x| x > 0))
        .or_else(|| val_i64(ctx.facts.get("mudTaxAnnual")).filter(|&x| x > 0))
        .or_else(|| val_i64(ctx.facts.get("cddAnnual")).filter(|&x| x > 0))
        .or_else(|| val_i64(ctx.facts.get("directChargesAnnual")).filter(|&x| x > 0));
    let heuristic_special_flag = std_block
        .and_then(|b| b.get("hasConfirmedOrSuspectedDistrict"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            let lc = ctx.combined_text.to_ascii_lowercase();
            lc.contains("mello-roos")
                || lc.contains("mello roos")
                || (ctx.state == "CA"
                    && val_f64(ctx.facts.get("taxRate"))
                        .map(|r| r > 0.0130)
                        .unwrap_or(false))
                || (ctx.state == "TX"
                    && val_f64(ctx.facts.get("taxRate"))
                        .map(|r| r >= 0.0235)
                        .unwrap_or(false))
        });

    let (mr_status, mr_evidence, mr_action) = if let Some(ann_sp) = confirmed_special {
        let mo_sp = (ann_sp as f64 / 12.0).round() as i64;
        (
            "watch",
            format!(
                "Confirmed non-ad-valorem special district / direct charge of {}/yr ({}/mo) on parcel record; lenders add 100% of monthly CFD/MUD/CDD levies into borrower PITIA DTI.",
                fmt_usd(ann_sp as f64),
                fmt_usd(mo_sp as f64)
            ),
            "Provide the exact County Tax Collector direct-charge breakdown to buyer loan officers before pre-approval sign-off so DTI does not fail in underwriting.".to_string(),
        )
    } else if heuristic_special_flag {
        (
            "verify",
            "Parcel tax rate, vintage, or remarks indicate potential Mello-Roos CFD / TX MUD / FL CDD special district exposure (exact annual levy not yet itemized).".to_string(),
            "Pull the current itemized county property-tax bill and NHD Tax Addendum to confirm exact special-tax bond lines for lender DTI calculation.".to_string(),
        )
    } else if !ctx.state.is_empty() && val_f64(ctx.facts.get("taxRate")).is_some() {
        let tr_pct = round_n(val_f64(ctx.facts.get("taxRate")).unwrap_or(0.0) * 100.0, 3);
        (
            "clear",
            format!(
                "Parcel statutory tax rate ({tr_pct}%) shows no Mello-Roos CFD, MUD, or CDD bond flag."
            ),
            "Attach copy of current county tax bill to disclosure packet as routine verification.".to_string(),
        )
    } else {
        (
            "verify",
            "Parcel-level direct charges / special tax roll not supplied; confirm whether non-ad-valorem parcel taxes apply.".to_string(),
            "Verify itemized county tax bill for any Mello-Roos, MUD/PID, CDD, or municipal parcel assessments.".to_string(),
        )
    };
    gates.push(json!({
        "id": "mello_roos_special_tax_dti",
        "category": "Tax & DTI Qualification",
        "title": "Mello-Roos / MUD / CDD Special Assessment DTI Impact",
        "status": mr_status,
        "evidence": mr_evidence,
        "agentAction": mr_action,
    }));

    // Gate 5: Property Insurability / FAIR Plan Contingency
    let ca_fhsz_str = val_str(ctx.hazards.pointer("/wildfire/california/fhsz")).unwrap_or("");
    let ca_in_fhsz = ctx
        .hazards
        .pointer("/wildfire/california/inFhsz")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let whp_label = val_str(ctx.hazards.pointer("/wildfire/whp/label")).unwrap_or("");
    let high_wildfire = ca_in_fhsz
        || ca_fhsz_str.to_ascii_lowercase().contains("very high")
        || whp_label.to_ascii_lowercase().contains("high");
    let roof_raw = val_str(ctx.facts.get("roof")).unwrap_or("");
    let is_wood_shake = roof_raw.to_ascii_lowercase().contains("shake")
        || roof_raw.to_ascii_lowercase().contains("wood");
    let year_built = val_i64(ctx.facts.get("yearBuilt")).filter(|&y| y > 1700);
    let current_yr = ctx.as_of_date.year() as i64;
    let explicit_reroof_from_condition = ctx
        .condition
        .pointer("/systems/roof/reroofVerified")
        .and_then(Value::as_bool)
        .or_else(|| {
            ctx.condition
                .pointer("/roofReplacement/reroofVerified")
                .and_then(Value::as_bool)
        });
    let has_reroof_permit = explicit_reroof_from_condition.unwrap_or_else(|| {
        collect_deduped_permits(ctx)
            .iter()
            .any(is_verified_reroof_permit)
    });
    let explicit_roof_age = val_i64(ctx.facts.get("roofAgeYears"))
        .or_else(|| val_i64(ctx.condition.pointer("/systems/roof/estimatedAgeYears")));
    let aging_unverified_roof = explicit_roof_age.map(|a| a >= 20).unwrap_or(false)
        || (!has_reroof_permit && year_built.map(|yb| current_yr - yb >= 22).unwrap_or(false));
    let fair_plan_likely = ctx
        .insurance
        .get("fairPlanLikely")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (ins_status, ins_evidence, ins_action) = if is_wood_shake
        || (high_wildfire && fair_plan_likely)
    {
        (
            "blocker",
            format!(
                "Severe carrier declination risk ({}{}): most admitted HO-3 carriers refuse binding without Class-A roof / California FAIR Plan + DIC wrap.",
                if is_wood_shake { "wood-shake roof " } else { "" },
                if high_wildfire { "High/Very High wildfire zone" } else { "" }
            ),
            "Quote California FAIR Plan + Difference-in-Conditions (DIC) or surplus-lines HO-3 BEFORE listing/contingency removal so lender impound DTI and binding timelines do not kill escrow.".to_string(),
        )
    } else if high_wildfire || aging_unverified_roof {
        (
            "watch",
            format!(
                "{}{}",
                if high_wildfire {
                    format!(
                        "Elevated wildfire hazard zone ({}); ",
                        if !ca_fhsz_str.is_empty() { ca_fhsz_str } else { whp_label }
                    )
                } else {
                    String::new()
                },
                if aging_unverified_roof {
                    format!(
                        "structure built in {} with {}no recent reroof permit on file — admitted insurers routinely require a 3–5 yr roof-life certification or 4-point inspection on roofs >20 yrs.",
                        year_built.unwrap_or(1975),
                        if roof_raw.is_empty() {
                            String::new()
                        } else {
                            format!("{roof_raw}, ")
                        }
                    )
                } else {
                    "verify admitted HO-3 carrier appetite and brush-clearance compliance.".to_string()
                }
            ),
            "Require buyer insurance binder check within Days 1–5 of escrow and provide roof inspection certification + 5-year CLUE loss history upfront.".to_string(),
        )
    } else if ctx.hazards.get("wildfire").is_some() && year_built.is_some() {
        (
            "clear",
            format!(
                "Outside wildfire FHSZ ({}) with documented post-2005 construction or verified reroof permit.",
                if !ca_fhsz_str.is_empty() { ca_fhsz_str } else { "Non-FHSZ" }
            ),
            "Confirm standard HO-3 replacement-cost binder prior to loan contingency removal.".to_string(),
        )
    } else {
        (
            "verify",
            "Wildfire hazard layer or roof age/permit history not supplied; carrier binding eligibility unverified.".to_string(),
            "Verify wildfire zone, FEMA flood zone, roof age, and pre-1980 electrical panel brand (no Zinsco / Federal Pacific) early in escrow.".to_string(),
        )
    };
    gates.push(json!({
        "id": "insurability_fair_plan",
        "category": "Hazard & Insurance",
        "title": "Property Insurability & FAIR Plan / Roof-Age Contingency",
        "status": ins_status,
        "evidence": ins_evidence,
        "agentAction": ins_action,
    }));

    // Gate 6: Permit & Square-Footage Discrepancy
    let sqft = val_i64(ctx.facts.get("sqft")).filter(|&s| s > 0);
    let gross_sqft = val_i64(ctx.facts.get("grossSqft")).filter(|&s| s > 0);
    let reg_sqft = val_i64(ctx.facts.pointer("/buildingRegister/livingSqft")).filter(|&s| s > 0);
    let sqft_note = val_str(ctx.facts.get("sqftNote")).unwrap_or("");
    let open_permits_n =
        val_i64(cure_before_escrow.get("openOrUnfinaledPermitsCount")).unwrap_or(0);
    let ce_open_n = val_i64(cure_before_escrow.get("codeEnforcementOpenCount")).unwrap_or(0);
    let sqft_gap = match (sqft, gross_sqft) {
        (Some(s), Some(g)) if (g - s).abs() >= 80 => Some(g - s),
        _ => match (sqft, reg_sqft) {
            (Some(s), Some(r)) if (s - r).abs() >= 80 => Some(s - r),
            _ => None,
        },
    };
    let note_flags_conflict = RE_SQFT_CONFLICT.is_match(sqft_note);

    let (ps_status, ps_evidence, ps_action) = if ce_open_n > 0 {
        (
            "blocker",
            format!(
                "{ce_open_n} open municipal code-enforcement case(s) reported on parcel; lenders block funding on unresolved habitability/building-code notices of violation."
            ),
            "Obtain city code-enforcement clearance or lender-approved escrow holdback agreement prior to appraisal.".to_string(),
        )
    } else if sqft_gap.is_some() || note_flags_conflict || open_permits_n > 0 {
        let mut parts = Vec::new();
        if let (Some(s), Some(g), Some(gap)) = (sqft, gross_sqft, sqft_gap) {
            parts.push(format!(
                "Main GLA ({} sq ft) differs from gross/combined area ({} sq ft, {:+} sq ft delta)",
                s, g, gap
            ));
        }
        if !sqft_note.is_empty() {
            parts.push(sqft_note.chars().take(180).collect::<String>());
        }
        if open_permits_n > 0 {
            parts.push(format!(
                "{open_permits_n} unfinaled/under-review permit(s) on file"
            ));
        }
        (
            "watch",
            parts.join(" · "),
            "Hand the appraiser city permit cards and separate ANSI Z765 sketches (main house GLA vs. permitted ADU vs. sunroom/bonus enclosure) at walkthrough so Fannie/Freddie GLA rules do not cause a short appraisal.".to_string(),
        )
    } else if sqft.is_some() {
        (
            "clear",
            format!(
                "Living area ({} sq ft) aligns across parcel records with 0 open permits and 0 open code-enforcement cases.",
                sqft.unwrap_or(0)
            ),
            "Include county assessor building record in MLS disclosures.".to_string(),
        )
    } else {
        (
            "verify",
            "Assessor vs. MLS living square footage and municipal permit finalization status not supplied.".to_string(),
            "Cross-check tax-roll GLA against MLS measurement and city building permit archive before pricing.".to_string(),
        )
    };
    gates.push(json!({
        "id": "permit_sqft_discrepancy",
        "category": "Appraisal & Permits",
        "title": "Permit Finalization & Assessor vs. MLS Sq-Ft Discrepancy",
        "status": ps_status,
        "evidence": ps_evidence,
        "agentAction": ps_action,
    }));

    // Gate 7: Solar UCC-1 / PACE Subordination
    let has_pace = RE_PACE_LIEN.is_match(&ctx.combined_text);
    let has_solar_lease = RE_SOLAR_LEASE_UCC.is_match(&ctx.combined_text);
    let has_any_solar = RE_SOLAR_ANY.is_match(&ctx.combined_text)
        || collect_deduped_permits(ctx).iter().any(|p| {
            let b = format!(
                "{} {}",
                val_str(p.get("type").or_else(|| p.get("category"))).unwrap_or(""),
                val_str(p.get("description")).unwrap_or("")
            )
            .to_ascii_lowercase();
            b.contains("solar") || b.contains("photovoltaic")
        });

    let (sol_status, sol_evidence, sol_action) = if has_pace {
        (
            "blocker",
            "PACE / HERO clean-energy tax-roll assessment detected; Fannie Mae & Freddie Mac strictly prohibit super-priority PACE liens from remaining senior to a new first mortgage.".to_string(),
            "Order an immediate PACE payoff statement and budget full payoff from seller proceeds at/before closing.".to_string(),
        )
    } else if has_solar_lease {
        (
            "watch",
            "Leased solar / Power Purchase Agreement (PPA) / solar UCC-1 financing signal detected in property records.".to_string(),
            "Initiate solar provider lease-transfer credit approval and temporary UCC-1 fixture release on Day 1 of escrow (solar transfer desks routinely take 10–21 business days) and include monthly PPA payment in buyer DTI.".to_string(),
        )
    } else if has_any_solar {
        (
            "verify",
            "Solar PV installation detected; ownership vs. PPA/loan UCC-1 status must be confirmed on Preliminary Title Report.".to_string(),
            "Obtain paid-in-full solar invoice / NEM interconnection agreement and verify zero active solar UCC-1 fixture filing on Prelim Schedule B.".to_string(),
        )
    } else if ctx
        .facts
        .as_object()
        .map(|o| !o.is_empty())
        .unwrap_or(false)
    {
        (
            "clear",
            "No PACE/HERO lien or solar lease/UCC-1 signal detected in parcel or permit records."
                .to_string(),
            "Routine check of Schedule B on Prelim for any unindexed UCC-1 fixture filings."
                .to_string(),
        )
    } else {
        (
            "verify",
            "Property features and lien records not supplied; solar UCC-1 / PACE status unverified.".to_string(),
            "Confirm whether the home has solar panels (owned vs. leased/PPA) or PACE tax assessments.".to_string(),
        )
    };
    gates.push(json!({
        "id": "solar_ucc1_pace",
        "category": "Title & Subordination",
        "title": "Solar Lease / UCC-1 & PACE Assessment Subordination",
        "status": sol_status,
        "evidence": sol_evidence,
        "agentAction": sol_action,
    }));

    // Gate 8: Vesting, Trust & FIRPTA Escrow Readiness
    let fc_status = val_str(ctx.facts.pointer("/foreclosure/status")).unwrap_or("");
    let has_nod_nts = matches!(
        fc_status,
        "pre_foreclosure_nod" | "notice_of_default" | "notice_of_trustee_sale" | "auction"
    );
    let has_probate = RE_INHERITED_OR_ESTATE.is_match(&ctx.combined_text);
    let has_trust_or_entity = RE_TRUST_OR_ENTITY_VESTING.is_match(&ctx.combined_text);

    let (vt_status, vt_evidence, vt_action) = if has_nod_nts {
        (
            "blocker",
            format!("Active distress / pre-foreclosure status (`{fc_status}`) reported on parcel."),
            "Verify lender reinstatement/payoff demand vs. net proceeds immediately; if upside-down, shift to formal short-sale package and trustee-sale postponement request.".to_string(),
        )
    } else if has_probate {
        (
            "watch",
            "Estate / Probate / Conservatorship signal detected in chain-of-title or remarks.".to_string(),
            "Confirm court-issued Letters Testamentary / Administration with full IAEA authority (vs. limited authority requiring court overbid confirmation) before opening escrow.".to_string(),
        )
    } else if has_trust_or_entity {
        (
            "watch",
            "Trust or corporate/LLC vesting signal detected in deed/title records.".to_string(),
            "Have seller complete Certification of Trust (or LLC Operating Agreement + signing resolution) and verify a bank account titled in the exact Trust/LLC name on Day 1 so closing proceeds are not frozen.".to_string(),
        )
    } else {
        (
            "verify",
            "Standard escrow vesting & tax-withholding verification required upon opening title.".to_string(),
            "Confirm all vested owners/spouses sign listing & grant deed, and collect federal FIRPTA (IRC § 1445, 15% foreign-seller withholding check) + state withholding certificates (e.g. CA Form 593) early in escrow.".to_string(),
        )
    };
    gates.push(json!({
        "id": "vesting_trust_firpta",
        "category": "Escrow & Authority",
        "title": "Vesting Authority, Trust / Probate & FIRPTA Readiness",
        "status": vt_status,
        "evidence": vt_evidence,
        "agentAction": vt_action,
    }));

    gates
}

// =================================================================================================
// Part 2.2 — `agentSuite.cmaWorkbench` (Interactive Line-Item CMA Adjustment Workbench)
// =================================================================================================

fn build_cma_workbench(ctx: &SuiteContext) -> Value {
    let raw_comps = ctx
        .valuation
        .get("comps")
        .or_else(|| ctx.valuation.pointer("/compsAnalysis/comps"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut rows: Vec<Value> = Vec::new();
    let mut comp_ppsf_vals: Vec<f64> = Vec::new();
    let mut comp_raw_vals: Vec<f64> = Vec::new();
    let mut comp_adj_vals: Vec<f64> = Vec::new();
    let mut weighted_sum = 0.0_f64;
    let mut weight_total = 0.0_f64;

    for (idx, c) in raw_comps.iter().enumerate() {
        if is_synthetic_comp_value(c) {
            continue;
        }
        let raw_price = val_i64(c.get("price").or_else(|| c.get("soldPrice"))).filter(|&p| p > 0);
        let adj_price = val_i64(c.get("adjustedPrice").or_else(|| c.get("adjusted")))
            .filter(|&p| p > 0)
            .or(raw_price);
        let sqft = val_i64(c.get("sqft")).filter(|&s| s > 0);
        let ppsf = val_i64(c.get("pricePerSqft"))
            .filter(|&p| p > 0)
            .or_else(|| match (raw_price, sqft) {
                (Some(rp), Some(sq)) if sq > 0 => Some((rp as f64 / sq as f64).round() as i64),
                _ => None,
            });
        let t_pct = val_f64(
            c.get("timeAdjPct")
                .or_else(|| c.pointer("/adjustments/timePct")),
        )
        .unwrap_or(0.0);
        let s_pct = val_f64(
            c.get("sizeAdjPct")
                .or_else(|| c.pointer("/adjustments/sizePct")),
        )
        .unwrap_or(0.0);
        let l_pct = val_f64(
            c.get("lotAdjPct")
                .or_else(|| c.pointer("/adjustments/lotPct")),
        )
        .unwrap_or(0.0);
        let bath_pct = val_f64(
            c.get("bathAdjPct")
                .or_else(|| c.pointer("/adjustments/bathPct")),
        )
        .unwrap_or(0.0);
        let bed_pct = val_f64(
            c.get("bedAdjPct")
                .or_else(|| c.pointer("/adjustments/bedPct")),
        )
        .unwrap_or(0.0);
        let age_pct = val_f64(
            c.get("ageAdjPct")
                .or_else(|| c.pointer("/adjustments/agePct")),
        )
        .unwrap_or(0.0);

        let time_adj_price = val_i64(c.get("timeAdjPrice"))
            .filter(|&p| p > 0)
            .or_else(|| raw_price.map(|rp| (rp as f64 * (1.0 + t_pct / 100.0)).round() as i64));

        // Decompose feature adjustments sequentially from `timeAdjPrice` -> `adjustedPrice`
        // so `timeAdjPrice + sqftDeltaUsd + bedBathDeltaUsd + ageCondDeltaUsd + lotDeltaUsd == adjustedPrice`
        let (sqft_delta, bed_bath_delta, age_cond_delta, lot_delta) =
            match (time_adj_price, adj_price) {
                (Some(tp), Some(ap)) => {
                    let tp_f = tp as f64;
                    let bb_mult = (1.0 + bath_pct / 100.0) * (1.0 + bed_pct / 100.0);
                    let after_sq = tp_f * (1.0 + s_pct / 100.0);
                    let after_bb = after_sq * bb_mult;
                    let after_age = after_bb * (1.0 + age_pct / 100.0);
                    let after_lot = after_age * (1.0 + l_pct / 100.0);

                    let mut d_sq = (after_sq - tp_f).round() as i64;
                    let mut d_bb = (after_bb - after_sq).round() as i64;
                    let mut d_age = (after_age - after_bb).round() as i64;
                    let mut d_lot = (after_lot - after_age).round() as i64;

                    let target_feat_delta = ap - tp;
                    let raw_sum = d_sq + d_bb + d_age + d_lot;
                    let residual = target_feat_delta - raw_sum;
                    if residual != 0 {
                        // Attribute small percentage-rounding residual to the largest active adjustment bucket
                        let abs_arr = [d_sq.abs(), d_bb.abs(), d_age.abs(), d_lot.abs()];
                        let max_idx = abs_arr
                            .iter()
                            .enumerate()
                            .max_by_key(|(_, &v)| v)
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        match max_idx {
                            0 => d_sq += residual,
                            1 => d_bb += residual,
                            2 => d_age += residual,
                            _ => d_lot += residual,
                        }
                    }
                    (Some(d_sq), Some(d_bb), Some(d_age), Some(d_lot))
                }
                _ => (None, None, None, None),
            };

        let dist_mi =
            val_f64(c.get("distanceMiles").or_else(|| c.get("distanceMi"))).map(|d| round_n(d, 2));
        let weight = val_f64(c.get("weight")).map(|w| round_n(w, 4));
        let sale_date = val_str(
            c.get("saleDate")
                .or_else(|| c.get("soldDate"))
                .or_else(|| c.get("date")),
        );

        if let Some(p) = ppsf {
            comp_ppsf_vals.push(p as f64);
        }
        if let Some(rp) = raw_price {
            comp_raw_vals.push(rp as f64);
        }
        if let Some(ap) = adj_price {
            comp_adj_vals.push(ap as f64);
            let w = weight.unwrap_or(1.0).max(0.0);
            weighted_sum += ap as f64 * w;
            weight_total += w;
        }

        rows.push(json!({
            "id": format!("comp-{}", idx + 1),
            "address": val_str(c.get("address")),
            "mlsNumber": val_str(c.get("mlsNumber")),
            "saleDate": sale_date,
            "rawPrice": raw_price,
            "listPrice": val_i64(c.get("listPrice")).filter(|&p| p > 0),
            "saleToListPct": val_f64(c.get("saleToListPct")),
            "timeAdjPrice": time_adj_price,
            "timeAdjPct": t_pct,
            "adjustedPrice": adj_price,
            "sqft": sqft,
            "pricePerSqft": ppsf,
            "beds": val_f64(c.get("beds")),
            "baths": val_f64(c.get("baths")),
            "lotSqft": val_i64(c.get("lotSqft")).filter(|&l| l > 0),
            "yearBuilt": val_i64(c.get("yearBuilt")).filter(|&y| y > 1700),
            "distanceMiles": dist_mi,
            "weight": weight,
            "sqftDeltaUsd": sqft_delta,
            "bedBathDeltaUsd": bed_bath_delta,
            "ageCondDeltaUsd": age_cond_delta,
            "lotDeltaUsd": lot_delta,
            "outlierFlag": c.get("outlierFlag").and_then(Value::as_bool).unwrap_or(false),
            "vsSubject": val_str(c.get("vsSubject")),
            "note": val_str(c.get("note")),
            "includedByDefault": true,
        }));
    }

    let subj_sqft = val_i64(ctx.facts.get("sqft")).filter(|&s| s > 0);
    let subj_beds = val_i64(ctx.facts.get("beds")).filter(|&b| b >= 0);
    let subj_baths = val_f64(ctx.facts.get("baths")).filter(|&b| b > 0.0);
    let subj_lot = val_i64(ctx.facts.get("lotSqft")).filter(|&l| l > 0);
    let subj_yb = val_i64(ctx.facts.get("yearBuilt")).filter(|&y| y > 1700);

    let ref_ppsf = ctx
        .per_sqft_usd
        .map(|p| p as f64)
        .or_else(|| median_f64(&comp_ppsf_vals));
    // Marginal GLA adjustment rate = ~60% of average $/sq ft (reflecting land-value separation & power-law elasticity)
    let marginal_sqft_value_usd = ref_ppsf.map(|ppsf| ((ppsf * 0.60) / 5.0).round() as i64 * 5);

    let weighted_adj_est =
        (weight_total > 0.0).then(|| (weighted_sum / weight_total).round() as i64);
    let med_adj = median_f64(&comp_adj_vals).map(|v| v.round() as i64);
    let med_raw = median_f64(&comp_raw_vals).map(|v| v.round() as i64);
    let med_ppsf = median_f64(&comp_ppsf_vals).map(|v| v.round() as i64);

    json!({
        "compCount": rows.len(),
        "subjectSpecs": {
            "sqft": subj_sqft,
            "grossSqft": val_i64(ctx.facts.get("grossSqft")).filter(|&s| s > 0),
            "beds": subj_beds,
            "baths": subj_baths,
            "lotSqft": subj_lot,
            "yearBuilt": subj_yb,
            "marginalSqftValueUsd": marginal_sqft_value_usd,
        },
        "summary": {
            "weightedAdjustedPriceUsd": weighted_adj_est,
            "medianAdjustedPriceUsd": med_adj,
            "medianRawPriceUsd": med_raw,
            "medianPricePerSqftUsd": med_ppsf,
        },
        "comps": rows,
    })
}

// =================================================================================================
// Part 2.3 — `agentSuite.listingLeadSignals` (Zero-PII Tenure, Equity & Prop-13 Lock-In)
// =================================================================================================

fn build_listing_lead_signals(ctx: &SuiteContext) -> Value {
    let tenure_years = ctx.acquisition.date.map(|acq_dt| {
        let days = (ctx.as_of_date - acq_dt).num_days().max(0);
        round_n(days as f64 / 365.25, 2)
    });
    let last_sale_price = ctx.acquisition.price_usd;
    let last_sale_date = ctx.acquisition.date_str.clone();
    let est_val = ctx.benchmark_price_usd;

    let gross_equity_gain_usd = match (est_val, last_sale_price) {
        (Some(ev), Some(lp)) => Some(ev - lp),
        _ => None,
    };
    let gross_equity_gain_pct = match (est_val, last_sale_price) {
        (Some(ev), Some(lp)) if lp > 0 => Some(round_n(((ev - lp) as f64 / lp as f64) * 100.0, 1)),
        _ => None,
    };

    let assessed_to_market_pct = match (ctx.current_assessed_usd, est_val) {
        (Some(av), Some(ev)) if ev > 0 => Some(round_n((av as f64 / ev as f64) * 100.0, 1)),
        _ => None,
    };
    let tax_lock_in_strength = assessed_to_market_pct.map(|ratio| {
        if ratio < 55.0 {
            "strong_tax_lock_in"
        } else if ratio < 85.0 {
            "moderate_tax_lock_in"
        } else if ratio <= 103.0 {
            "assessed_near_market"
        } else {
            "assessed_above_market_prop8_candidate"
        }
    });

    // Extract prior listing & sale cadence from `facts.saleHistory` and `facts.pastListings`
    let mut raw_events: Vec<(String, String, Option<i64>, Option<String>)> = Vec::new();
    let mut seen_ev: HashSet<String> = HashSet::new();
    let mut push_ev = |dt: Option<&str>, ev: Option<&str>, pr: Option<i64>, note: Option<&str>| {
        let d_str = dt.unwrap_or("").trim();
        let e_str = ev.unwrap_or("").trim();
        if d_str.is_empty() && e_str.is_empty() {
            return;
        }
        let key = format!(
            "{}:{}:{}",
            d_str,
            e_str.to_ascii_lowercase(),
            pr.unwrap_or(-1)
        );
        if seen_ev.insert(key) {
            raw_events.push((
                d_str.to_string(),
                if e_str.is_empty() {
                    "Event".to_string()
                } else {
                    e_str.to_string()
                },
                pr.filter(|&p| p > 0),
                note.map(String::from),
            ));
        }
    };

    if let Some(arr) = ctx.facts.get("saleHistory").and_then(Value::as_array) {
        for item in arr {
            push_ev(
                val_str(item.get("date")),
                val_str(item.get("event")),
                val_i64(item.get("price")),
                val_str(item.get("note")),
            );
        }
    }
    for ptr in ["/pastListings", "/disclosures/pastListings"] {
        if let Some(arr) = ctx.facts.pointer(ptr).and_then(Value::as_array) {
            for item in arr {
                push_ev(
                    val_str(item.get("date").or_else(|| item.get("listDate"))),
                    val_str(item.get("event").or_else(|| item.get("status"))),
                    val_i64(item.get("price").or_else(|| item.get("listPrice"))),
                    val_str(item.get("note").or_else(|| item.get("remarks"))),
                );
            }
        }
    }

    raw_events.sort_by(|a, b| a.0.cmp(&b.0));

    let mut price_cut_count = 0_i64;
    let mut withdrawn_or_expired_count = 0_i64;
    let mut prev_active_list_price: Option<i64> = None;
    let mut prior_listing_events: Vec<Value> = Vec::with_capacity(raw_events.len());

    for (dt, ev, pr, note) in raw_events {
        let ev_lc = ev.to_ascii_lowercase();
        let is_cut = ev_lc.contains("cut") || ev_lc.contains("reduc") || ev_lc.contains("drop");
        let is_withdrawn = ev_lc.contains("withdrawn")
            || ev_lc.contains("expired")
            || ev_lc.contains("cancel")
            || ev_lc.contains("delist");
        let is_list = ev_lc.contains("list") && !is_withdrawn;
        let is_sold = ev_lc.contains("sold") || ev_lc.contains("close");

        if is_cut {
            price_cut_count += 1;
        }
        if is_withdrawn {
            withdrawn_or_expired_count += 1;
        }

        let delta_pct = match (pr, prev_active_list_price) {
            (Some(cur_p), Some(prev_lp)) if (is_cut || is_sold) && prev_lp > 0 => Some(round_n(
                ((cur_p - prev_lp) as f64 / prev_lp as f64) * 100.0,
                1,
            )),
            _ => None,
        };

        if is_list || is_cut {
            if pr.is_some() {
                prev_active_list_price = pr;
            }
        } else if is_sold || is_withdrawn {
            prev_active_list_price = None;
        }

        prior_listing_events.push(json!({
            "date": if dt.is_empty() { Value::Null } else { json!(dt) },
            "event": ev,
            "priceUsd": pr,
            "deltaFromPriorListPct": delta_pct,
            "note": note,
        }));
    }

    // Reverse chronological order so newest listing/sale events appear first in Agent Deal-Desk UI
    prior_listing_events.reverse();

    json!({
        "ownershipTenureYears": tenure_years,
        "lastSaleDate": last_sale_date,
        "lastSalePriceUsd": last_sale_price,
        "benchmarkValueUsd": est_val,
        "estimatedGrossEquityGainUsd": gross_equity_gain_usd,
        "estimatedGrossEquityGainPct": gross_equity_gain_pct,
        "grossEquityBasisLabel": "Gross price appreciation before mortgage debt (private loan balance is not in public records)",
        "assessedValueUsd": ctx.current_assessed_usd,
        "assessedToMarketRatioPct": assessed_to_market_pct,
        "taxLockInStrength": tax_lock_in_strength,
        "priceCutCount": price_cut_count,
        "withdrawnOrExpiredCount": withdrawn_or_expired_count,
        "priorListingEvents": prior_listing_events,
        "privacyNotice": "Zero-PII parcel intelligence: derived strictly from public deed/MLS event dates and county roll assessments without homeowner names, contact data, or private debt balances.",
    })
}

// =================================================================================================
// Unit Tests (Real Fixtures + Missing-Field Zero-Mock Honesty + Cliff/Portability/Seasonality)
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_json_honesty_zero_panics_zero_invented_numbers() {
        let out = build_seller_and_agent_suites(&json!({}));

        // Part 1: Seller Suite missing-field honesty
        let ns = &out.seller_suite["netSheetModel"];
        assert!(ns["benchmarkSalePriceUsd"].is_null());
        assert!(ns["lowSalePriceUsd"].is_null());
        assert!(ns["highSalePriceUsd"].is_null());
        assert!(ns["recordedPurchasePriceUsd"].is_null());
        assert!(ns["recordedPurchaseDate"].is_null());
        assert!(ns["documentedPermittedImprovementsUsd"].is_null());
        assert!(ns["estimatedAdjustedBasisUsd"].is_null());
        assert!(ns["sellerTransferTaxUsd"].is_null());
        assert!(ns["transferTaxRatePct"].is_null());
        assert!(ns["transferTaxJurisdiction"].is_null());
        assert!(ns["sellerTitleAndEscrowBenchmarkUsd"].is_null());
        assert!(ns["annualPropertyTaxUsd"].is_null());
        assert!(ns["dailyProratedTaxUsd"].is_null());
        assert!(ns["mortgagePayoffUsd"].is_null());
        assert!(ns["listingBrokerFeePct"].is_null());
        assert!(ns["buyerBrokerConcessionPct"].is_null());
        assert!(ns["prepAndCreditBudgetUsd"].is_null());
        assert_eq!(ns["userEditableFields"].as_array().map(Vec::len), Some(4));

        let irc = &ns["irc121Exclusion"];
        assert_eq!(irc["singleExclusionUsd"].as_i64(), Some(250_000));
        assert_eq!(irc["marriedExclusionUsd"].as_i64(), Some(500_000));
        assert!(irc["ownershipYearsHeld"].is_null());
        assert!(irc["meetsTwoOfFiveYearTenure"].is_null());
        assert!(irc["estimatedGrossGainUsd"].is_null());
        assert!(irc["taxableGainSingleUsd"].is_null());
        assert!(irc["taxableGainMarriedUsd"].is_null());
        assert!(irc["stateTopCgPct"].is_null());

        let tp = &out.seller_suite["taxPortability"];
        assert_eq!(tp["applicable"].as_bool(), Some(false));
        assert!(tp["estimatedAnnualProp19TaxSavingsUsd"].is_null());
        assert!(tp["estimatedAnnualTaxSavingsUsd"].is_null());

        let cure = &out.seller_suite["cureBeforeEscrow"];
        assert_eq!(
            cure["openOrUnfinaledPermits"].as_array().map(Vec::len),
            Some(0)
        );
        assert!(cure["codeEnforcementOpenCount"].is_null());
        assert_eq!(
            cure["pointOfSaleMandates"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(
            cure["repairVsCreditRanking"].as_array().map(Vec::len),
            Some(0)
        );

        let lt = &out.seller_suite["launchTimingAndBracket"];
        assert_eq!(lt["seasonalityAvailable"].as_bool(), Some(false));
        assert_eq!(
            lt["monthlySeasonalityIndex"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(
            lt["strongestListingMonths"].as_array().map(Vec::len),
            Some(0)
        );
        assert!(lt["nearestBracketUsd"].is_null());
        assert!(lt["pctAboveBracket"].is_null());

        // Part 2: Agent Suite missing-field honesty
        let gates = out.agent_suite["escrowKillerMatrix"]
            .as_array()
            .expect("escrowKillerMatrix array");
        assert_eq!(gates.len(), 8);
        for g in gates {
            assert_eq!(g["status"].as_str(), Some("verify"));
        }

        let cma = &out.agent_suite["cmaWorkbench"];
        assert_eq!(cma["comps"].as_array().map(Vec::len), Some(0));
        assert!(cma["subjectSpecs"]["sqft"].is_null());
        assert!(cma["subjectSpecs"]["marginalSqftValueUsd"].is_null());

        let lead = &out.agent_suite["listingLeadSignals"];
        assert!(lead["ownershipTenureYears"].is_null());
        assert!(lead["lastSalePriceUsd"].is_null());
        assert!(lead["estimatedGrossEquityGainUsd"].is_null());
        assert!(lead["assessedToMarketRatioPct"].is_null());
        assert_eq!(lead["priorListingEvents"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn test_synth_campbell_report_grounded_outputs() {
        let mut rep = crate::test_synth::synth_report_secondary();
        if let Some(root) = rep.as_object_mut() {
            root.remove("condition");
            root.remove("permitsAndCondition");
        }
        if let Some(mods) = rep["modules"].as_object_mut() {
            mods.remove("condition");
            mods.remove("permitsAndCondition");
        }
        if let Some(fobj) = rep["facts"].as_object_mut() {
            fobj.remove("buildingRegister");
            fobj.remove("countyRecords");
        }
        rep["modules"]["valuation"]["data"]["estimate"] = json!({
            "value": 2_380_000,
            "low": 2_190_000,
            "high": 2_570_000,
            "asOf": "2026-10-01"
        });
        rep["facts"]["lastSale"] = json!({ "date": "2022-06-15", "price": 2_260_000 });
        rep["facts"]["taxHistory"] = json!([
            { "year": 2025, "assessment": 2_510_000, "tax": 30_120 }
        ]);
        rep["facts"]["officialAssessment"]["totalAssessedValue"] = json!(2_510_000);
        rep["facts"]["officialAssessment"]["annualTax"] = json!(30_120);
        rep["facts"]["permits"] = json!([
            {
                "number": "SYNTH-PLN-2025-18",
                "date": "2025-04-10",
                "status": "Under Review",
                "description": "Detached rear pergola & patio electrical"
            }
        ]);
        rep["facts"]["disclosures"] = json!({
            "codeEnforcement": { "openCount": 0, "closedCount": 1, "items": [] }
        });
        rep["facts"]["saleHistory"] = json!([
            { "date": "2022-06-15", "event": "Sold", "price": 2_260_000 },
            { "date": "2022-05-12", "event": "Pending", "price": 2_275_000 },
            { "date": "2022-04-28", "event": "Price Reduced", "price": 2_275_000 },
            { "date": "2022-04-10", "event": "Listed", "price": 2_349_000 },
            { "date": "2015-08-20", "event": "Sold", "price": 1_380_000 }
        ]);

        let out = build_seller_and_agent_suites(&rep);

        let ns = &out.seller_suite["netSheetModel"];
        assert_eq!(ns["benchmarkSalePriceUsd"].as_i64(), Some(2_380_000));
        assert_eq!(ns["lowSalePriceUsd"].as_i64(), Some(2_190_000));
        assert_eq!(ns["highSalePriceUsd"].as_i64(), Some(2_570_000));
        assert_eq!(ns["recordedPurchasePriceUsd"].as_i64(), Some(2_260_000));
        assert_eq!(ns["recordedPurchaseDate"].as_str(), Some("2022-06-15"));
        assert_eq!(ns["estimatedAdjustedBasisUsd"].as_i64(), Some(2_260_000));
        // Campbell / Santa Clara County: 0.11% county transfer tax ($2,618), $0 seller title & escrow by Northern CA custom
        assert_eq!(ns["sellerTransferTaxUsd"].as_i64(), Some(2_618));
        assert_eq!(ns["sellerTitleAndEscrowBenchmarkUsd"].as_i64(), Some(0));
        assert_eq!(ns["ownersTitlePayerCustom"].as_str(), Some("buyer"));
        assert_eq!(ns["annualPropertyTaxUsd"].as_i64(), Some(30_120));
        assert!(ns["dailyProratedTaxUsd"].as_f64().unwrap_or(0.0) > 80.0);
        // Never invent private debt or agent fees
        assert!(ns["mortgagePayoffUsd"].is_null());
        assert!(ns["listingBrokerFeePct"].is_null());

        let irc = &ns["irc121Exclusion"];
        assert_eq!(irc["meetsTwoOfFiveYearTenure"].as_bool(), Some(true));
        assert_eq!(irc["estimatedGrossGainUsd"].as_i64(), Some(120_000));
        assert_eq!(irc["taxableGainSingleUsd"].as_i64(), Some(0));
        assert_eq!(irc["taxableGainMarriedUsd"].as_i64(), Some(0));
        assert_eq!(irc["stateTopCgPct"].as_f64(), Some(13.3));

        // CA Prop 19 portability on over-assessed roll ($2,510,000 assessed vs $2,380,000 market -> $0 differential + Prop 8 flag)
        let tp = &out.seller_suite["taxPortability"];
        assert_eq!(tp["applicable"].as_bool(), Some(true));
        assert_eq!(tp["programCode"].as_str(), Some("CA_PROP_19"));
        assert_eq!(tp["currentAssessedValueUsd"].as_i64(), Some(2_510_000));
        assert_eq!(tp["estimatedAnnualProp19TaxSavingsUsd"].as_i64(), Some(0));
        assert_eq!(tp["isOverAssessedProp8Candidate"].as_bool(), Some(true));

        // Cure Before Escrow catches SYNTH-PLN-2025-18 ("Under Review"), CA POS mandates, and high-ROI prep items
        let cure = &out.seller_suite["cureBeforeEscrow"];
        let open_p = cure["openOrUnfinaledPermits"].as_array().unwrap();
        assert_eq!(open_p.len(), 1);
        assert_eq!(open_p[0]["permitNumber"].as_str(), Some("SYNTH-PLN-2025-18"));
        assert_eq!(cure["codeEnforcementOpenCount"].as_i64(), Some(0));
        assert!(!cure["pointOfSaleMandates"].as_array().unwrap().is_empty());
        let ranking = cure["repairVsCreditRanking"].as_array().unwrap();
        assert!(!ranking.is_empty());
        assert_eq!(
            ranking[0]["recommendation"].as_str(),
            Some("prep_before_listing")
        );

        // 12-Month Seasonality from 144-point synthetic ZHVI series
        let lt = &out.seller_suite["launchTimingAndBracket"];
        assert_eq!(lt["seasonalityAvailable"].as_bool(), Some(true));
        assert_eq!(
            lt["monthlySeasonalityIndex"].as_array().map(Vec::len),
            Some(12)
        );
        assert_eq!(
            lt["strongestListingMonths"].as_array().map(Vec::len),
            Some(3)
        );

        // Agent CMA Workbench exact dollar-delta reconciliation
        let cma = &out.agent_suite["cmaWorkbench"];
        let comps = cma["comps"].as_array().unwrap();
        assert!(!comps.is_empty());
        for row in comps {
            assert_eq!(row["includedByDefault"].as_bool(), Some(true));
            let tp = row["timeAdjPrice"].as_i64().unwrap();
            let d_sq = row["sqftDeltaUsd"].as_i64().unwrap();
            let d_bb = row["bedBathDeltaUsd"].as_i64().unwrap();
            let d_age = row["ageCondDeltaUsd"].as_i64().unwrap();
            let d_lot = row["lotDeltaUsd"].as_i64().unwrap();
            let adj = row["adjustedPrice"].as_i64().unwrap();
            assert_eq!(tp + d_sq + d_bb + d_age + d_lot, adj);
        }
        assert_eq!(cma["subjectSpecs"]["sqft"].as_i64(), Some(2200));
        assert!(cma["subjectSpecs"]["marginalSqftValueUsd"]
            .as_i64()
            .is_some());

        // Agent Listing Lead Signals
        let lead = &out.agent_suite["listingLeadSignals"];
        assert_eq!(lead["lastSalePriceUsd"].as_i64(), Some(2_260_000));
        assert_eq!(lead["estimatedGrossEquityGainUsd"].as_i64(), Some(120_000));
        assert_eq!(lead["priceCutCount"].as_i64(), Some(1));
        assert!(lead["priorListingEvents"].as_array().unwrap().len() >= 5);
    }

    #[test]
    fn test_synth_san_jose_measure_e_and_gates() {
        let mut rep = crate::test_synth::synth_report_primary();
        rep["modules"]["valuation"]["data"]["estimate"] = json!({
            "value": 2_114_000,
            "low": 1_980_000,
            "high": 2_250_000,
            "asOf": "2026-09-15"
        });
        rep["facts"]["lastSale"] = json!({ "date": "2025-03-05", "price": 2_320_000 });
        rep["facts"]["permits"] = json!([]);
        rep["facts"]["sqftNote"] =
            json!("Includes ~180 sq ft unpermitted sunroom enclosure beyond assessor roll.");

        let out = build_seller_and_agent_suites(&rep);

        let ns = &out.seller_suite["netSheetModel"];
        assert_eq!(ns["benchmarkSalePriceUsd"].as_i64(), Some(2_114_000));
        assert_eq!(ns["recordedPurchasePriceUsd"].as_i64(), Some(2_320_000));
        assert_eq!(ns["recordedPurchaseDate"].as_str(), Some("2025-03-05"));
        // San Jose Measure E ($2M+ @ 0.75% = $855) + 50% SJ 0.33% conveyance ($3,488.1) + 0.11% SCC ($2,325.4) = $6,669
        assert_eq!(ns["sellerTransferTaxUsd"].as_i64(), Some(6_669));
        assert!(ns["transferTaxJurisdiction"]
            .as_str()
            .unwrap_or("")
            .contains("Measure E"));
        assert_eq!(ns["sellerTitleAndEscrowBenchmarkUsd"].as_i64(), Some(0));

        // Held ~1.53 yrs (< 2.0 yrs) with negative gross gain (-$206,000) -> $0 taxable gain
        let irc = &ns["irc121Exclusion"];
        assert_eq!(irc["meetsTwoOfFiveYearTenure"].as_bool(), Some(false));
        assert_eq!(irc["estimatedGrossGainUsd"].as_i64(), Some(-206_000));
        assert_eq!(irc["taxableGainSingleUsd"].as_i64(), Some(0));

        // Escrow Killer Matrix flags Jumbo at 80% LTV ($1,691,200 > $1,249,125) and sunroom sq-ft note
        let gates = out.agent_suite["escrowKillerMatrix"].as_array().unwrap();
        let loan_gate = gates
            .iter()
            .find(|g| g["id"] == "loan_tier_appraisal_overlay")
            .unwrap();
        assert_eq!(loan_gate["status"].as_str(), Some("watch"));
        let sqft_gate = gates
            .iter()
            .find(|g| g["id"] == "permit_sqft_discrepancy")
            .unwrap();
        assert_eq!(sqft_gate["status"].as_str(), Some("watch"));
    }

    #[test]
    fn test_bracket_cliff_short_series_and_multi_state_rules() {
        // 24-month series (< 36 months) must NOT invent a synthetic seasonality curve,
        // while $2,040,000 (+2.0% above $2.0M) must trigger the Search-Bracket Cliff alert,
        // and <90-day hold must trigger FHA anti-flipping blocker.
        let mut short_series = Vec::new();
        for i in 0..24 {
            let yr = 2024 + (i / 12);
            let mo = (i % 12) + 1;
            short_series.push(json!([
                format!("{yr:04}-{mo:02}"),
                1_900_000.0 + (i as f64) * 5_000.0
            ]));
        }
        let synthetic_fl = json!({
            "geo": { "state": "FL", "county": "Orange County", "countyFips": "12095", "city": "Orlando" },
            "facts": {
                "propertyType": "Condo",
                "sqft": 1800,
                "yearBuilt": 2018,
                "hoaMonthly": 680,
                "hoa": { "hasLitigation": true, "reserveAllocationPct": 6.5 },
                "lastSale": { "date": "2026-08-01", "price": 1_200_000 },
                "disclosures": {
                    "permits": [
                        { "number": "BLD-2026-9", "date": "2026-08-15", "status": "Finaled", "jobValue": 50_000 }
                    ]
                },
                "taxHistory": [{ "year": 2026, "assessment": 1_350_000, "tax": 13_230 }],
                "taxRate": 0.0098
            },
            "modules": {
                "valuation": {
                    "data": {
                        "estimate": { "value": 2_040_000, "low": 1_950_000, "high": 2_130_000, "asOf": "2026-09-15" }
                    }
                },
                "market": {
                    "data": { "zhvi": { "series": short_series } }
                }
            }
        });

        let out = build_seller_and_agent_suites(&synthetic_fl);
        let ns = &out.seller_suite["netSheetModel"];
        assert_eq!(
            ns["documentedPermittedImprovementsUsd"].as_i64(),
            Some(50_000)
        );
        assert_eq!(ns["estimatedAdjustedBasisUsd"].as_i64(), Some(1_250_000));
        assert_eq!(ns["irc121Exclusion"]["stateTopCgPct"].as_f64(), Some(0.0));
        // Orange County FL custom: seller pays promulgated Owner's Title + Title/Attorney settlement ($7,675 + $1,350 = $9,025)
        assert_eq!(ns["sellerTitleAndEscrowBenchmarkUsd"].as_i64(), Some(9_025));

        // FL Save Our Homes portability caps $690k differential at $500,000 statutory max
        let tp = &out.seller_suite["taxPortability"];
        assert_eq!(tp["programCode"].as_str(), Some("FL_SAVE_OUR_HOMES"));
        assert_eq!(tp["portableAssessmentReductionUsd"].as_i64(), Some(500_000));
        assert_eq!(tp["estimatedAnnualTaxSavingsUsd"].as_i64(), Some(4_900));

        // <36 months -> empty seasonality array, while $2,040,000 flags $2,000,000 cliff (+2.0%)
        let lt = &out.seller_suite["launchTimingAndBracket"];
        assert_eq!(lt["seasonalityAvailable"].as_bool(), Some(false));
        assert_eq!(
            lt["monthlySeasonalityIndex"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(lt["nearestBracketUsd"].as_i64(), Some(2_000_000));
        assert_eq!(lt["pctAboveBracket"].as_f64(), Some(2.0));

        // FHA <90d flip AND Condo litigation/<10% reserves both flag "blocker"
        let gates = out.agent_suite["escrowKillerMatrix"].as_array().unwrap();
        let fha_g = gates
            .iter()
            .find(|g| g["id"] == "fha_anti_flipping")
            .unwrap();
        assert_eq!(fha_g["status"].as_str(), Some("blocker"));
        let condo_g = gates
            .iter()
            .find(|g| g["id"] == "condo_pud_warrantability")
            .unwrap();
        assert_eq!(condo_g["status"].as_str(), Some("blocker"));
    }

    #[test]
    fn gate5_excludes_solar_pv_and_ambiguous_who_pays_is_null() {
        // 1. Solar PV / battery / EV / parent-lot permits must NEVER satisfy reroof verification
        assert!(!is_verified_reroof_permit(&json!({
            "category": "solar",
            "rawType": "Roof Mounted PV System",
            "description": "Install 7.2 kW roof mounted solar PV system and Tesla Powerwall"
        })));
        assert!(!is_verified_reroof_permit(&json!({
            "category": "electrical",
            "description": "200A subpanel upgrade & EV charger with roof conduit"
        })));
        assert!(!is_verified_reroof_permit(&json!({
            "category": "reroof",
            "description": "Tear-off composition shingle re-roof",
            "isParentLot": true
        })));
        assert!(is_verified_reroof_permit(&json!({
            "category": "reroof",
            "description": "Tear-off composition shingle re-roof, 24 squares"
        })));

        // 2. Statewide CA (`varies_by_county` without county FIPS) must emit Null seller title/escrow benchmark
        let mut ca_ambig = crate::test_synth::synth_report_secondary();
        ca_ambig["geo"] = json!({ "state": "CA" });
        let out = build_seller_and_agent_suites(&ca_ambig);
        let ns = &out.seller_suite["netSheetModel"];
        assert!(ns["sellerTitleAndEscrowBenchmarkUsd"].is_null());
        assert_eq!(ns["ownersTitlePayerCustom"].as_str(), Some("varies_by_county"));
    }
}
