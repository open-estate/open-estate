//! Canonical regional construction/rebuild cost indices, HO-3 insurance base rates,
//! peril county/state sets, property tax + CA Prop 13 constants, electricity rates,
//! county/state `WhoPaysRule` closing custom table, and progressive transfer-tax calculator.

use crate::db::{shared_cache_store::GeoIndexColumns, DbStore};
use crate::extensions::proto::shared_v1::{
    InsuranceBenchmarkProto, TaxRateAreaProto, TransferTaxTableProto,
};
use crate::extensions::shared_cache::{
    dataset_ids, get_shared_dataset_proto, put_shared_dataset_proto, ShareScope,
};
use crate::registry::us_geo::census_division_and_region;
use crate::shared::{now_iso, round_n};
use anyhow::Result;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

// =================================================================================================
// 1. Dwelling Reconstruction $/sqft & Regional Contractor Cost Multipliers
// =================================================================================================

pub static COUNTY_REBUILD_COST_SQFT: Lazy<HashMap<&'static str, f64>> = Lazy::new(|| {
    HashMap::from([
        // San Francisco Bay Area & Wine Country (CA)
        ("06075", 505.0), // San Francisco
        ("06041", 500.0), // Marin
        ("06081", 495.0), // San Mateo
        ("06085", 480.0), // Santa Clara
        ("06055", 480.0), // Napa
        ("06087", 472.0), // Santa Cruz
        ("06001", 470.0), // Alameda
        ("06097", 465.0), // Sonoma
        ("06013", 460.0), // Contra Costa
        // Southern & Central Coast California
        ("06083", 470.0), // Santa Barbara
        ("06037", 442.0), // Los Angeles
        ("06059", 438.0), // Orange
        ("06111", 430.0), // Ventura
        ("06073", 425.0), // San Diego
        ("06053", 440.0), // Monterey
        ("06079", 420.0), // San Luis Obispo
        // High-Cost Northeast, Pacific Northwest, Hawaii & Mountain Counties
        ("36061", 545.0), // Manhattan NY
        ("36047", 465.0), // Brooklyn NY
        ("36081", 440.0), // Queens NY
        ("36059", 435.0), // Nassau NY
        ("36103", 430.0), // Suffolk NY
        ("36119", 450.0), // Westchester NY
        ("09001", 440.0), // Fairfield CT
        ("25025", 435.0), // Suffolk MA (Boston)
        ("25017", 420.0), // Middlesex MA
        ("25019", 560.0), // Nantucket MA
        ("25007", 525.0), // Dukes MA (Martha's Vineyard)
        ("11001", 405.0), // District of Columbia
        ("53033", 420.0), // King County WA (Seattle/Bellevue)
        ("15003", 505.0), // Honolulu HI
        ("15009", 525.0), // Maui HI
        ("15007", 515.0), // Kauai HI
        ("15001", 470.0), // Hawaii County HI
        ("08097", 545.0), // Pitkin CO (Aspen)
        ("56039", 535.0), // Teton WY (Jackson)
        ("08037", 490.0), // Eagle CO (Vail)
        ("08013", 405.0), // Boulder CO
        ("08031", 345.0), // Denver CO
        // South Florida High-Velocity Hurricane Zone (HVHZ)
        ("12086", 335.0), // Miami-Dade FL
        ("12011", 330.0), // Broward FL
        ("12087", 410.0), // Monroe FL (Keys)
        ("12099", 325.0), // Palm Beach FL
    ])
});

pub fn state_base_rebuild_per_sqft(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "HI" => 475.0,
        "CA" | "AK" => 415.0,
        "NY" | "MA" => 385.0,
        "WA" | "DC" => 370.0,
        "CT" | "NJ" => 355.0,
        "OR" | "CO" | "RI" | "VT" | "NH" => 330.0,
        "MD" | "ME" | "NV" | "UT" | "MN" | "IL" => 295.0,
        "FL" | "VA" | "AZ" | "PA" | "DE" | "WI" | "MI" | "MT" | "ID" => 275.0,
        "TX" | "NC" | "GA" | "SC" | "TN" | "LA" | "MO" | "KS" | "NE" | "NM" => 245.0,
        "OK" | "OH" | "IN" | "KY" | "AL" | "MS" | "AR" | "IA" | "WV" | "ND" | "SD" => 230.0,
        _ => 240.0,
    }
}

/// Canonical Coverage-A dwelling reconstruction cost per sq ft before terrain/story/age adjustments.
pub fn rebuild_cost_per_sqft(state: &str, county_fips: Option<&str>) -> f64 {
    if let Some(fips) = county_fips.map(str::trim).filter(|f| !f.is_empty()) {
        if let Some(&c) = COUNTY_REBUILD_COST_SQFT.get(fips) {
            return c;
        }
    }
    state_base_rebuild_per_sqft(state)
}

pub static COUNTY_COST_MULT: Lazy<HashMap<&'static str, (f64, &'static str)>> = Lazy::new(|| {
    HashMap::from([
        // California — Bay Area
        ("06075", (1.58, "San Francisco Metro")),
        ("06081", (1.56, "San Mateo / Peninsula")),
        ("06085", (1.54, "Santa Clara / Silicon Valley")),
        ("06041", (1.57, "Marin County")),
        ("06001", (1.48, "Alameda / East Bay")),
        ("06013", (1.44, "Contra Costa / East Bay")),
        ("06087", (1.46, "Santa Cruz Coastal")),
        ("06055", (1.48, "Napa Valley")),
        ("06097", (1.42, "Sonoma County")),
        ("06095", (1.34, "Solano County")),
        // California — Southern & Central
        ("06037", (1.36, "Los Angeles Metro")),
        ("06059", (1.35, "Orange County")),
        ("06073", (1.33, "San Diego Metro")),
        ("06083", (1.42, "Santa Barbara Coastal")),
        ("06111", (1.32, "Ventura County")),
        ("06053", (1.38, "Monterey Peninsula")),
        ("06079", (1.34, "San Luis Obispo")),
        ("06067", (1.24, "Sacramento Metro")),
        ("06061", (1.26, "Placer County")),
        ("06017", (1.26, "El Dorado County")),
        ("06065", (1.20, "Riverside / Inland Empire")),
        ("06071", (1.20, "San Bernardino / Inland Empire")),
        // Northeast — NYC / Boston / Tri-State
        ("36061", (1.62, "Manhattan, NY")),
        ("36047", (1.48, "Brooklyn, NY")),
        ("36081", (1.45, "Queens, NY")),
        ("36005", (1.40, "Bronx, NY")),
        ("36085", (1.40, "Staten Island, NY")),
        ("36059", (1.44, "Nassau County / Long Island")),
        ("36103", (1.42, "Suffolk County / Long Island")),
        ("36119", (1.44, "Westchester County, NY")),
        ("09001", (1.36, "Fairfield County, CT")),
        ("34003", (1.35, "Bergen County, NJ")),
        ("34013", (1.34, "Essex County, NJ")),
        ("34017", (1.36, "Hudson County, NJ")),
        ("34027", (1.34, "Morris County, NJ")),
        ("25025", (1.40, "Boston / Suffolk, MA")),
        ("25017", (1.38, "Middlesex / Greater Boston, MA")),
        ("25021", (1.38, "Norfolk / Greater Boston, MA")),
        ("25009", (1.33, "Essex / North Shore, MA")),
        // Pacific NW, Hawaii, Mountain & Mid-Atlantic
        ("53033", (1.34, "Seattle / King County, WA")),
        ("53061", (1.28, "Snohomish County, WA")),
        ("53053", (1.24, "Pierce / Tacoma, WA")),
        ("41051", (1.22, "Portland / Multnomah, OR")),
        ("41067", (1.21, "Washington County, OR")),
        ("15003", (1.58, "Honolulu / Oahu, HI")),
        ("15009", (1.64, "Maui County, HI")),
        ("15007", (1.62, "Kauai County, HI")),
        ("15001", (1.52, "Hawaii Island, HI")),
        ("11001", (1.26, "Washington, DC")),
        ("51013", (1.25, "Arlington, VA")),
        ("51059", (1.24, "Fairfax County, VA")),
        ("51107", (1.22, "Loudoun County, VA")),
        ("24031", (1.24, "Montgomery County, MD")),
        ("08031", (1.18, "Denver, CO")),
        ("08013", (1.22, "Boulder, CO")),
        ("08097", (1.64, "Pitkin / Aspen, CO")),
        ("08037", (1.52, "Eagle / Vail, CO")),
        ("56039", (1.60, "Teton / Jackson, WY")),
        ("49043", (1.38, "Summit / Park City, UT")),
        ("49035", (1.09, "Salt Lake County, UT")),
        // Midwest, Sunbelt & Major Metros
        ("17031", (1.20, "Chicago / Cook County, IL")),
        ("17043", (1.18, "DuPage County, IL")),
        ("17097", (1.18, "Lake County, IL")),
        ("27053", (1.14, "Minneapolis / Hennepin, MN")),
        ("12086", (1.14, "Miami-Dade, FL")),
        ("12011", (1.12, "Broward, FL")),
        ("12099", (1.14, "Palm Beach, FL")),
        ("12095", (1.03, "Orlando / Orange, FL")),
        ("12057", (1.03, "Tampa / Hillsborough, FL")),
        ("48453", (1.05, "Austin / Travis, TX")),
        ("48113", (0.98, "Dallas County, TX")),
        ("48085", (0.99, "Collin County, TX")),
        ("48439", (0.96, "Tarrant / Fort Worth, TX")),
        ("48201", (0.95, "Houston / Harris, TX")),
        ("48029", (0.93, "San Antonio / Bexar, TX")),
        ("04013", (1.03, "Phoenix / Maricopa, AZ")),
        ("04019", (0.98, "Tucson / Pima, AZ")),
        ("32003", (1.08, "Las Vegas / Clark, NV")),
        ("32031", (1.16, "Reno / Washoe, NV")),
        ("13121", (0.99, "Atlanta / Fulton, GA")),
        ("37119", (0.97, "Charlotte / Mecklenburg, NC")),
        ("37183", (0.98, "Raleigh / Wake, NC")),
        ("47037", (1.00, "Nashville / Davidson, TN")),
    ])
});

