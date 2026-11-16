//! Canonical 2026 FHFA conforming, high-balance, and HUD/FHA county mortgage loan limits.
//!
//! Eliminates 2024/2025/2026 limit drift previously scattered across `valuation.rs`,
//! `deed_title_marketplace.rs`, `buyer_diligence.rs`, and `ai/agent_harness.rs`.

use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub const FHFA_BASELINE_1UNIT_2026: i64 = 832_750;
pub const FHFA_CEILING_1UNIT_2026: i64 = 1_249_125;
pub const FHA_FLOOR_1UNIT_2026: i64 = 541_287;

pub static HIGH_COST_CEILING_COUNTIES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        // California coastal & Bay Area / SoCal ceiling counties
        "06001", "06013", "06041", "06055", "06075", "06081", "06085", "06087", "06069", "06053",
        "06083", "06079", "06037", "06059", "06111",
        // NY / NJ / MA islands / DC / NoVA / MD
        "36005", "36047", "36059", "36061", "36081", "36085", "36103", "36119", "34003", "34013",
        "34017", "34019", "34027", "34031", "34035", "34039", "25007", "25019", "11001", "51013",
        "51059", "51107", "51510", "51600", "51610", "24031", "24033",
        // Resort & statutory high-cost counties (WY Teton, CO mountain, UT Summit, ID Blaine, HI, AK)
        "56039", "08037", "08045", "08097", "08117", "49043", "16013", "15001", "15003", "15007",
        "15009", "02013", "02016", "02020", "02110", "02220",
    ])
});

pub static PARTIAL_HIGH_COST_BY_FIPS: Lazy<HashMap<&'static str, i64>> = Lazy::new(|| {
    HashMap::from([
        ("06073", 1_077_550), // San Diego, CA
        ("06067", 832_750),   // Sacramento, CA
        ("53033", 1_037_300), // King County, WA
        ("53053", 1_037_300), // Pierce County, WA
        ("53061", 1_037_300), // Snohomish County, WA
        ("25017", 943_000),   // Middlesex, MA
        ("25021", 943_000),   // Norfolk, MA
        ("25023", 943_000),   // Plymouth, MA
        ("25025", 943_000),   // Suffolk, MA
        ("25009", 943_000),   // Essex, MA
        ("08001", 862_500),   // Adams, CO
        ("08005", 862_500),   // Arapahoe, CO
        ("08013", 897_000),   // Boulder, CO
        ("08031", 862_500),   // Denver, CO
        ("08035", 862_500),   // Douglas, CO
        ("08059", 862_500),   // Jefferson, CO
    ])
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountyLoanLimits {
    pub conforming_limit: i64,
    pub fha_limit: i64,
    pub baseline_limit: i64,
    pub is_high_cost_area: bool,
    pub units: usize,
}

/// FHFA statutory 1-to-4 unit conforming limit multiplier.
pub fn unit_limit_multiplier(units: usize) -> f64 {
    match units.clamp(1, 4) {
        1 => 1.0,
        2 => 1.2808,
        3 => 1.5485,
        _ => 1.9245,
    }
}

/// Infer 1–4 residential units from free-text `propertyType`.
pub fn infer_residential_units_from_type(property_type: &str) -> usize {
    let pt_lc = property_type.to_ascii_lowercase();
    if pt_lc.contains("fourplex") || pt_lc.contains("4-plex") || pt_lc.contains("quadplex") {
        4
    } else if pt_lc.contains("triplex") || pt_lc.contains("3-plex") {
        3
    } else if pt_lc.contains("duplex")
        || pt_lc.contains("multi")
        || pt_lc.contains("2-4")
        || pt_lc.contains("2–4")
    {
        2
    } else {
        1
    }
}

/// Compute 2026 FHFA & FHA loan limits for `(state, county_fips, units)`.
pub fn resolve_county_loan_limits(state: &str, county_fips: &str, units: usize) -> CountyLoanLimits {
    let st = state.trim().to_ascii_uppercase();
    let fips = county_fips.trim();
    // Preserve historical 2-unit multiplier behavior when `infer_residential_units_from_type` returns 2
    let u = units.clamp(1, 4);
    let mult = unit_limit_multiplier(u);

    let baseline_1u = FHFA_BASELINE_1UNIT_2026 as f64;
    let ceiling_1u = FHFA_CEILING_1UNIT_2026 as f64;
    let fha_floor_1u = FHA_FLOOR_1UNIT_2026 as f64;

    let is_ceiling = HIGH_COST_CEILING_COUNTIES.contains(fips)
        || matches!(st.as_str(), "AK" | "HI" | "GU" | "VI");

    let one_unit_conf = if is_ceiling {
        ceiling_1u
    } else {
        PARTIAL_HIGH_COST_BY_FIPS
            .get(fips)
            .copied()
            .map(|x| x as f64)
            .unwrap_or(baseline_1u)
    };

    let one_unit_fha = if is_ceiling {
        ceiling_1u
    } else {
        PARTIAL_HIGH_COST_BY_FIPS
            .get(fips)
            .copied()
            .map(|x| x as f64)
            .unwrap_or(fha_floor_1u)
    };

    let conforming_limit = (one_unit_conf * mult).round() as i64;
    let baseline_limit = (baseline_1u * mult).round() as i64;
    let fha_limit = (one_unit_fha * mult).round() as i64;

    CountyLoanLimits {
        conforming_limit,
        fha_limit,
        baseline_limit,
        is_high_cost_area: conforming_limit > baseline_limit,
        units: u,
    }
}

/// Resolve loan limits from `valuation.loanPrograms` if already computed, otherwise deterministically
/// from `(geo, facts)`.
pub fn resolve_loan_limits_from_report_parts(
    geo: &Value,
    facts: &Value,
    valuation: Option<&Value>,
) -> CountyLoanLimits {
    let pt = facts
        .get("propertyType")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let units = infer_residential_units_from_type(pt);

    if let Some(val) = valuation {
        if let (Some(conf), Some(fha), Some(base), Some(high)) = (
            val.pointer("/loanPrograms/countyConformingLimit")
                .and_then(|v| v.as_i64()),
            val.pointer("/loanPrograms/countyFhaLimit")
                .and_then(|v| v.as_i64()),
            val.pointer("/loanPrograms/baselineConformingLimit")
                .and_then(|v| v.as_i64()),
            val.pointer("/loanPrograms/isHighCostArea")
                .and_then(|v| v.as_bool()),
        ) {
            return CountyLoanLimits {
                conforming_limit: conf,
                fha_limit: fha,
                baseline_limit: base,
                is_high_cost_area: high,
                units,
            };
        }
    }

    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    resolve_county_loan_limits(st, fips, units)
}

/// Classify a base loan amount into `"Conforming"`, `"High-Balance Conforming"`, or `"Jumbo (Non-Conforming)"`.
pub fn classify_loan_tier(loan_amount: i64, limits: &CountyLoanLimits) -> &'static str {
    if loan_amount <= limits.baseline_limit {
        "Conforming"
    } else if loan_amount <= limits.conforming_limit {
        "High-Balance Conforming"
    } else {
        "Jumbo (Non-Conforming)"
    }
}