/// State-level contractor/remodel cost multiplier relative to `1.00` US national baseline.
pub fn state_cost_multiplier(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "HI" => 1.56,
        "AK" => 1.42,
        "CA" => 1.34,
        "NY" | "MA" => 1.32,
        "NJ" => 1.28,
        "DC" => 1.26,
        "WA" | "CT" => 1.24,
        "RI" => 1.20,
        "OR" => 1.18,
        "CO" => 1.15,
        "NH" | "MD" => 1.14,
        "VT" => 1.13,
        "IL" => 1.12,
        "ME" | "MN" => 1.10,
        "NV" => 1.09,
        "PA" | "VA" | "DE" => 1.08,
        "UT" => 1.06,
        "MT" => 1.05,
        "WY" | "FL" => 1.04,
        "ID" | "WI" => 1.03,
        "AZ" | "MI" => 1.01,
        "ND" => 0.98,
        "OH" => 0.97,
        "NM" | "MO" => 0.96,
        "GA" | "NC" | "TX" | "IN" | "IA" | "NE" | "SD" => 0.95,
        "TN" | "KS" => 0.94,
        "SC" => 0.93,
        "WV" | "KY" => 0.92,
        "LA" => 0.91,
        "OK" | "AL" => 0.89,
        "AR" => 0.88,
        "MS" => 0.87,
        _ => 1.00,
    }
}

/// Returns `(cost_multiplier, census_division, census_region, optional_county_market_label)`.
pub fn regional_cost_profile(
    state: &str,
    county_fips: Option<&str>,
) -> (f64, &'static str, &'static str, Option<&'static str>) {
    let (div, reg) = census_division_and_region(state);
    if let Some(fips) = county_fips.map(str::trim).filter(|f| !f.is_empty()) {
        if let Some(&(mult, label)) = COUNTY_COST_MULT.get(fips) {
            return (mult, div, reg, Some(label));
        }
    }
    (state_cost_multiplier(state), div, reg, None)
}

/// Calibrates licensed contractor repair pricing by US metro / county / state from `report` JSON
/// (or applies an explicit caller `override_multiplier` clamped to `0.5..=3.0`).
pub fn calibrate_regional_cost_multiplier_from_report(
    report: Option<&Value>,
    override_multiplier: Option<f64>,
) -> (f64, String) {
    if let Some(ov) = override_multiplier {
        if ov.is_finite() && ov > 0.0 {
            let clamped = (ov.clamp(0.5, 3.0) * 100.0).round() / 100.0;
            return (
                clamped,
                format!("Custom regional contractor multiplier ({clamped:.2}x)"),
            );
        }
    }

    let Some(rep) = report else {
        return (1.0, "US National Baseline (1.00x)".to_string());
    };

    let state = rep
        .pointer("/geo/state")
        .or_else(|| rep.pointer("/meta/state"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    let county_fips = rep
        .pointer("/geo/countyFips")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(fips) = county_fips {
        if let Some(&(mult, label)) = COUNTY_COST_MULT.get(fips) {
            return (mult, format!("{label} ({mult:.2}x US avg)"));
        }
    }

    let metro_or_city = format!(
        "{} {} {}",
        rep.pointer("/geo/msa/name")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        rep.pointer("/geo/city")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        rep.pointer("/meta/displayAddress")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
    .to_ascii_lowercase();

    if metro_or_city.contains("san jose")
        || metro_or_city.contains("san francisco")
        || metro_or_city.contains("oakland")
        || metro_or_city.contains("sunnyvale")
        || metro_or_city.contains("palo alto")
        || metro_or_city.contains("santa clara")
        || metro_or_city.contains("san mateo")
    {
        return (
            1.50,
            "Bay Area / Silicon Valley, CA (1.50x US avg)".to_string(),
        );
    }
    if metro_or_city.contains("los angeles")
        || metro_or_city.contains("san diego")
        || metro_or_city.contains("irvine")
        || metro_or_city.contains("anaheim")
        || metro_or_city.contains("new york")
        || metro_or_city.contains("boston")
        || metro_or_city.contains("seattle")
        || metro_or_city.contains("honolulu")
    {
        return (
            1.38,
            format!(
                "High-Cost Coastal Metro ({}, 1.38x US avg)",
                if state.is_empty() { "Coastal" } else { &state }
            ),
        );
    }

    if state.is_empty() {
        return (1.00, "US National Baseline (1.00x)".to_string());
    }
    let mult = state_cost_multiplier(&state);
    (
        mult,
        format!("{state} Licensed Trade Index ({mult:.2}x US avg)"),
    )
}

// =================================================================================================
// 2. Homeowners Insurance (HO-3) Base Rates & Peril County/State Sets
// =================================================================================================

pub static TIER1_COASTAL_WIND_FIPS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        // Florida HVHZ & Coastal Wind-Borne Debris Counties
        "12086", "12011", "12099", "12087", "12071", "12021", "12103", "12057", "12115", "12081",
        "12009", "12031", "12109", "12033", "12005", "12091", "12113", "12131", "12015", "12053",
        "12101", "12061", "12085", "12111", "12127", "12035", "12089",
        // Texas TWIA Tier-1 Coastal Counties + Harris Bayfront
        "48167", "48039", "48355", "48061", "48245", "48071", "48321", "48007", "48409", "48057",
        "48273", "48489", "48391", "48261",
        // Louisiana Coastal / Citizens Wind Zone Parishes
        "22071", "22051", "22075", "22087", "22103", "22109", "22057", "22019", "22113", "22045",
        "22023", "22089", "22093", "22095",
        // AL / MS / SC / NC / GA / NY / MA Coastal Wind Pools
        "01097", "01003", "28047", "28045", "28059", "45019", "45013", "45051", "45043", "37049",
        "37055", "37129", "37019", "37031", "37133", "37041", "37137", "37141", "13051", "13127",
        "36103", "25001", "25007", "25019",
    ])
});

pub static CONVECTIVE_HAIL_STATES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        "CO", "TX", "OK", "KS", "NE", "MN", "MO", "IA", "AR", "SD", "ND", "IL", "IN", "MS", "AL",
        "TN", "WY",
    ])
});

/// Baseline admitted HO-3 rate per $1 of Coverage A BEFORE parcel-specific peril surcharges.
pub fn state_admitted_ho3_rate(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "FL" => 0.0086,
        "LA" => 0.0080,
        "OK" => 0.0074,
        "TX" => 0.0066,
        "KS" | "NE" => 0.0062,
        "CO" | "MS" | "AL" | "AR" => 0.0054,
        "MN" | "MO" | "IA" | "SD" | "ND" | "TN" => 0.0046,
        "SC" | "NC" | "GA" => 0.0042,
        "CA" => 0.0033,
        "HI" | "WA" | "OR" | "UT" | "NV" | "ID" => 0.0028,
        "NY" | "NJ" | "MA" | "CT" | "RI" | "MD" | "VA" | "DC" => 0.0034,
        _ => 0.0036,
    }
}

/// Effective annual insurance rate as a fraction of market price when Coverage A / parcel hazards
/// are not yet available (e.g., fast scan, alerts, or missing sqft fallback).
pub fn state_effective_insurance_pct_of_price(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "FL" => 0.0110,
        "LA" => 0.0095,
        "OK" => 0.0075,
        "TX" => 0.0068,
        "KS" => 0.0062,
        "NE" => 0.0056,
        "CO" | "MS" | "AL" | "AR" => 0.0052,
        "CA" => 0.0035,
        "HI" | "WA" | "OR" | "UT" | "NV" => 0.0032,
        _ => 0.0040,
    }
}

/// Baseline annual HO-3 insurance estimate combining Coverage-A rebuild (`sqft * $/sqft * admitted_rate`)
/// with liability/market loading (`price * 0.0002`), floored at `$1,200/yr`.
pub fn estimate_baseline_annual_insurance(
    price: f64,
    sqft: Option<f64>,
    state: &str,
    county_fips: Option<&str>,
) -> f64 {
    let rebuild = sqft.unwrap_or(1_800.0) * rebuild_cost_per_sqft(state, county_fips);
    (rebuild * state_admitted_ho3_rate(state) + price.max(0.0) * 0.0002).max(1_200.0)
}

// =================================================================================================
// 3. Property Tax & California Proposition 13 Constants + Regional Utility Rates
// =================================================================================================

/// California Constitution Art. XIIIA § 1 statutory base rate (`1.00%`, excluding local voter bonds).
pub const CA_PROP13_CONSTITUTIONAL_BASE_RATE: f64 = 0.0100;
/// Typical California post-reassessment effective rate including local voter-approved bonds (`1.15%`).
pub const CA_PROP13_TYPICAL_EFFECTIVE_RATE: f64 = 0.0115;

/// Canonical state-level effective residential property tax rate (fraction of market value, e.g. `0.0115` = 1.15%)
/// used as a fallback when parcel-specific tax roll data is unavailable.
pub fn state_effective_property_tax_rate(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "NJ" => 0.0223,
        "IL" => 0.0207,
        "CT" => 0.0196,
        "NH" => 0.0186,
        "VT" => 0.0183,
        "TX" => 0.0175,
        "NY" => 0.0162,
        "NE" => 0.0154,
        "WI" => 0.0151,
        "OH" => 0.0148,
        "IA" => 0.0143,
        "PA" => 0.0141,
        "MI" => 0.0138,
        "KS" => 0.0134,
        "RI" => 0.0132,
        "ME" => 0.0120,
        "CA" => CA_PROP13_TYPICAL_EFFECTIVE_RATE,
        "MA" => 0.0114,
        "AK" => 0.0114,
        "SD" => 0.0112,
        "MN" => 0.0108,
        "MD" => 0.0102,
        "FL" => 0.0098,
        "ND" => 0.0096,
        "WA" => 0.0094,
        "MO" => 0.0093,
        "OR" => 0.0092,
        "GA" => 0.0088,
        "OK" => 0.0085,
        "KY" => 0.0083,
        "VA" => 0.0082,
        "IN" => 0.0081,
        "NC" => 0.0078,
        "MT" => 0.0076,
        "NM" => 0.0075,
        "MS" => 0.0075,
        "TN" => 0.0064,
        "ID" => 0.0063,
        "AZ" => 0.0062,
        "AR" => 0.0061,
        "DC" => 0.0060,
        "UT" => 0.0058,
        "WY" => 0.0057,
        "WV" => 0.0057,
        "SC" => 0.0056,
        "DE" => 0.0056,
        "CO" => 0.0055,
        "NV" => 0.0055,
        "LA" => 0.0055,
        "AL" => 0.0040,
        "HI" => 0.0030,
        _ => 0.0110,
    }
}

/// EIA state-level residential retail electricity benchmark (`$/kWh`) fallback ONLY when live
/// utility tariff, CEC/HIFLD EIA-861, OpenEI URDB, and BLS series are unavailable.
/// Updated against EIA Electric Power Monthly state residential averages (`cents/kWh -> $/kWh`).
pub fn lookup_state_eia_benchmark_kwh(state: &str) -> (f64, &'static str) {
    match state.trim().to_ascii_uppercase().as_str() {
        "HI" => (0.422, "EIA State Residential Average Fallback (HI)"),
        "CA" => (0.302, "EIA State Residential Average Fallback (CA)"),
        "CT" | "MA" | "NH" | "RI" | "ME" => (
            0.285,
            "EIA State Residential Average Fallback (New England)",
        ),
        "NY" | "AK" => (0.245, "EIA State Residential Average Fallback (NY/AK)"),
        "NJ" | "VT" => (0.205, "EIA State Residential Average Fallback (NJ/VT)"),
        "MI" | "PA" | "MD" | "DC" => (
            0.180,
            "EIA State Residential Average Fallback (Mid-Atlantic/MI)",
        ),
        "FL" | "AZ" | "TX" | "CO" | "NV" => {
            (0.150, "EIA State Residential Average Fallback (Sunbelt)")
        }
        "WA" | "OR" | "ID" | "UT" => (
            0.125,
            "EIA State Residential Average Fallback (PNW/Mountain)",
        ),
        _ => (0.168, "EIA National Residential Average Fallback (US)"),
    }
}

/// EIA residential retail electricity benchmark (`$/kWh`) by state (offline fallback only).
pub fn regional_electricity_kwh_rate(state: &str) -> f64 {
    lookup_state_eia_benchmark_kwh(state).0
}

// =================================================================================================
// 4. Granular County & State Title/Escrow `WhoPaysRule` Registry
// =================================================================================================

pub const WHO_PAYS_LABEL: &str = "Typical custom, negotiable, confirm with escrow";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhoPaysRule {
    pub region_key: &'static str,
    pub region_name: &'static str,
    pub state: &'static str,
    pub county_fips: Option<&'static str>,
    pub owners_policy_payer: &'static str,
    pub lenders_policy_payer: &'static str,
    pub escrow_fee_payer: &'static str,
    pub rate_regime: &'static str,
    pub closing_model: &'static str,
    pub custom_note: &'static str,
    pub label: &'static str,
}

pub static WHO_PAYS_TABLE: Lazy<Vec<WhoPaysRule>> = Lazy::new(|| {
    vec![
        WhoPaysRule {
            region_key: "06085",
            region_name: "Santa Clara County, CA (Northern CA)",
            state: "CA",
            county_fips: Some("06085"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Northern California Bay Area custom: buyer usually pays owner's title policy, lender's policy, and escrow fee; city/county transfer taxes vary (e.g., San Jose Measure E / conveyance tax).",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06037",
            region_name: "Los Angeles County, CA (Southern CA)",
            state: "CA",
            county_fips: Some("06037"),
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Southern California custom: seller usually pays owner's title policy; buyer pays lender's policy; escrow fee is typically split 50/50 (or each pays own side).",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06075",
            region_name: "San Francisco County, CA",
            state: "CA",
            county_fips: Some("06075"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Buyer customarily pays owner's & lender's title policies and escrow fee; seller customarily pays tiered SF transfer tax.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06001",
            region_name: "Alameda County, CA",
            state: "CA",
            county_fips: Some("06001"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Northern CA East Bay custom: buyer usually pays title & escrow; city transfer taxes (e.g., Oakland/Berkeley) often split.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06013",
            region_name: "Contra Costa County, CA",
            state: "CA",
            county_fips: Some("06013"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Northern CA custom: buyer typically pays owner's & lender's title policies and escrow fee.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06081",
            region_name: "San Mateo County, CA",
            state: "CA",
            county_fips: Some("06081"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Peninsula custom: buyer typically pays title & escrow fees.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06041",
            region_name: "Marin County, CA",
            state: "CA",
            county_fips: Some("06041"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "North Bay custom: buyer typically pays owner's & lender's title policies and escrow fee.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06059",
            region_name: "Orange County, CA",
            state: "CA",
            county_fips: Some("06059"),
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Southern CA custom: seller typically pays owner's title policy; buyer pays lender's policy; escrow typically split.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06073",
            region_name: "San Diego County, CA",
            state: "CA",
            county_fips: Some("06073"),
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Southern CA custom: seller typically pays owner's title policy; escrow fee typically split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06065",
            region_name: "Riverside County, CA",
            state: "CA",
            county_fips: Some("06065"),
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Inland Empire / Southern CA custom: seller typically pays owner's policy; escrow typically split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06071",
            region_name: "San Bernardino County, CA",
            state: "CA",
            county_fips: Some("06071"),
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Inland Empire / Southern CA custom: seller typically pays owner's policy; escrow typically split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06111",
            region_name: "Ventura County, CA",
            state: "CA",
            county_fips: Some("06111"),
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Southern CA custom: seller typically pays owner's policy; buyer pays lender's policy.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "06067",
            region_name: "Sacramento County, CA",
            state: "CA",
            county_fips: Some("06067"),
            owners_policy_payer: "split_50_50",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Sacramento Valley custom: owner's policy and escrow fees are often split 50/50 (varies by contract).",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "CA",
            region_name: "California (Statewide Default)",
            state: "CA",
            county_fips: None,
            owners_policy_payer: "varies_by_county",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "varies_by_county",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "California custom varies sharply by county: Northern CA buyers typically pay owner's policy; Southern CA sellers typically pay owner's policy.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "TX",
            region_name: "Texas (Promulgated-Rate State)",
            state: "TX",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "promulgated",
            closing_model: "Title / Escrow Company (Texas TDI Promulgated Rates)",
            custom_note: "Texas Dept. of Insurance promulgates uniform base title premiums statewide; seller typically pays owner's policy in TREC contracts, while buyer pays lender's policy & endorsements.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "12086",
            region_name: "Miami-Dade County, FL (Promulgated-Rate State)",
            state: "FL",
            county_fips: Some("12086"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "promulgated",
            closing_model: "Title Company or Closing Attorney (FL Promulgated Rates)",
            custom_note: "Florida OIR promulgates base title rates; unlike most of FL, Miami-Dade/Broward custom typically assigns owner's policy & title selection to the buyer.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "12011",
            region_name: "Broward County, FL (Promulgated-Rate State)",
            state: "FL",
            county_fips: Some("12011"),
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "promulgated",
            closing_model: "Title Company or Closing Attorney (FL Promulgated Rates)",
            custom_note: "South Florida (Broward) custom typically assigns owner's title policy to the buyer under FL promulgated rate schedule.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "FL",
            region_name: "Florida (Promulgated-Rate State)",
            state: "FL",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "seller",
            rate_regime: "promulgated",
            closing_model: "Title Company or Closing Attorney (FL Promulgated Rates)",
            custom_note: "Florida OIR promulgates title insurance rates; in most FL counties the seller pays owner's policy (Miami-Dade, Broward, Collier & Sarasota customarily assign it to buyer).",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "NM",
            region_name: "New Mexico (Promulgated-Rate State)",
            state: "NM",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "promulgated",
            closing_model: "Title / Escrow Company (NM OSI Promulgated Rates)",
            custom_note: "New Mexico Office of Superintendent of Insurance promulgates title rates; seller typically pays owner's policy and escrow is split.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "NY",
            region_name: "New York (Attorney-Closing State)",
            state: "NY",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer_and_lender_attorneys",
            rate_regime: "filed_rate_bureau",
            closing_model: "Attorney-closing state (TIRSA Rate Manual)",
            custom_note: "Attorney-closing state: both buyer and seller retain attorneys (plus lender's closing attorney); buyer typically pays owner's & lender's title insurance under TIRSA filed rates.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "MA",
            region_name: "Massachusetts (Attorney-Closing State)",
            state: "MA",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "closing_attorney",
            rate_regime: "competitive_filed",
            closing_model: "Attorney-closing state",
            custom_note: "Attorney-closing state: lender's/closing attorney conducts title certification and settlement; buyer typically pays owner's & lender's policies and closing attorney fees.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "NJ",
            region_name: "New Jersey (Attorney-Closing State)",
            state: "NJ",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer_attorney_or_title",
            rate_regime: "filed_rate_bureau",
            closing_model: "Attorney-closing state (NJLTIRB Rate Manual)",
            custom_note: "Attorney-closing state with 3-day attorney review period; buyer typically pays owner's & lender's title insurance under NJLTIRB rates.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "GA",
            region_name: "Georgia (Attorney-Closing State)",
            state: "GA",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "closing_attorney",
            rate_regime: "competitive_filed",
            closing_model: "Attorney-closing state",
            custom_note: "Attorney-closing state: Georgia law requires a licensed GA attorney to supervise real estate closings; buyer typically pays title & closing attorney fees unless negotiated.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "SC",
            region_name: "South Carolina (Attorney-Closing State)",
            state: "SC",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "closing_attorney",
            rate_regime: "competitive_filed",
            closing_model: "Attorney-closing state",
            custom_note: "Attorney-closing state: SC Supreme Court rules require an attorney to oversee title search, closing, and recording; buyer typically pays title insurance.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "NC",
            region_name: "North Carolina (Attorney-Closing State)",
            state: "NC",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "closing_attorney",
            rate_regime: "filed_rate_bureau",
            closing_model: "Attorney-closing state",
            custom_note: "Attorney-closing state: closing attorney certifies title; buyer typically pays title premiums and attorney settlement fee.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "CT",
            region_name: "Connecticut (Attorney-Closing State)",
            state: "CT",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "closing_attorney",
            rate_regime: "competitive_filed",
            closing_model: "Attorney-closing state",
            custom_note: "Attorney-closing state: buyer and seller use attorneys; buyer typically pays owner's & lender's title insurance.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "WA",
            region_name: "Washington",
            state: "WA",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company (or Limited Practice Officer)",
            custom_note: "Seller typically pays owner's title policy and WA REET excise tax; buyer pays lender's policy; escrow fee typically split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "OR",
            region_name: "Oregon",
            state: "OR",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Seller typically pays owner's policy; buyer pays lender's policy; escrow fee split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "AZ",
            region_name: "Arizona",
            state: "AZ",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Seller typically pays owner's policy; buyer pays lender's policy; escrow fee split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "NV",
            region_name: "Nevada",
            state: "NV",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Escrow / Title Company",
            custom_note: "Seller typically pays owner's policy; buyer pays lender's policy; escrow fee split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "CO",
            region_name: "Colorado",
            state: "CO",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "split_50_50",
            rate_regime: "filed_competitive",
            closing_model: "Title / Escrow Company",
            custom_note: "Seller typically pays owner's policy; buyer pays lender's policy; closing fee split 50/50.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "IL",
            region_name: "Illinois",
            state: "IL",
            county_fips: None,
            owners_policy_payer: "seller",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer_or_split",
            rate_regime: "competitive_filed",
            closing_model: "Title Company + Attorney Review",
            custom_note: "Illinois custom: seller typically pays owner's policy; attorneys commonly review contracts and attend title-company closings.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "VA",
            region_name: "Virginia",
            state: "VA",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "competitive_filed",
            closing_model: "Settlement Agent / Title / Attorney",
            custom_note: "Buyer chooses settlement agent and typically pays owner's & lender's title policies and settlement fees.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "MD",
            region_name: "Maryland",
            state: "MD",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Title Company / Closing Attorney",
            custom_note: "Buyer selects title/settlement agent and typically pays title policies; state/county transfer taxes often split.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "DC",
            region_name: "District of Columbia",
            state: "DC",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_competitive",
            closing_model: "Title Company / Closing Attorney",
            custom_note: "Buyer typically selects title/settlement agent and pays title insurance; recordation & transfer taxes split.",
            label: WHO_PAYS_LABEL,
        },
        WhoPaysRule {
            region_key: "PA",
            region_name: "Pennsylvania",
            state: "PA",
            county_fips: None,
            owners_policy_payer: "buyer",
            lenders_policy_payer: "buyer",
            escrow_fee_payer: "buyer",
            rate_regime: "filed_rate_bureau",
            closing_model: "Title Agent / Closing Attorney (TIRBOP All-Inclusive Rate)",
            custom_note: "Pennsylvania TIRBOP filed rate bundles search, exam, and title policy; buyer typically pays title insurance.",
            label: WHO_PAYS_LABEL,
        },
    ]
});

pub static DEFAULT_WHO_PAYS: WhoPaysRule = WhoPaysRule {
    region_key: "US_DEFAULT",
    region_name: "US General Fallback",
    state: "US",
    county_fips: None,
    owners_policy_payer: "negotiable",
    lenders_policy_payer: "buyer",
    escrow_fee_payer: "split_50_50",
    rate_regime: "filed_competitive",
    closing_model: "Title / Escrow or Closing Attorney",
    custom_note: "Title & settlement allocation varies by county/state contract custom; buyer usually pays lender's policy while owner's policy varies.",
    label: WHO_PAYS_LABEL,
};

/// Resolve county-first, then state-level `WhoPaysRule` closing custom for a `geo` JSON object.
pub fn lookup_who_pays(geo: &Value) -> &'static WhoPaysRule {
    let fips = geo
        .get("countyFips")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let st = geo
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();

    if !fips.is_empty() {
        if let Some(rule) = WHO_PAYS_TABLE.iter().find(|r| r.county_fips == Some(fips)) {
            return rule;
        }
    }
    let county_lc = geo
        .get("county")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !county_lc.is_empty() {
        let inferred_fips =
            if st == "CA" || county_lc.contains("santa clara") || county_lc.contains("los angeles")
            {
                if county_lc.contains("santa clara") {
                    Some("06085")
                } else if county_lc.contains("los angeles") {
                    Some("06037")
                } else if county_lc.contains("san francisco") {
                    Some("06075")
                } else if county_lc.contains("alameda") {
                    Some("06001")
                } else if county_lc.contains("contra costa") {
                    Some("06013")
                } else if county_lc.contains("san mateo") {
                    Some("06081")
                } else if county_lc.contains("marin") {
                    Some("06041")
                } else if county_lc.contains("orange") {
                    Some("06059")
                } else if county_lc.contains("san diego") {
                    Some("06073")
                } else if county_lc.contains("riverside") {
                    Some("06065")
                } else if county_lc.contains("san bernardino") {
                    Some("06071")
                } else if county_lc.contains("ventura") {
                    Some("06111")
                } else if county_lc.contains("sacramento") {
                    Some("06067")
                } else {
                    None
                }
            } else if st == "FL" && county_lc.contains("miami-dade") {
                Some("12086")
            } else if st == "FL" && county_lc.contains("broward") {
                Some("12011")
            } else {
                None
            };
        if let Some(ifips) = inferred_fips {
            if let Some(rule) = WHO_PAYS_TABLE.iter().find(|r| r.county_fips == Some(ifips)) {
                return rule;
            }
        }
    }
    if !st.is_empty() {
        if let Some(rule) = WHO_PAYS_TABLE
            .iter()
            .find(|r| r.county_fips.is_none() && r.state == st)
        {
            return rule;
        }
    }
    &DEFAULT_WHO_PAYS
}

// =================================================================================================
// 5. Progressive City / County / State Real-Estate Transfer & Mortgage Recording Tax Calculator
// =================================================================================================

#[derive(Debug, Clone)]
pub struct TransferTaxEstimate {
    pub jurisdiction: String,
    pub total_tax_usd: f64,
    pub buyer_share_usd: f64,
    pub seller_share_usd: f64,
    pub buyer_mortgage_recording_tax_usd: f64,
    pub effective_rate_pct: f64,
    pub is_high_transfer_tax: bool,
    pub payer_custom_note: String,
    pub breakdown: Vec<String>,
}

pub fn estimate_transfer_tax_schedule(
    state: &str,
    county: &str,
    county_fips: &str,
    city: &str,
    purchase_price: f64,
    base_loan_for_mrt: f64,
) -> TransferTaxEstimate {
    let p = purchase_price.max(1.0);
    let st = state.trim().to_ascii_uppercase();
    let st_ref = st.as_str();
    let city_lc = city.to_ascii_lowercase();
    let county_lc = county.to_ascii_lowercase();
    let fips = county_fips.trim();

    if st_ref == "CA" {
        if city_lc.contains("san francisco") || fips == "06075" {
            let rate = if p <= 250_000.0 {
                0.0050
            } else if p < 1_000_000.0 {
                0.0068
            } else if p < 5_000_000.0 {
                0.0075
            } else if p < 10_000_000.0 {
                0.0225
            } else if p < 25_000_000.0 {
                0.0550
            } else {
                0.0600
            };
            let total = p * rate;
            return TransferTaxEstimate {
                jurisdiction: "San Francisco, CA (Tiered Transfer Tax)".into(),
                total_tax_usd: total,
                buyer_share_usd: 0.0,
                seller_share_usd: total,
                buyer_mortgage_recording_tax_usd: 0.0,
                effective_rate_pct: round_n(rate * 100.0, 2),
                is_high_transfer_tax: true,
                payer_custom_note: "San Francisco tiered transfer tax (0.50% to 6.00% across price brackets); customarily seller-paid on standard resales, often negotiated/split on new construction or REO.".into(),
                breakdown: vec![format!("SF City & County Tiered RPTT @ {:.2}%: ${}", rate * 100.0, total.round() as i64)],
            };
        }
        if city_lc.contains("oakland") {
            let county_tax = p * 0.0011;
            let city_rate = if p <= 300_000.0 {
                0.0100
            } else if p <= 2_000_000.0 {
                0.0150
            } else if p <= 5_000_000.0 {
                0.0175
            } else {
                0.0250
            };
            let city_tax = p * city_rate;
            let total = county_tax + city_tax;
            let buyer_share = city_tax * 0.5;
            return TransferTaxEstimate {
                jurisdiction: "Oakland / Alameda County, CA".into(),
                total_tax_usd: total,
                buyer_share_usd: buyer_share,
                seller_share_usd: total - buyer_share,
                buyer_mortgage_recording_tax_usd: 0.0,
                effective_rate_pct: round_n((total / p) * 100.0, 3),
                is_high_transfer_tax: true,
                payer_custom_note: "Oakland tiered city transfer tax (1.00%–2.50%) is customarily split 50/50 between buyer and seller; Alameda County $1.10/$1,000 is seller-paid.".into(),
                breakdown: vec![
                    format!("Oakland City Transfer Tax @ {:.2}% (50/50 split): ${}", city_rate * 100.0, city_tax.round() as i64),
                    format!("Alameda County Transfer Tax @ 0.11%: ${}", county_tax.round() as i64),
                ],
            };
        }
        if city_lc.contains("berkeley") {
            let county_tax = p * 0.0011;
            let city_rate = if p > 1_800_000.0 { 0.0250 } else { 0.0150 };
            let city_tax = p * city_rate;
            let total = county_tax + city_tax;
            let buyer_share = city_tax * 0.5;
            return TransferTaxEstimate {
                jurisdiction: "Berkeley / Alameda County, CA".into(),
                total_tax_usd: total,
                buyer_share_usd: buyer_share,
                seller_share_usd: total - buyer_share,
                buyer_mortgage_recording_tax_usd: 0.0,
                effective_rate_pct: round_n((total / p) * 100.0, 3),
                is_high_transfer_tax: true,
                payer_custom_note: "Berkeley city transfer tax (1.50% / 2.50% high-tier) is customarily split 50/50 between buyer and seller.".into(),
                breakdown: vec![
                    format!("Berkeley City Transfer Tax @ {:.2}% (50/50 split): ${}", city_rate * 100.0, city_tax.round() as i64),
                    format!("Alameda County Transfer Tax @ 0.11%: ${}", county_tax.round() as i64),
                ],
            };
        }
        if city_lc.contains("san jose") {
            let county_tax = p * 0.0011;
            let sj_conv = p * 0.0033;
            let meas_e = if p >= 2_000_000.0 {
                let b1 = (p.min(5_000_000.0) - 2_000_000.0).max(0.0) * 0.0075;
                let b2 = (p.min(10_000_000.0) - 5_000_000.0).max(0.0) * 0.0100;
                let b3 = (p - 10_000_000.0).max(0.0) * 0.0150;
                b1 + b2 + b3
            } else {
                0.0
            };
            let total = county_tax + sj_conv + meas_e;
            let buyer_share = sj_conv * 0.5;
            return TransferTaxEstimate {
                jurisdiction: "San Jose / Santa Clara County, CA (incl. Measure E)".into(),
                total_tax_usd: total,
                buyer_share_usd: buyer_share,
                seller_share_usd: total - buyer_share,
                buyer_mortgage_recording_tax_usd: 0.0,
                effective_rate_pct: round_n((total / p) * 100.0, 3),
                is_high_transfer_tax: true,
                payer_custom_note: "San Jose 0.33% conveyance tax is customarily split 50/50; Measure E progressive tax applies at $2M+ (0.75%–1.50% marginal, customarily seller-paid/negotiable).".into(),
                breakdown: vec![
                    format!("Santa Clara County @ 0.11%: ${}", county_tax.round() as i64),
                    format!("San Jose Conveyance @ 0.33% (50/50 split): ${}", sj_conv.round() as i64),
                    format!("San Jose Measure E (>= $2M marginal): ${}", meas_e.round() as i64),
                ],
            };
        }
        let county_tax = p * 0.0011;
        let la_ula = if city_lc.contains("los angeles") && p >= 5_150_000.0 {
            let r = if p >= 10_300_000.0 { 0.055 } else { 0.040 };
            p * r
        } else {
            0.0
        };
        let total = county_tax + la_ula;
        return TransferTaxEstimate {
            jurisdiction: if la_ula > 0.0 {
                "Los Angeles, CA (Measure ULA)".into()
            } else {
                "California Baseline County Transfer Tax".into()
            },
            total_tax_usd: total,
            buyer_share_usd: 0.0,
            seller_share_usd: total,
            buyer_mortgage_recording_tax_usd: 0.0,
            effective_rate_pct: round_n((total / p) * 100.0, 3),
            is_high_transfer_tax: la_ula > 0.0,
            payer_custom_note: if la_ula > 0.0 {
                "LA Measure ULA adds 4.0%–5.5% above $5.15M/$10.3M (seller-paid); county documentary transfer tax is $1.10/$1,000.".into()
            } else {
                "California baseline documentary transfer tax is $1.10 per $1,000 (0.11%), customarily seller-paid in most counties (check charter city conveyance taxes).".into()
            },
            breakdown: vec![format!(
                "County Doc Transfer Tax @ 0.11%: ${}",
                county_tax.round() as i64
            )],
        };
    }

    if st_ref == "NY" {
        let is_nyc = city_lc.contains("new york")
            || city_lc.contains("manhattan")
            || city_lc.contains("brooklyn")
            || city_lc.contains("queens")
            || city_lc.contains("bronx")
            || city_lc.contains("staten")
            || matches!(fips, "36005" | "36047" | "36061" | "36081" | "36085");
        let nys_rate = if is_nyc && p >= 3_000_000.0 {
            0.0065
        } else {
            0.0040
        };
        let nys_transfer = p * nys_rate;
        let mansion_rate = if p >= 1_000_000.0 {
            if !is_nyc || p < 2_000_000.0 {
                0.0100
            } else if p < 3_000_000.0 {
                0.0125
            } else if p < 5_000_000.0 {
                0.0150
            } else if p < 10_000_000.0 {
                0.0225
            } else if p < 15_000_000.0 {
                0.0325
            } else if p < 20_000_000.0 {
                0.0350
            } else if p < 25_000_000.0 {
                0.0375
            } else {
                0.0390
            }
        } else {
            0.0
        };
        let mansion_tax = p * mansion_rate;
        let nyc_rptt = if is_nyc {
            p * if p > 500_000.0 { 0.01425 } else { 0.0100 }
        } else {
            0.0
        };
        let mrt_rate = if is_nyc {
            if base_loan_for_mrt >= 500_000.0 {
                0.01925
            } else {
                0.0180
            }
        } else {
            0.0075
        };
        let mrt = (base_loan_for_mrt - 10_000.0).max(0.0) * mrt_rate;
        let total = nys_transfer + mansion_tax + nyc_rptt;
        return TransferTaxEstimate {
            jurisdiction: if is_nyc {
                "New York City / NYS (RPTT + Mansion Tax + MRT)".into()
            } else {
                "New York State (Transfer + Mansion Tax)".into()
            },
            total_tax_usd: total,
            buyer_share_usd: mansion_tax,
            seller_share_usd: nys_transfer + nyc_rptt,
            buyer_mortgage_recording_tax_usd: mrt,
            effective_rate_pct: round_n((total / p) * 100.0, 3),
            is_high_transfer_tax: is_nyc || mansion_tax > 0.0,
            payer_custom_note: "NYS Mansion Tax (1.00%–3.90% at $1M+) and NY Mortgage Recording Tax (~1.80%–1.925% in NYC on financed mortgages, 0% on co-ops) are buyer-paid. NYC RPTT & NYS transfer tax are customarily seller-paid on resales (often buyer-paid on sponsor new dev).".into(),
            breakdown: vec![
                format!("NYS Transfer Tax @ {:.2}% (seller): ${}", nys_rate * 100.0, nys_transfer.round() as i64),
                format!("NYC RPTT (seller on resale): ${}", nyc_rptt.round() as i64),
                format!("NYS Mansion Tax @ {:.2}% (buyer): ${}", mansion_rate * 100.0, mansion_tax.round() as i64),
                format!("NY Mortgage Recording Tax est. @ {:.3}% of loan (buyer): ${}", mrt_rate * 100.0, mrt.round() as i64),
            ],
        };
    }

    if st_ref == "WA" {
        let state_reet = {
            let b1 = p.min(525_000.0) * 0.0110;
            let b2 = (p.min(1_525_000.0) - 525_000.0).max(0.0) * 0.0128;
            let b3 = (p.min(3_025_000.0) - 1_525_000.0).max(0.0) * 0.0275;
            let b4 = (p - 3_025_000.0).max(0.0) * 0.0300;
            b1 + b2 + b3 + b4
        };
        let local_rate =
            if city_lc.contains("seattle") || county_lc.contains("king") || fips == "53033" {
                0.0050
            } else {
                0.0025
            };
        let local_reet = p * local_rate;
        let total = state_reet + local_reet;
        return TransferTaxEstimate {
            jurisdiction: if city_lc.contains("seattle") || fips == "53033" {
                "Seattle / King County, WA (Graduated WA REET + Local REET)".into()
            } else {
                "Washington State (Graduated REET)".into()
            },
            total_tax_usd: total,
            buyer_share_usd: 0.0,
            seller_share_usd: total,
            buyer_mortgage_recording_tax_usd: 0.0,
            effective_rate_pct: round_n((total / p) * 100.0, 3),
            is_high_transfer_tax: true,
            payer_custom_note: "Washington Real Estate Excise Tax (graduated 1.10%–3.00% state + 0.25%–0.50% local) is customarily seller-paid, materially affecting resale friction and net proceeds.".into(),
            breakdown: vec![
                format!("WA State Graduated REET: ${}", state_reet.round() as i64),
                format!("Local City/County REET @ {:.2}%: ${}", local_rate * 100.0, local_reet.round() as i64),
            ],
        };
    }

    if st_ref == "DC" {
        let rate_each = if p >= 400_000.0 { 0.0145 } else { 0.0110 };
        let buyer_tax = p * rate_each;
        let seller_tax = p * rate_each;
        let total = buyer_tax + seller_tax;
        return TransferTaxEstimate {
            jurisdiction: "Washington, D.C. (Recordation + Transfer Tax)".into(),
            total_tax_usd: total,
            buyer_share_usd: buyer_tax,
            seller_share_usd: seller_tax,
            buyer_mortgage_recording_tax_usd: 0.0,
            effective_rate_pct: round_n((total / p) * 100.0, 3),
            is_high_transfer_tax: true,
            payer_custom_note: "D.C. charges 1.10%–1.45% Recordation Tax (buyer) + 1.10%–1.45% Transfer Tax (seller). First-time D.C. homebuyers may qualify for a reduced 0.725% recordation rate.".into(),
            breakdown: vec![
                format!("DC Buyer Recordation Tax @ {:.2}%: ${}", rate_each * 100.0, buyer_tax.round() as i64),
                format!("DC Seller Transfer Tax @ {:.2}%: ${}", rate_each * 100.0, seller_tax.round() as i64),
            ],
        };
    }

    if st_ref == "PA" {
        let is_philly = city_lc.contains("philadelphia") || fips == "42101";
        let rate = if is_philly { 0.04278 } else { 0.0200 };
        let total = p * rate;
        let half = total * 0.5;
        return TransferTaxEstimate {
            jurisdiction: if is_philly {
                "Philadelphia, PA (Combined State + City Realty Transfer Tax)".into()
            } else {
                "Pennsylvania (State + Local Realty Transfer Tax)".into()
            },
            total_tax_usd: total,
            buyer_share_usd: half,
            seller_share_usd: total - half,
            buyer_mortgage_recording_tax_usd: 0.0,
            effective_rate_pct: round_n(rate * 100.0, 3),
            is_high_transfer_tax: true,
            payer_custom_note: "Pennsylvania realty transfer tax (4.278% in Philadelphia; 2.00% typical elsewhere) is customarily split 50/50 between buyer and seller.".into(),
            breakdown: vec![format!("Combined PA Transfer Tax @ {:.3}% (50/50 split): ${}", rate * 100.0, total.round() as i64)],
        };
    }

    if st_ref == "FL" {
        let deed_rate = if county_lc.contains("miami") || fips == "12086" {
            0.0060
        } else {
            0.0070
        };
        let deed_stamps = p * deed_rate;
        let note_stamps = base_loan_for_mrt * 0.0035;
        let intang = base_loan_for_mrt * 0.0020;
        let mrt = note_stamps + intang;
        return TransferTaxEstimate {
            jurisdiction: "Florida (Doc Stamps on Deed/Note + Intangible Tax)".into(),
            total_tax_usd: deed_stamps,
            buyer_share_usd: 0.0,
            seller_share_usd: deed_stamps,
            buyer_mortgage_recording_tax_usd: mrt,
            effective_rate_pct: round_n(deed_rate * 100.0, 3),
            is_high_transfer_tax: true,
            payer_custom_note: "Florida seller typically pays deed documentary stamps ($0.70/$100 statewide, $0.60/$100 SFR in Miami-Dade); buyer pays mortgage doc stamps ($0.35/$100) + intangible tax ($0.20/$100 of loan).".into(),
            breakdown: vec![
                format!("FL Deed Doc Stamps @ {:.2}% (seller): ${}", deed_rate * 100.0, deed_stamps.round() as i64),
                format!("FL Buyer Note Stamps (0.35%) + Intangible Tax (0.20%) on loan: ${}", mrt.round() as i64),
            ],
        };
    }

    if st_ref == "NJ" {
        let rtf = p * 0.0085;
        let mansion = if p > 1_000_000.0 { p * 0.0100 } else { 0.0 };
        let total = rtf + mansion;
        return TransferTaxEstimate {
            jurisdiction: "New Jersey (Realty Transfer Fee + >$1M Mansion Tax)".into(),
            total_tax_usd: total,
            buyer_share_usd: mansion,
            seller_share_usd: rtf,
            buyer_mortgage_recording_tax_usd: 0.0,
            effective_rate_pct: round_n((total / p) * 100.0, 3),
            is_high_transfer_tax: mansion > 0.0 || rtf > 4000.0,
            payer_custom_note: "NJ Realty Transfer Fee is seller-paid; NJ 1.00% Mansion Tax applies to buyers on residential purchases > $1,000,000.".into(),
            breakdown: vec![
                format!("NJ Seller RTF (est.): ${}", rtf.round() as i64),
                format!("NJ Buyer 1% Mansion Tax (>$1M): ${}", mansion.round() as i64),
            ],
        };
    }

    if st_ref == "MA" {
        let rate = 0.00456;
        let total = p * rate;
        return TransferTaxEstimate {
            jurisdiction: "Massachusetts (Deeds Excise Tax)".into(),
            total_tax_usd: total,
            buyer_share_usd: 0.0,
            seller_share_usd: total,
            buyer_mortgage_recording_tax_usd: 0.0,
            effective_rate_pct: round_n(rate * 100.0, 3),
            is_high_transfer_tax: true,
            payer_custom_note: "Massachusetts deeds excise tax is $4.56 per $1,000 (0.456%, higher on Cape/Islands), customarily seller-paid.".into(),
            breakdown: vec![format!("MA Deeds Excise @ $4.56/$1k: ${}", total.round() as i64)],
        };
    }

    if st_ref == "IL" {
        let is_chi = city_lc.contains("chicago");
        let state_county = p * 0.0015;
        let chi_buyer = if is_chi { p * 0.0075 } else { 0.0 };
        let chi_seller = if is_chi { p * 0.0030 } else { 0.0 };
        let total = state_county + chi_buyer + chi_seller;
        return TransferTaxEstimate {
            jurisdiction: if is_chi {
                "Chicago / Cook County, IL"
            } else {
                "Illinois (State + County Transfer Tax)"
            }
            .into(),
            total_tax_usd: total,
            buyer_share_usd: chi_buyer,
            seller_share_usd: state_county + chi_seller,
            buyer_mortgage_recording_tax_usd: 0.0,
            effective_rate_pct: round_n((total / p) * 100.0, 3),
            is_high_transfer_tax: is_chi,
            payer_custom_note: if is_chi {
                "Chicago charges a 0.75% buyer-paid city transfer tax ($7.50/$1k) plus 0.30% seller-paid city tax and 0.15% state/county stamps.".into()
            } else {
                "Illinois baseline state + county transfer tax is $1.50/$1,000 (0.15%, seller-paid); check municipal home-rule transfer stamps.".into()
            },
            breakdown: vec![format!(
                "IL Transfer Stamps Total: ${}",
                total.round() as i64
            )],
        };
    }

    let base_rate = match st_ref {
        "MD" => 0.0125,
        "DE" => 0.0400,
        "NV" => 0.0051,
        "VA" => 0.0035,
        "NC" => 0.0020,
        "GA" => 0.0010,
        "TN" => 0.0037,
        "MN" => 0.0033,
        "WI" => 0.0030,
        "SC" => 0.0037,
        "CO" => 0.0001,
        "TX" | "AZ" | "UT" | "ID" | "MT" | "WY" | "NM" | "ND" | "MS" | "MO" | "LA" | "IN"
        | "KS" | "AK" | "OR" => 0.0,
        _ => 0.0020,
    };
    let total = p * base_rate;
    let buyer_share = if st_ref == "DE" { total * 0.5 } else { 0.0 };
    TransferTaxEstimate {
        jurisdiction: if base_rate == 0.0 {
            format!(
                "{} ($0 State Real Estate Transfer Tax)",
                if st_ref.is_empty() { "State" } else { st_ref }
            )
        } else {
            format!(
                "{} Baseline State Transfer Tax",
                if st_ref.is_empty() { "State" } else { st_ref }
            )
        },
        total_tax_usd: total,
        buyer_share_usd: buyer_share,
        seller_share_usd: total - buyer_share,
        buyer_mortgage_recording_tax_usd: 0.0,
        effective_rate_pct: round_n(base_rate * 100.0, 3),
        is_high_transfer_tax: base_rate >= 0.010,
        payer_custom_note: if base_rate == 0.0 {
            "No state documentary/real estate transfer tax (nominal deed recording fees only)."
                .into()
        } else {
            "Estimated statutory baseline rate; confirm county/municipal transfer stamps and split custom with escrow/title.".into()
        },
        breakdown: vec![format!(
            "Baseline Transfer Tax @ {:.3}%: ${}",
            base_rate * 100.0,
            total.round() as i64
        )],
    }
}

// =================================================================================================
// 6. Database-First Shared Datasets (`tax_rate_area`, `transfer_tax_table`, `insurance_benchmark`)
// =================================================================================================

pub const TAX_RATE_AREA_TTL_SECS: u64 = 30 * 86_400;
pub const TRANSFER_TAX_TABLE_TTL_SECS: u64 = 90 * 86_400;
pub const INSURANCE_BENCHMARK_TTL_SECS: u64 = 30 * 86_400;

#[derive(Debug, Clone, Default)]
pub struct RatesAndCostsPersistInput {
    pub state: String,
    pub county: String,
    pub county_fips: String,
    pub city: String,
    pub zip: String,
    pub purchase_price: f64,
    pub effective_property_tax_rate: Option<f64>,
    pub tra_code: Option<String>,
    pub has_special_tax_district: bool,
    pub special_tax_district_label: Option<String>,
    pub point_of_sale_mandates: Vec<String>,
    pub annual_insurance_usd: Option<f64>,
    pub flood_base_annual_usd: Option<i64>,
    pub earthquake_base_annual_usd: Option<i64>,
    pub carrier_market_status: Option<String>,
}

fn county_scope_key(state: &str, county_fips: &str) -> Option<String> {
    let fips = county_fips.trim();
    if !fips.is_empty() {
        return Some(fips.to_string());
    }
    let st = state.trim().to_ascii_uppercase();
    if !st.is_empty() {
        return Some(format!("state:{st}"));
    }
    None
}

fn city_params_key(city: &str) -> String {
    let slug = city.trim().to_ascii_lowercase();
    if slug.is_empty() {
        "county_default".to_string()
    } else {
        slug
    }
}

fn zip_params_key(zip: &str) -> String {
    let z = zip.trim();
    if z.is_empty() {
        "county_default".to_string()
    } else {
        format!("zip:{z}")
    }
}

pub fn build_tax_rate_area_proto(input: &RatesAndCostsPersistInput) -> TaxRateAreaProto {
    let st = input.state.trim().to_ascii_uppercase();
    let eff = input
        .effective_property_tax_rate
        .filter(|r| r.is_finite() && *r > 0.0)
        .unwrap_or_else(|| state_effective_property_tax_rate(&st));
    let base_ad_valorem_pct = if st == "CA" {
        round_n(CA_PROP13_CONSTITUTIONAL_BASE_RATE * 100.0, 3)
    } else {
        round_n(eff * 100.0, 3)
    };
    let homestead = match st.as_str() {
        "CA" => Some("Prop 13 base $7,000 homeowners exemption ($70/yr)".to_string()),
        "FL" => Some("FL Homestead Exemption up to $50,000 + Save Our Homes 3% cap".to_string()),
        "TX" => Some("TX $100,000 School District Homestead Exemption".to_string()),
        _ => None,
    };
    let source_url = if st == "CA" {
        "https://www.boe.ca.gov/proptaxes/proptax.htm"
    } else {
        "https://www.census.gov/topics/public-sector/government-finances.html"
    };
    TaxRateAreaProto {
        state: st,
        county_fips: input.county_fips.trim().to_string(),
        city: (!input.city.trim().is_empty()).then(|| input.city.trim().to_string()),
        tra_code: input
            .tra_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
        base_ad_valorem_pct,
        effective_property_tax_rate: round_n(eff, 5),
        has_special_tax_district: input.has_special_tax_district,
        special_tax_district_label: input.special_tax_district_label.clone(),
        homestead_exemption_summary: homestead,
        point_of_sale_mandates: input.point_of_sale_mandates.clone(),
        source_url: source_url.to_string(),
        fetched_at_rfc3339: now_iso(),
    }
}

pub fn build_transfer_tax_table_proto(input: &RatesAndCostsPersistInput) -> TransferTaxTableProto {
    let st = input.state.trim().to_ascii_uppercase();
    let price = if input.purchase_price.is_finite() && input.purchase_price > 10_000.0 {
        input.purchase_price
    } else {
        1_000_000.0
    };
    let tt = estimate_transfer_tax_schedule(
        &st,
        &input.county,
        &input.county_fips,
        &input.city,
        price,
        price * 0.80,
    );
    let (state_pct, county_pct, city_pct) = if st == "CA" {
        let county = 0.11;
        let city = (tt.effective_rate_pct - county).max(0.0);
        (0.0, county, round_n(city, 3))
    } else {
        (tt.effective_rate_pct, 0.0, 0.0)
    };
    let source_url = match st.as_str() {
        "CA" => "https://www.boe.ca.gov/proptaxes/proptax.htm",
        "NY" => "https://www.tax.ny.gov/pit/property/",
        "WA" => "https://dor.wa.gov/taxes-rates/other-taxes/real-estate-excise-tax",
        _ => "https://www.taxadmin.org/state-tax-agencies",
    };
    TransferTaxTableProto {
        state: st,
        county_fips: (!input.county_fips.trim().is_empty())
            .then(|| input.county_fips.trim().to_string()),
        city: (!input.city.trim().is_empty()).then(|| input.city.trim().to_string()),
        state_transfer_tax_pct: state_pct,
        county_transfer_tax_pct: county_pct,
        city_transfer_tax_pct: city_pct,
        custom_payer_split: tt.payer_custom_note,
        source_url: source_url.to_string(),
        fetched_at_rfc3339: now_iso(),
    }
}

pub fn build_insurance_benchmark_proto(
    input: &RatesAndCostsPersistInput,
) -> InsuranceBenchmarkProto {
    let st = input.state.trim().to_ascii_uppercase();
    let fips_opt =
        (!input.county_fips.trim().is_empty()).then(|| input.county_fips.trim().to_string());
    let geo_key = county_scope_key(&st, &input.county_fips).unwrap_or_else(|| "US".to_string());
    let price = if input.purchase_price.is_finite() && input.purchase_price > 10_000.0 {
        input.purchase_price
    } else {
        1_000_000.0
    };
    let annual = input
        .annual_insurance_usd
        .filter(|x| x.is_finite() && *x > 0.0)
        .unwrap_or_else(|| {
            estimate_baseline_annual_insurance(price, None, &st, fips_opt.as_deref())
        });
    let pct = if price > 0.0 && input.annual_insurance_usd.is_some() {
        round_n(annual / price, 5)
    } else {
        state_effective_insurance_pct_of_price(&st)
    };
    let carrier_status = input.carrier_market_status.clone().or_else(|| {
        if st == "CA" {
            Some(
                "Admitted carriers selective in WUI/coastal ZIPs; CA FAIR Plan available as wrap"
                    .to_string(),
            )
        } else if fips_opt
            .as_deref()
            .is_some_and(|f| TIER1_COASTAL_WIND_FIPS.contains(f))
        {
            Some("Tier-1 coastal wind pool / Citizens market exposure".to_string())
        } else {
            Some("Standard admitted HO-3 market".to_string())
        }
    });
    let source_url = if st == "CA" {
        "https://www.insurance.ca.gov/01-consumers/105-type/5-residential/"
    } else {
        "https://content.naic.org/cipr-topics/homeowners-insurance"
    };
    InsuranceBenchmarkProto {
        geo_key,
        state: st,
        county_fips: fips_opt,
        zip: (!input.zip.trim().is_empty()).then(|| input.zip.trim().to_string()),
        effective_pct_of_value: pct,
        median_annual_premium_usd: Some(annual.round() as i64),
        flood_base_annual_usd: input.flood_base_annual_usd,
        earthquake_base_annual_usd: input.earthquake_base_annual_usd,
        carrier_market_status: carrier_status,
        source_name: "NAIC / State DOI & FAIR Plan Homeowners Benchmark".to_string(),
        source_url: source_url.to_string(),
        fetched_at_rfc3339: now_iso(),
    }
}

pub async fn persist_rates_and_tax_datasets(
    db: &DbStore,
    input: &RatesAndCostsPersistInput,
) -> Result<()> {
    let Some(scope_key) = county_scope_key(&input.state, &input.county_fips) else {
        return Ok(());
    };
    let geo = GeoIndexColumns {
        zip: (!input.zip.trim().is_empty()).then(|| input.zip.trim().to_string()),
        county: (!input.county_fips.trim().is_empty())
            .then(|| input.county_fips.trim().to_string())
            .or_else(|| (!input.county.trim().is_empty()).then(|| input.county.trim().to_string())),
        state: (!input.state.trim().is_empty()).then(|| input.state.trim().to_ascii_uppercase()),
        ..Default::default()
    };
    let city_param = city_params_key(&input.city);
    let zip_param = zip_params_key(&input.zip);

    let tra = build_tax_rate_area_proto(input);
    let ttax = build_transfer_tax_table_proto(input);
    let ins = build_insurance_benchmark_proto(input);

    let tra_json = serde_json::to_string(&tra).ok();
    let ttax_json = serde_json::to_string(&ttax).ok();
    let ins_json = serde_json::to_string(&ins).ok();

    put_shared_dataset_proto(
        None,
        db,
        dataset_ids::TAX_RATE_AREA,
        ShareScope::County,
        &scope_key,
        &city_param,
        geo.clone(),
        &tra,
        tra_json,
        None,
        None,
        TAX_RATE_AREA_TTL_SECS,
    )
    .await?;
    put_shared_dataset_proto(
        None,
        db,
        dataset_ids::TRANSFER_TAX_TABLE,
        ShareScope::County,
        &scope_key,
        &city_param,
        geo.clone(),
        &ttax,
        ttax_json,
        None,
        None,
        TRANSFER_TAX_TABLE_TTL_SECS,
    )
    .await?;
    put_shared_dataset_proto(
        None,
        db,
        dataset_ids::INSURANCE_BENCHMARK,
        ShareScope::County,
        &scope_key,
        &zip_param,
        geo,
        &ins,
        ins_json,
        None,
        None,
        INSURANCE_BENCHMARK_TTL_SECS,
    )
    .await?;
    Ok(())
}

pub async fn load_tax_rate_area_shared(
    db: &DbStore,
    state: &str,
    county_fips: &str,
    city: &str,
) -> Result<Option<TaxRateAreaProto>> {
    let Some(scope_key) = county_scope_key(state, county_fips) else {
        return Ok(None);
    };
    let hit = get_shared_dataset_proto::<TaxRateAreaProto>(
        None,
        db,
        dataset_ids::TAX_RATE_AREA,
        ShareScope::County,
        &scope_key,
        &city_params_key(city),
    )
    .await?;
    Ok(hit.map(|(_, p)| p))
}

pub async fn load_transfer_tax_table_shared(
    db: &DbStore,
    state: &str,
    county_fips: &str,
    city: &str,
) -> Result<Option<TransferTaxTableProto>> {
    let Some(scope_key) = county_scope_key(state, county_fips) else {
        return Ok(None);
    };
    let hit = get_shared_dataset_proto::<TransferTaxTableProto>(
        None,
        db,
        dataset_ids::TRANSFER_TAX_TABLE,
        ShareScope::County,
        &scope_key,
        &city_params_key(city),
    )
    .await?;
    Ok(hit.map(|(_, p)| p))
}

pub async fn load_insurance_benchmark_shared(
    db: &DbStore,
    state: &str,
    county_fips: &str,
    zip: &str,
) -> Result<Option<InsuranceBenchmarkProto>> {
    let Some(scope_key) = county_scope_key(state, county_fips) else {
        return Ok(None);
    };
    let hit = get_shared_dataset_proto::<InsuranceBenchmarkProto>(
        None,
        db,
        dataset_ids::INSURANCE_BENCHMARK,
        ShareScope::County,
        &scope_key,
        &zip_params_key(zip),
    )
    .await?;
    Ok(hit.map(|(_, p)| p))
}
