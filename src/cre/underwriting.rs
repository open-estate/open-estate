// Institutional Small/Mid-Market ($1M–$25M) Deterministic CRE Financial Underwriting Engine.
// Computes:
//   - GPR, Other Income, CAM/NNN Expense Reimbursements, Vacancy & Credit Loss %, EGI
//   - Itemized OpEx with Post-Acquisition Property Tax Reassessment Shock (CA Prop 13 + split-roll
//     note, TX/FL/IL high-assessment regimes), Commercial Insurance, Management (3–6% EGI),
//     Utilities/CAM, Repairs & Maintenance, and Replacement CapEx Reserves ($/unit/yr & $/sqft/yr)
//   - NOI & Expense Ratio with automatic 3-way NNN vs. Modified Gross vs. Full-Service Gross comparison
//   - Cap Rate Valuation Matrix + 5-Point Cap Rate Sensitivity Table (-100/-50/Base/+50/+100 bps)
//   - 4-Way Commercial Debt & Return Stack Comparison:
//       1) Conventional CRE Bank / CMBS (65% LTV, 25-yr amort, 5/7/10-yr balloon, 1.25x DSCR vs LTV constraint)
//       2) SBA 504 Owner-User (50% Bank 1st + 40% CDC Debenture + 10% Buyer Down, >=51% owner occupancy)
//       3) SBA 7(a) Commercial Loan (up to $5.0M statutory cap, 25-yr amort)
//       4) Agency Multifamily — Fannie Mae DUS / Freddie Mac SBL (5+ units, 30-yr amort, 75% LTV, 1.25x DSCR)
//   - Key Investor Metrics: In-Place Cap Rate, Stabilized Yield-on-Cost, DSCR, Debt Yield,
//     Year-1 Cash-on-Cash Return, Break-Even Occupancy %, and 5-Yr & 10-Yr Levered IRR + Equity Multiple.

use crate::cre::{CreAssetClass, CreClassification};
use crate::shared::{round_n, ModuleOutput, SourceItem};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CRE_UNDERWRITING_DISCLAIMER: &str = "Automated CRE financial underwriting, cap-rate modeling, and trade-area screening — NOT a commercial appraisal (USPAP), property condition inspection, ASTM E1527-21 Phase I ESA, legal/tax opinion, or lending commitment. Verify actual T-12 financials, leases, zoning, tax reassessment rules, and lender debt quotes with licensed commercial professionals.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreLeaseStructure {
    #[serde(rename = "nnn", alias = "triple_net")]
    Nnn,
    #[serde(rename = "modified_gross", alias = "mg")]
    ModifiedGross,
    #[serde(rename = "full_service_gross", alias = "gross")]
    FullServiceGross,
}

impl CreLeaseStructure {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Nnn => "NNN (Triple Net)",
            Self::ModifiedGross => "Modified Gross",
            Self::FullServiceGross => "Full-Service Gross",
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Nnn => "nnn",
            Self::ModifiedGross => "modified_gross",
            Self::FullServiceGross => "full_service_gross",
        }
    }

    pub fn parse_opt(raw: &str) -> Option<Self> {
        let s = raw.trim().to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        if s.contains("nnn") || s.contains("triple") || s == "net" {
            Some(Self::Nnn)
        } else if s.contains("mod") || s == "mg" || s.contains("industrial gross") {
            Some(Self::ModifiedGross)
        } else if s.contains("gross") || s == "fsg" || s.contains("full") {
            Some(Self::FullServiceGross)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreUnderwritingOverrides {
    #[serde(alias = "asset_class")]
    pub asset_class: Option<String>,
    #[serde(alias = "lease_structure", alias = "leaseType")]
    pub lease_structure: Option<String>,
    #[serde(alias = "purchase_price", alias = "askingPrice", alias = "price")]
    pub purchase_price: Option<f64>,
    #[serde(alias = "gross_sqft", alias = "sqft", alias = "rentableSqft")]
    pub gross_sqft: Option<f64>,
    #[serde(alias = "lot_sqft")]
    pub lot_sqft: Option<f64>,
    #[serde(alias = "unit_count")]
    pub units: Option<u32>,
    #[serde(alias = "year_built")]
    pub year_built: Option<i32>,
    pub state: Option<String>,
    #[serde(alias = "metro_tier")]
    pub metro_tier: Option<String>,
    #[serde(alias = "gross_potential_rent_annual", alias = "gpr")]
    pub gross_potential_rent_annual: Option<f64>,
    #[serde(alias = "rent_per_sqft_yr")]
    pub rent_per_sqft_yr: Option<f64>,
    #[serde(alias = "monthly_rent_per_unit")]
    pub monthly_rent_per_unit: Option<f64>,
    #[serde(alias = "other_income_annual")]
    pub other_income_annual: Option<f64>,
    #[serde(alias = "reimbursements_annual")]
    pub reimbursements_annual: Option<f64>,
    #[serde(alias = "vacancy_pct")]
    pub vacancy_pct: Option<f64>,
    #[serde(alias = "in_place_property_tax_annual")]
    pub in_place_property_tax_annual: Option<f64>,
    #[serde(alias = "property_tax_rate_pct")]
    pub property_tax_rate_pct: Option<f64>,
    #[serde(alias = "insurance_annual")]
    pub insurance_annual: Option<f64>,
    #[serde(alias = "management_pct")]
    pub management_pct: Option<f64>,
    #[serde(alias = "utilities_cam_annual")]
    pub utilities_cam_annual: Option<f64>,
    #[serde(alias = "repairs_maintenance_annual")]
    pub repairs_maintenance_annual: Option<f64>,
    #[serde(alias = "capex_reserve_per_unit_yr")]
    pub capex_reserve_per_unit_yr: Option<f64>,
    #[serde(alias = "capex_reserve_per_sqft_yr")]
    pub capex_reserve_per_sqft_yr: Option<f64>,
    #[serde(alias = "market_cap_rate_pct")]
    pub market_cap_rate_pct: Option<f64>,
    #[serde(alias = "conventional_rate_pct", alias = "interestRatePct")]
    pub conventional_rate_pct: Option<f64>,
    #[serde(alias = "conventional_ltv_pct", alias = "ltvPct")]
    pub conventional_ltv_pct: Option<f64>,
    #[serde(alias = "conventional_amort_years")]
    pub conventional_amort_years: Option<u32>,
    #[serde(alias = "conventional_term_years")]
    pub conventional_term_years: Option<u32>,
    #[serde(alias = "min_dscr")]
    pub min_dscr: Option<f64>,
    #[serde(alias = "sba_504_bank_rate_pct")]
    pub sba_504_bank_rate_pct: Option<f64>,
    #[serde(alias = "sba_504_cdc_rate_pct")]
    pub sba_504_cdc_rate_pct: Option<f64>,
    #[serde(alias = "sba_7a_rate_pct")]
    pub sba_7a_rate_pct: Option<f64>,
    #[serde(alias = "agency_rate_pct")]
    pub agency_rate_pct: Option<f64>,
    #[serde(alias = "owner_occupancy_pct")]
    pub owner_occupancy_pct: Option<f64>,
    #[serde(alias = "selected_loan_program")]
    pub selected_loan_program: Option<String>,
    #[serde(alias = "value_add_capex")]
    pub value_add_capex: Option<f64>,
    #[serde(alias = "stabilized_rent_upside_pct")]
    pub stabilized_rent_upside_pct: Option<f64>,
    #[serde(alias = "annual_rent_growth_pct")]
    pub annual_rent_growth_pct: Option<f64>,
    #[serde(alias = "annual_expense_growth_pct")]
    pub annual_expense_growth_pct: Option<f64>,
    #[serde(alias = "exit_cap_spread_bps")]
    pub exit_cap_spread_bps: Option<f64>,
    #[serde(alias = "closing_cost_pct")]
    pub closing_cost_pct: Option<f64>,
    #[serde(alias = "lot_acres", alias = "lotAcres")]
    pub acreage: Option<f64>,
    #[serde(alias = "farming_mode")]
    pub farming_mode: Option<String>,
    #[serde(alias = "allowable_far", alias = "far")]
    pub allowable_far: Option<f64>,
    #[serde(alias = "target_use")]
    pub target_use: Option<String>,
    #[serde(alias = "selected_concept", alias = "selectedConcept")]
    pub concept: Option<String>,
    #[serde(alias = "store_sqft")]
    pub store_sqft: Option<f64>,
    #[serde(alias = "annual_base_rent_psf", alias = "baseRentPsf")]
    pub annual_base_rent_psf: Option<f64>,
    #[serde(alias = "nnn_cam_psf", alias = "camPsf")]
    pub nnn_cam_psf: Option<f64>,
    #[serde(alias = "target_price_point")]
    pub target_price_point: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreUnderwritingInput {
    pub asset_class: CreAssetClass,
    pub lease_structure: CreLeaseStructure,
    pub purchase_price: f64,
    pub purchase_price_basis: String,
    pub gross_sqft: f64,
    pub lot_sqft: f64,
    pub units: u32,
    pub year_built: Option<i32>,
    pub state: String,
    pub metro_tier: String,
    pub gpr_annual: f64,
    pub gpr_basis: String,
    pub other_income_annual: f64,
    pub explicit_reimbursements_annual: Option<f64>,
    pub vacancy_pct: f64,
    pub in_place_tax_annual: Option<f64>,
    pub explicit_tax_rate_pct: Option<f64>,
    pub report_tax_rate_pct: Option<f64>,
    pub explicit_insurance_annual: Option<f64>,
    pub management_pct: f64,
    pub explicit_utilities_cam_annual: Option<f64>,
    pub explicit_repairs_maint_annual: Option<f64>,
    pub explicit_capex_per_unit_yr: Option<f64>,
    pub explicit_capex_per_sqft_yr: Option<f64>,
    pub explicit_market_cap_rate_pct: Option<f64>,
    pub mort30_benchmark_pct: Option<f64>,
    pub conventional_rate_pct: f64,
    pub conventional_ltv_pct: f64,
    pub conventional_amort_years: u32,
    pub conventional_term_years: u32,
    pub min_dscr: f64,
    pub sba_504_bank_rate_pct: f64,
    pub sba_504_cdc_rate_pct: f64,
    pub sba_7a_rate_pct: f64,
    pub agency_rate_pct: f64,
    pub owner_occupancy_pct: f64,
    pub selected_loan_program: Option<String>,
    pub value_add_capex: f64,
    pub stabilized_rent_upside_pct: f64,
    pub annual_rent_growth_pct: f64,
    pub annual_expense_growth_pct: f64,
    pub exit_cap_spread_bps: f64,
    pub closing_cost_pct: f64,
}

fn num_at(v: &Value, ptr: &str) -> Option<f64> {
    let target = v.pointer(ptr)?;
    target
        .as_f64()
        .or_else(|| target.as_i64().map(|i| i as f64))
        .filter(|x| x.is_finite())
}

fn infer_metro_tier(report: &Value, state: &str) -> String {
    let msa_name = report
        .pointer("/geo/msa/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let zhvi = num_at(report, "/modules/market/data/zhvi/latest").unwrap_or(0.0);
    let mhi = num_at(report, "/modules/market/data/acs/zip/medianHouseholdIncome")
        .or_else(|| {
            num_at(
                report,
                "/modules/market/data/acs/county/medianHouseholdIncome",
            )
        })
        .unwrap_or(0.0);

    let is_gateway = msa_name.contains("san jose")
        || msa_name.contains("san francisco")
        || msa_name.contains("los angeles")
        || msa_name.contains("san diego")
        || msa_name.contains("new york")
        || msa_name.contains("boston")
        || msa_name.contains("seattle")
        || msa_name.contains("washington")
        || msa_name.contains("miami")
        || zhvi >= 850_000.0
        || mhi >= 115_000.0;

    if is_gateway {
        return "Tier 1 (Gateway / High-Barrier)".to_string();
    }

    let is_tier2 = msa_name.contains("austin")
        || msa_name.contains("dallas")
        || msa_name.contains("houston")
        || msa_name.contains("denver")
        || msa_name.contains("phoenix")
        || msa_name.contains("atlanta")
        || msa_name.contains("charlotte")
        || msa_name.contains("raleigh")
        || msa_name.contains("nashville")
        || msa_name.contains("tampa")
        || msa_name.contains("orlando")
        || msa_name.contains("chicago")
        || msa_name.contains("portland")
        || matches!(state, "CA" | "WA" | "MA" | "CO" | "NJ" | "NY" | "FL" | "TX")
        || zhvi >= 450_000.0
        || mhi >= 80_000.0;

    if is_tier2 {
        "Tier 2 (Primary Growth Metro)".to_string()
    } else {
        "Tier 3 (Secondary / Yield Metro)".to_string()
    }
}

fn default_market_rent_per_sqft_yr(asset_class: CreAssetClass, metro_tier: &str) -> f64 {
    let tier_idx = if metro_tier.starts_with("Tier 1") {
        0
    } else if metro_tier.starts_with("Tier 2") {
        1
    } else {
        2
    };
    match (asset_class, tier_idx) {
        (CreAssetClass::Multifamily5Plus, 0) => 38.0,
        (CreAssetClass::Multifamily5Plus, 1) => 26.0,
        (CreAssetClass::Multifamily5Plus, _) => 19.5,
        (CreAssetClass::MixedUse, 0) => 40.0,
        (CreAssetClass::MixedUse, 1) => 28.5,
        (CreAssetClass::MixedUse, _) => 21.0,
        (CreAssetClass::Retail, 0) => 44.0,
        (CreAssetClass::Retail, 1) => 31.0,
        (CreAssetClass::Retail, _) => 22.5,
        (CreAssetClass::Office, 0) => 42.0,
        (CreAssetClass::Office, 1) => 30.0,
        (CreAssetClass::Office, _) => 22.0,
        (CreAssetClass::Industrial, 0) => 21.5,
        (CreAssetClass::Industrial, 1) => 15.0,
        (CreAssetClass::Industrial, _) => 10.5,
        (CreAssetClass::Hospitality, 0) => 52.0,
        (CreAssetClass::Hospitality, 1) => 38.0,
        (CreAssetClass::Hospitality, _) => 28.0,
        (CreAssetClass::CommercialLand, 0) => 9.5,
        (CreAssetClass::CommercialLand, 1) => 6.0,
        (CreAssetClass::CommercialLand, _) => 3.8,
        (CreAssetClass::AgriculturalFarmLand, 0) => 380.0 / 43_560.0,
        (CreAssetClass::AgriculturalFarmLand, 1) => 275.0 / 43_560.0,
        (CreAssetClass::AgriculturalFarmLand, _) => 185.0 / 43_560.0,
    }
}

pub fn benchmark_cap_rate_pct(
    asset_class: CreAssetClass,
    metro_tier: &str,
    mort30_opt: Option<f64>,
) -> f64 {
    let tier_idx = if metro_tier.starts_with("Tier 1") {
        0
    } else if metro_tier.starts_with("Tier 2") {
        1
    } else {
        2
    };
    let base = match (asset_class, tier_idx) {
        (CreAssetClass::AgriculturalFarmLand, 0) => 3.85,
        (CreAssetClass::AgriculturalFarmLand, 1) => 4.25,
        (CreAssetClass::AgriculturalFarmLand, _) => 4.65,
        (CreAssetClass::Multifamily5Plus, 0) => 5.25,
        (CreAssetClass::Multifamily5Plus, 1) => 5.75,
        (CreAssetClass::Multifamily5Plus, _) => 6.35,
        (CreAssetClass::Industrial, 0) => 5.55,
        (CreAssetClass::Industrial, 1) => 6.10,
        (CreAssetClass::Industrial, _) => 6.75,
        (CreAssetClass::MixedUse, 0) => 5.85,
        (CreAssetClass::MixedUse, 1) => 6.35,
        (CreAssetClass::MixedUse, _) => 7.00,
        (CreAssetClass::Retail, 0) => 6.15,
        (CreAssetClass::Retail, 1) => 6.65,
        (CreAssetClass::Retail, _) => 7.25,
        (CreAssetClass::CommercialLand, 0) => 6.50,
        (CreAssetClass::CommercialLand, 1) => 7.15,
        (CreAssetClass::CommercialLand, _) => 7.85,
        (CreAssetClass::Office, 0) => 7.25,
        (CreAssetClass::Office, 1) => 7.90,
        (CreAssetClass::Office, _) => 8.65,
        (CreAssetClass::Hospitality, 0) => 8.00,
        (CreAssetClass::Hospitality, 1) => 8.65,
        (CreAssetClass::Hospitality, _) => 9.25,
    };

    // Calibrate gently (+/- up to 50 bps) to prevailing long-term interest rate environment
    let rate_adj = mort30_opt
        .map(|m30| ((m30 - 6.50) * 0.25).clamp(-0.50, 0.60))
        .unwrap_or(0.0);
    round_n((base + rate_adj).clamp(3.75, 12.0), 2)
}

impl CreUnderwritingInput {
    pub fn from_report_and_overrides(
        report: &Value,
        asset_class: CreAssetClass,
        classification: &CreClassification,
        overrides: Option<&CreUnderwritingOverrides>,
    ) -> Self {
        let ov = overrides.cloned().unwrap_or_default();
        let facts = report.get("facts").unwrap_or(&Value::Null);

        let state = ov
            .state
            .as_deref()
            .or_else(|| report.pointer("/geo/state").and_then(|v| v.as_str()))
            .or_else(|| facts.get("state").and_then(|v| v.as_str()))
            .unwrap_or("CA")
            .trim()
            .to_ascii_uppercase();

        let metro_tier = ov
            .metro_tier
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| infer_metro_tier(report, &state));

        let units = ov
            .units
            .or(classification.detected_units)
            .unwrap_or_else(|| match asset_class {
                CreAssetClass::Multifamily5Plus => 8,
                CreAssetClass::MixedUse => 4,
                CreAssetClass::Hospitality => 24,
                _ => 1,
            })
            .max(1);

        let gross_sqft = ov
            .gross_sqft
            .filter(|&s| s > 0.0)
            .or_else(|| num_at(facts, "/grossSqft").filter(|&s| s > 0.0))
            .or_else(|| num_at(facts, "/sqft").filter(|&s| s > 0.0))
            .or_else(|| {
                num_at(report, "/modules/fengshui/data/house/areaSqft").filter(|&s| s > 0.0)
            })
            .unwrap_or_else(|| match asset_class {
                CreAssetClass::Multifamily5Plus => (units as f64) * 850.0,
                CreAssetClass::Industrial => 15_000.0,
                CreAssetClass::Retail => 6_500.0,
                CreAssetClass::Office => 7_500.0,
                CreAssetClass::MixedUse => 6_000.0,
                CreAssetClass::Hospitality => (units as f64) * 520.0,
                CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand => 0.0,
            });

        let lot_sqft = ov
            .lot_sqft
            .filter(|&s| s > 0.0)
            .or_else(|| ov.acreage.filter(|&a| a > 0.0).map(|a| a * 43_560.0))
            .or_else(|| num_at(facts, "/lotSqft").filter(|&s| s > 0.0))
            .unwrap_or_else(|| {
                if asset_class == CreAssetClass::AgriculturalFarmLand {
                    40.0 * 43_560.0
                } else if gross_sqft > 0.0 {
                    (gross_sqft * 2.4).max(5_000.0)
                } else {
                    21_780.0
                }
            });

        let year_built = ov.year_built.or_else(|| {
            facts
                .get("yearBuilt")
                .and_then(|v| v.as_i64())
                .map(|y| y as i32)
                .filter(|&y| (1750..=2100).contains(&y))
        });

        let lease_structure = ov
            .lease_structure
            .as_deref()
            .and_then(CreLeaseStructure::parse_opt)
            .unwrap_or_else(|| asset_class.default_lease_structure());

        let mort30 = num_at(report, "/modules/market/data/rates/mort30").filter(|&r| r > 1.5);
        let default_mkt_cap = benchmark_cap_rate_pct(asset_class, &metro_tier, mort30);

        // Calibrate Gross Potential Rent (GPR)
        let eff_area_for_rent = if asset_class == CreAssetClass::CommercialLand {
            lot_sqft.max(5_000.0)
        } else if asset_class == CreAssetClass::AgriculturalFarmLand {
            lot_sqft.max(43_560.0)
        } else {
            gross_sqft.max(1_000.0)
        };

        let (gpr_annual, gpr_basis) = if let Some(gpr) =
            ov.gross_potential_rent_annual.filter(|&x| x > 0.0)
        {
            (gpr, "Explicit Gross Potential Rent (GPR) input".to_string())
        } else if let Some(r_psf) = ov.rent_per_sqft_yr.filter(|&x| x > 0.0) {
            (
                r_psf * eff_area_for_rent,
                format!(
                    "Explicit base rent ${r_psf:.2}/sq ft/yr × {:.0} sq ft",
                    eff_area_for_rent
                ),
            )
        } else if let Some(r_unit) = ov.monthly_rent_per_unit.filter(|&x| x > 0.0) {
            (
                r_unit * (units as f64) * 12.0,
                format!("Explicit ${r_unit:.0}/unit/mo × {units} units × 12 mos"),
            )
        } else if asset_class == CreAssetClass::AgriculturalFarmLand {
            if let Some(cash_rent_total) = num_at(
                report,
                "/cre/landAndFarm/valuation/cashRentCapitalizedValue/grossAnnualCashRent",
            )
            .or_else(|| {
                num_at(
                    report,
                    "/landAndFarm/valuation/cashRentCapitalizedValue/grossAnnualCashRent",
                )
            })
            .filter(|&r| r > 0.0)
            {
                (
                    cash_rent_total,
                    "USDA NASS soil-adjusted farmland cash-lease benchmark".to_string(),
                )
            } else {
                let acres = (lot_sqft / 43_560.0).max(1.0);
                let per_acre = if matches!(state.as_str(), "CA" | "WA" | "OR" | "FL") {
                    360.0
                } else if matches!(
                    state.as_str(),
                    "IA" | "IL" | "IN" | "MN" | "NE" | "OH" | "WI" | "ID"
                ) {
                    275.0
                } else {
                    185.0
                };
                (
                    (per_acre * acres).round(),
                    format!("USDA NASS regional farmland cash rent (${per_acre:.0}/acre/yr × {acres:.1} acres)"),
                )
            }
        } else if asset_class == CreAssetClass::CommercialLand {
            if let Some(ground_rent) = num_at(
                report,
                "/cre/landAndFarm/groundLeaseValuation/annualGroundRent",
            )
            .or_else(|| num_at(report, "/landAndFarm/groundLeaseValuation/annualGroundRent"))
            .filter(|&r| r > 0.0)
            {
                (
                    ground_rent,
                    "Commercial pad/ground-lease benchmark rent".to_string(),
                )
            } else {
                let psf = default_market_rent_per_sqft_yr(asset_class, &metro_tier);
                (
                    psf * eff_area_for_rent,
                    format!(
                        "{metro_tier} {} benchmark (${psf:.2}/lot sq ft (ground lease)/yr)",
                        asset_class.label()
                    ),
                )
            }
        } else if asset_class == CreAssetClass::Multifamily5Plus {
            let zori = num_at(report, "/modules/market/data/rent/zori")
                .or_else(|| num_at(report, "/modules/market/data/rent/metroZori"))
                .or_else(|| num_at(report, "/modules/market/data/acs/zip/medianGrossRent"));
            if let Some(z) = zori.filter(|&v| v > 400.0) {
                let unit_rent = (z * 0.92).max(950.0);
                (
                    unit_rent * (units as f64) * 12.0,
                    format!(
                        "Submarket ZIP rent index / ACS apartment rent benchmark (${unit_rent:.0}/unit/mo × {units} units)"
                    ),
                )
            } else {
                let psf = default_market_rent_per_sqft_yr(asset_class, &metro_tier);
                (
                    psf * eff_area_for_rent,
                    format!("{metro_tier} multifamily market rent (${psf:.2}/sq ft/yr)"),
                )
            }
        } else if let Some(re) = num_at(facts, "/rentEstimate/rent")
            .or_else(|| num_at(facts, "/rentEstimate"))
            .filter(|&r| r > 500.0)
        {
            (
                re * 12.0,
                "Observed parcel monthly rent estimate annualized".to_string(),
            )
        } else {
            let psf = default_market_rent_per_sqft_yr(asset_class, &metro_tier);
            let area_lbl = if asset_class == CreAssetClass::CommercialLand {
                "lot sq ft (ground lease)"
            } else {
                "rentable sq ft"
            };
            (
                psf * eff_area_for_rent,
                format!(
                    "{metro_tier} {} benchmark (${psf:.2}/{area_lbl}/yr)",
                    asset_class.label()
                ),
            )
        };

        let is_unimproved_land_or_farm = matches!(
            asset_class,
            CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand
        );
        let land_farm_blended_val = num_at(report, "/cre/landAndFarm/blendedEstimate/value")
            .or_else(|| num_at(report, "/cre/landAndFarm/valuation/blendedEstimate/value"))
            .or_else(|| num_at(report, "/landAndFarm/blendedEstimate/value"))
            .or_else(|| num_at(report, "/landAndFarm/valuation/blendedEstimate/value"))
            .filter(|&p| p > 10_000.0);
        let res_avm_conf = report
            .pointer("/modules/valuation/data/estimate/confidence")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let res_avm = num_at(report, "/modules/valuation/data/estimate/value")
            .filter(|&p| p > 25_000.0)
            .filter(|_| {
                !is_unimproved_land_or_farm
                    || (gross_sqft > 0.0 && !res_avm_conf.eq_ignore_ascii_case("low"))
            });

        let (purchase_price, purchase_price_basis) = if let Some(pp) =
            ov.purchase_price.filter(|&p| p > 0.0)
        {
            (pp, "Explicit acquisition price input".to_string())
        } else if let Some(lp) = num_at(facts, "/listPrice")
            .or_else(|| num_at(facts, "/price"))
            .filter(|&p| p > 25_000.0)
        {
            (lp, "Active/asking list price".to_string())
        } else if let Some(lf_val) = land_farm_blended_val.filter(|_| is_unimproved_land_or_farm) {
            (
                lf_val,
                format!("{} blended land valuation", asset_class.label()),
            )
        } else if let Some(avm) = res_avm {
            (avm, "Report blended market valuation".to_string())
        } else if let Some(assessed) = num_at(facts, "/officialAssessment/assessedTotal")
            .or_else(|| num_at(facts, "/officialAssessment/totalValue"))
            .or_else(|| num_at(facts, "/assessedValue"))
            .filter(|&p| p > 25_000.0)
        {
            (assessed, "County official assessed value".to_string())
        } else if let Some(lf_val) = land_farm_blended_val {
            (
                lf_val,
                format!("{} blended land valuation", asset_class.label()),
            )
        } else {
            // Imply realistic acquisition price from estimated NOI & benchmark cap rate
            let est_noi_ratio = match lease_structure {
                CreLeaseStructure::Nnn => 0.84,
                CreLeaseStructure::ModifiedGross => 0.70,
                CreLeaseStructure::FullServiceGross => 0.60,
            };
            let implied = ((gpr_annual * est_noi_ratio) / (default_mkt_cap / 100.0))
                .max(if asset_class == CreAssetClass::AgriculturalFarmLand {
                    85_000.0
                } else {
                    250_000.0
                })
                .round();
            (
                implied,
                format!("Implied capitalized value at {default_mkt_cap:.2}% benchmark cap rate"),
            )
        };

        let other_income_default_pct = match asset_class {
            CreAssetClass::Multifamily5Plus => 0.04,
            CreAssetClass::MixedUse => 0.03,
            CreAssetClass::Hospitality => 0.08,
            CreAssetClass::Retail | CreAssetClass::Office => 0.015,
            CreAssetClass::Industrial => 0.01,
            CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand => 0.0,
        };
        let other_income_annual = ov
            .other_income_annual
            .filter(|&x| x >= 0.0)
            .unwrap_or_else(|| (gpr_annual * other_income_default_pct).round());

        let acs_vac = num_at(report, "/modules/market/data/acs/tract/vacancyPct")
            .or_else(|| num_at(report, "/modules/market/data/acs/zip/vacancyPct"));
        let vacancy_pct = ov
            .vacancy_pct
            .filter(|&v| (0.0..=60.0).contains(&v))
            .unwrap_or_else(|| {
                let base_v = asset_class.default_vacancy_pct();
                if asset_class == CreAssetClass::Multifamily5Plus {
                    if let Some(av) = acs_vac {
                        return round_n(
                            (base_v * 0.7 + av.clamp(2.5, 12.0) * 0.3).clamp(3.0, 10.0),
                            1,
                        );
                    }
                }
                base_v
            });

        let in_place_tax_annual = ov
            .in_place_property_tax_annual
            .filter(|&t| t >= 0.0)
            .or_else(|| num_at(facts, "/officialAssessment/annualTax").filter(|&t| t > 0.0))
            .or_else(|| {
                facts
                    .get("taxHistory")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| {
                        arr.iter()
                            .filter(|row| {
                                row.get("tax")
                                    .and_then(|t| t.as_f64())
                                    .map(|t| t > 0.0)
                                    .unwrap_or(false)
                            })
                            .max_by_key(|row| row.get("year").and_then(|y| y.as_i64()).unwrap_or(0))
                    })
                    .and_then(|row| row.get("tax").and_then(|v| v.as_f64()))
                    .filter(|&t| t > 0.0)
            })
            .or_else(|| num_at(facts, "/annualTax").filter(|&t| t > 0.0));

        let report_tax_rate_pct = num_at(facts, "/taxRate")
            .map(|r| if r < 0.15 { r * 100.0 } else { r })
            .or_else(|| num_at(facts, "/officialAssessment/statutoryTaxRatePct"))
            .or_else(|| num_at(facts, "/officialAssessment/effectiveTaxRatePct"))
            .or_else(|| {
                num_at(
                    report,
                    "/modules/market/data/govTaxBenchmark/effectiveTaxRatePct",
                )
            })
            .filter(|&r| (0.20..=6.0).contains(&r));

        let default_mgmt_pct = match asset_class {
            CreAssetClass::AgriculturalFarmLand => 3.0,
            CreAssetClass::Industrial | CreAssetClass::CommercialLand => 3.5,
            CreAssetClass::Retail | CreAssetClass::Office => 4.0,
            CreAssetClass::MixedUse => 4.5,
            CreAssetClass::Multifamily5Plus => {
                if units >= 24 {
                    4.0
                } else {
                    5.0
                }
            }
            CreAssetClass::Hospitality => 6.0,
        };
        let management_pct = ov
            .management_pct
            .filter(|&m| (0.0..=15.0).contains(&m))
            .unwrap_or(default_mgmt_pct);

        let base_conv_rate = ov
            .conventional_rate_pct
            .filter(|&r| (1.0..=20.0).contains(&r))
            .unwrap_or_else(|| {
                mort30
                    .map(|m| round_n((m + 0.35).clamp(5.25, 9.50), 2))
                    .unwrap_or(6.85)
            });

        let sba_504_bank_rate_pct = ov
            .sba_504_bank_rate_pct
            .filter(|&r| (1.0..=20.0).contains(&r))
            .unwrap_or(base_conv_rate);
        let sba_504_cdc_rate_pct = ov
            .sba_504_cdc_rate_pct
            .filter(|&r| (1.0..=20.0).contains(&r))
            .unwrap_or_else(|| round_n((base_conv_rate - 0.55).max(4.50), 2));
        let sba_7a_rate_pct = ov
            .sba_7a_rate_pct
            .filter(|&r| (1.0..=20.0).contains(&r))
            .unwrap_or_else(|| round_n(base_conv_rate + 0.50, 2));
        let agency_rate_pct = ov
            .agency_rate_pct
            .filter(|&r| (1.0..=20.0).contains(&r))
            .unwrap_or_else(|| round_n((base_conv_rate - 0.65).max(4.75), 2));

        Self {
            asset_class,
            lease_structure,
            purchase_price,
            purchase_price_basis,
            gross_sqft,
            lot_sqft,
            units,
            year_built,
            state,
            metro_tier,
            gpr_annual,
            gpr_basis,
            other_income_annual,
            explicit_reimbursements_annual: ov.reimbursements_annual.filter(|&x| x >= 0.0),
            vacancy_pct,
            in_place_tax_annual,
            explicit_tax_rate_pct: ov
                .property_tax_rate_pct
                .filter(|&r| (0.10..=8.0).contains(&r)),
            report_tax_rate_pct,
            explicit_insurance_annual: ov.insurance_annual.filter(|&x| x >= 0.0),
            management_pct,
            explicit_utilities_cam_annual: ov.utilities_cam_annual.filter(|&x| x >= 0.0),
            explicit_repairs_maint_annual: ov.repairs_maintenance_annual.filter(|&x| x >= 0.0),
            explicit_capex_per_unit_yr: ov.capex_reserve_per_unit_yr.filter(|&x| x >= 0.0),
            explicit_capex_per_sqft_yr: ov.capex_reserve_per_sqft_yr.filter(|&x| x >= 0.0),
            explicit_market_cap_rate_pct: ov
                .market_cap_rate_pct
                .filter(|&c| (2.0..=18.0).contains(&c)),
            mort30_benchmark_pct: mort30,
            conventional_rate_pct: base_conv_rate,
            conventional_ltv_pct: ov
                .conventional_ltv_pct
                .filter(|&l| (10.0..=85.0).contains(&l))
                .unwrap_or(65.0),
            conventional_amort_years: ov
                .conventional_amort_years
                .filter(|&y| (5..=40).contains(&y))
                .unwrap_or(25),
            conventional_term_years: ov
                .conventional_term_years
                .filter(|&y| (1..=30).contains(&y))
                .unwrap_or(10),
            min_dscr: ov
                .min_dscr
                .filter(|&d| (1.0..=2.5).contains(&d))
                .unwrap_or(1.25),
            sba_504_bank_rate_pct,
            sba_504_cdc_rate_pct,
            sba_7a_rate_pct,
            agency_rate_pct,
            owner_occupancy_pct: ov
                .owner_occupancy_pct
                .filter(|&o| (0.0..=100.0).contains(&o))
                .unwrap_or(0.0),
            selected_loan_program: ov.selected_loan_program,
            value_add_capex: ov.value_add_capex.filter(|&x| x >= 0.0).unwrap_or(0.0),
            stabilized_rent_upside_pct: ov
                .stabilized_rent_upside_pct
                .filter(|&x| (-20.0..=100.0).contains(&x))
                .unwrap_or(if ov.value_add_capex.unwrap_or(0.0) > 0.0 {
                    4.0
                } else {
                    0.0
                }),
            annual_rent_growth_pct: ov
                .annual_rent_growth_pct
                .filter(|&x| (-5.0..=15.0).contains(&x))
                .unwrap_or(3.0),
            annual_expense_growth_pct: ov
                .annual_expense_growth_pct
                .filter(|&x| (-5.0..=15.0).contains(&x))
                .unwrap_or(2.5),
            exit_cap_spread_bps: ov
                .exit_cap_spread_bps
                .filter(|&x| (-200.0..=300.0).contains(&x))
                .unwrap_or(25.0),
            closing_cost_pct: ov
                .closing_cost_pct
                .filter(|&x| (0.0..=8.0).contains(&x))
                .unwrap_or(1.75),
        }
    }
}

#[derive(Debug, Clone)]
struct TaxRegimeModel {
    regime_name: String,
    effective_post_sale_rate_pct: f64,
    in_place_annual: f64,
    post_acquisition_annual: f64,
    reassessment_shock_annual: f64,
    reassessment_shock_pct: f64,
    year1_supplemental_bill_est: f64,
    annual_tax_growth_cap_pct: Option<f64>,
    split_roll_risk_note: Option<String>,
    assessment_notes: String,
}

fn model_property_tax_reassessment(inp: &CreUnderwritingInput) -> TaxRegimeModel {
    let st = inp.state.as_str();
    let is_mf = inp.asset_class == CreAssetClass::Multifamily5Plus;

    let (regime_name, default_rate_pct, growth_cap, split_roll_note, notes) = match st {
        "CA" => (
            "California Proposition 13 (100% Acquisition Reassessment)".to_string(),
            inp.report_tax_rate_pct
                .map(|r| r.clamp(1.05, 1.85))
                .unwrap_or(1.18),
            Some(2.0),
            Some(if is_mf {
                "Residential/multifamily apartments have historically been exempted from California commercial split-roll proposals, though Prop 13 still triggers immediate 100% reassessment to acquisition price plus a Year-1 supplemental tax bill.".to_string()
            } else {
                "California Split-Roll Risk Note: Commercial & industrial parcels valued >$3M face recurring ballot/legislative proposals to strip Prop 13 acquisition-value protection and reassess to fair market value every 1–3 years. Underwrite NNN tax pass-through language carefully (verify no Prop 13 / change-of-ownership tax-protection carveout in tenant leases!).".to_string()
            }),
            "Reassessed to 100% of purchase price upon change of ownership (1.0% constitutional base + local voter bonds/direct levies), then capped at max 2.0%/yr statutory inflation.".to_string(),
        ),
        "TX" => (
            "Texas High-Millage Annual CAD Commercial Appraisal".to_string(),
            inp.report_tax_rate_pct
                .map(|r| r.clamp(1.75, 3.10))
                .unwrap_or(2.28),
            None,
            None,
            "Texas County Appraisal Districts (CADs) reappraise commercial & multifamily assets annually at 100% market value with zero homestead cap; aggressive post-sale equity/income protests should be budgeted annually.".to_string(),
        ),
        "FL" => (
            "Florida Non-Homestead 10% Cap Reset Upon Transfer".to_string(),
            inp.report_tax_rate_pct
                .map(|r| r.clamp(1.45, 2.45))
                .unwrap_or(1.88),
            Some(10.0),
            None,
            "Seller's 10% non-homestead annual assessment cap resets to full just/market value on January 1 following acquisition, often producing severe Year-1/Year-2 tax shock.".to_string(),
        ),
        "IL" => (
            "Illinois Commercial Classification Multiplier Regime".to_string(),
            inp.report_tax_rate_pct
                .map(|r| r.clamp(1.90, 4.20))
                .unwrap_or(if is_mf { 2.15 } else { 2.85 }),
            None,
            None,
            "Cook County / Illinois applies a higher statutory assessment level (25% commercial/industrial vs. 10% residential/apartment) amplified by the state equalization factor upon triennial reassessment.".to_string(),
        ),
        _ => (
            format!("{st} Post-Acquisition Commercial Assessment"),
            inp.report_tax_rate_pct
                .map(|r| (r * 1.05).clamp(0.65, 3.50))
                .unwrap_or(1.38),
            None,
            None,
            "Modeled post-closing assessment reset aligned to purchase price and local effective commercial millage.".to_string(),
        ),
    };

    let eff_rate_pct = inp.explicit_tax_rate_pct.unwrap_or(default_rate_pct);
    let post_acq = (inp.purchase_price * (eff_rate_pct / 100.0)).round();
    let in_place = inp
        .in_place_tax_annual
        .unwrap_or_else(|| (post_acq * 0.68).round());
    let shock_dollar = post_acq - in_place;
    let shock_pct = if in_place > 0.0 {
        round_n((shock_dollar / in_place) * 100.0, 1)
    } else {
        0.0
    };
    let supp_est = if st == "CA" && shock_dollar > 0.0 {
        (shock_dollar * 0.75).round()
    } else {
        0.0
    };

    TaxRegimeModel {
        regime_name,
        effective_post_sale_rate_pct: round_n(eff_rate_pct, 3),
        in_place_annual: in_place,
        post_acquisition_annual: post_acq,
        reassessment_shock_annual: shock_dollar,
        reassessment_shock_pct: shock_pct,
        year1_supplemental_bill_est: supp_est,
        annual_tax_growth_cap_pct: growth_cap,
        split_roll_risk_note: split_roll_note,
        assessment_notes: notes,
    }
}

#[derive(Debug, Clone)]
struct LeaseScenarioResult {
    structure: CreLeaseStructure,
    gpr_annual: f64,
    other_income_annual: f64,
    expense_reimbursements_annual: f64,
    vacancy_loss_annual: f64,
    effective_gross_income: f64,
    management_annual: f64,
    total_opex_annual: f64,
    opex_ex_reserves_annual: f64,
    noi: f64,
    noi_before_reserves: f64,
    expense_ratio_pct: f64,
    net_unrecovered_opex_annual: f64,
    equivalent_rent_per_sqft_yr_for_base_noi: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
fn compute_lease_scenario(
    structure: CreLeaseStructure,
    gpr_annual: f64,
    other_income_annual: f64,
    vacancy_pct: f64,
    post_acq_tax: f64,
    insurance: f64,
    utilities_cam: f64,
    repairs_maint: f64,
    capex_reserves: f64,
    management_pct: f64,
    explicit_reimb_opt: Option<f64>,
) -> LeaseScenarioResult {
    let vac_rate = (vacancy_pct / 100.0).clamp(0.0, 0.90);
    let occ_rate = 1.0 - vac_rate;

    // Vacancy & credit loss on scheduled rent + ancillary other income
    let gross_rent_and_other = gpr_annual + other_income_annual;
    let vacancy_loss_annual = (gross_rent_and_other * vac_rate).round();

    // Expense reimbursements depend on lease structure (with vacancy slippage on vacant suites)
    let reimb_annual = if let Some(exp_r) = explicit_reimb_opt {
        exp_r.round()
    } else {
        match structure {
            CreLeaseStructure::Nnn => {
                // Tenants reimburse pro-rata Taxes, Insurance, CAM/Utilities, and common R&M
                let reimb_pool = post_acq_tax + insurance + utilities_cam + (repairs_maint * 0.70);
                (reimb_pool * occ_rate).round()
            }
            CreLeaseStructure::ModifiedGross => {
                // Tenants reimburse CAM/Utilities share + base-year tax/insurance escalations (~45% pool)
                let reimb_pool = (utilities_cam * 0.80)
                    + ((post_acq_tax + insurance) * 0.25)
                    + (repairs_maint * 0.25);
                (reimb_pool * occ_rate).round()
            }
            CreLeaseStructure::FullServiceGross => 0.0,
        }
    };

    let egi = (gross_rent_and_other - vacancy_loss_annual + reimb_annual).max(0.0);
    let management_annual = (egi * (management_pct / 100.0)).round();
    let opex_ex_reserves =
        post_acq_tax + insurance + management_annual + utilities_cam + repairs_maint;
    let total_opex = opex_ex_reserves + capex_reserves;
    let noi = egi - total_opex;
    let noi_before_reserves = egi - opex_ex_reserves;
    let expense_ratio_pct = if egi > 0.0 {
        round_n((total_opex / egi) * 100.0, 2)
    } else {
        0.0
    };
    let net_unrecovered_opex = (total_opex - reimb_annual).max(0.0);

    LeaseScenarioResult {
        structure,
        gpr_annual,
        other_income_annual,
        expense_reimbursements_annual: reimb_annual,
        vacancy_loss_annual,
        effective_gross_income: egi,
        management_annual,
        total_opex_annual: total_opex,
        opex_ex_reserves_annual: opex_ex_reserves,
        noi,
        noi_before_reserves,
        expense_ratio_pct,
        net_unrecovered_opex_annual: net_unrecovered_opex,
        equivalent_rent_per_sqft_yr_for_base_noi: None,
    }
}

pub fn amortizing_monthly_payment(loan_amount: f64, annual_rate_pct: f64, amort_years: u32) -> f64 {
    if loan_amount <= 0.0 || amort_years == 0 {
        return 0.0;
    }
    let n = (amort_years * 12) as f64;
    let r = (annual_rate_pct / 100.0) / 12.0;
    if r <= 1e-9 {
        return loan_amount / n;
    }
    let factor = (1.0 + r).powf(n);
    (loan_amount * r * factor) / (factor - 1.0)
}

pub fn remaining_loan_balance(
    loan_amount: f64,
    annual_rate_pct: f64,
    amort_years: u32,
    elapsed_years: u32,
) -> f64 {
    if loan_amount <= 0.0 || elapsed_years >= amort_years {
        return 0.0;
    }
    let n = (amort_years * 12) as f64;
    let m = (elapsed_years * 12) as f64;
    let r = (annual_rate_pct / 100.0) / 12.0;
    if r <= 1e-9 {
        return (loan_amount * (1.0 - m / n)).max(0.0);
    }
    let fn_pow = (1.0 + r).powf(n);
    let fm_pow = (1.0 + r).powf(m);
    (loan_amount * (fn_pow - fm_pow) / (fn_pow - 1.0)).max(0.0)
}

fn npv_at_rate(cash_flows: &[f64], r: f64) -> f64 {
    let mut sum = 0.0;
    let base = 1.0 + r;
    if base <= 1e-6 {
        return f64::INFINITY;
    }
    for (t, &cf) in cash_flows.iter().enumerate() {
        sum += cf / base.powi(t as i32);
    }
    sum
}

/// Solves exact annualized Internal Rate of Return (IRR %) on periodic annual cash flows
/// using bracketed bisection + secant refinement.
pub fn solve_irr_pct(cash_flows: &[f64]) -> Option<f64> {
    if cash_flows.len() < 2 {
        return None;
    }
    let has_neg = cash_flows.iter().any(|&c| c < 0.0);
    let has_pos = cash_flows.iter().any(|&c| c > 0.0);
    if !has_neg || !has_pos {
        return None;
    }

    // Scan coarse grid to find sign-change bracket closest to realistic CRE returns
    let mut lo = -0.85_f64;
    let mut hi = 3.00_f64;
    let mut f_lo = npv_at_rate(cash_flows, lo);
    let mut bracket_found = false;

    let mut prev_r = lo;
    let mut prev_f = f_lo;
    for step in 1..=200 {
        let r = -0.85 + (step as f64) * 0.025;
        let f = npv_at_rate(cash_flows, r);
        if prev_f.is_finite() && f.is_finite() && prev_f.signum() != f.signum() {
            lo = prev_r;
            hi = r;
            f_lo = prev_f;
            bracket_found = true;
            break;
        }
        prev_r = r;
        prev_f = f;
    }

    if !bracket_found {
        return None;
    }

    for _ in 0..90 {
        let mid = 0.5 * (lo + hi);
        let f_mid = npv_at_rate(cash_flows, mid);
        if !f_mid.is_finite() {
            break;
        }
        if f_mid.abs() < 1e-6 || (hi - lo).abs() < 1e-9 {
            return Some(round_n(mid * 100.0, 2));
        }
        if f_lo.signum() == f_mid.signum() {
            lo = mid;
            f_lo = f_mid;
        } else {
            hi = mid;
        }
    }

    let root = 0.5 * (lo + hi);
    Some(round_n(root * 100.0, 2))
}

#[derive(Debug, Clone)]
struct HoldProjectionMetrics {
    five_yr_levered_irr_pct: Option<f64>,
    five_yr_equity_multiple: f64,
    ten_yr_levered_irr_pct: Option<f64>,
    ten_yr_equity_multiple: f64,
    five_yr_unlevered_irr_pct: Option<f64>,
    ten_yr_unlevered_irr_pct: Option<f64>,
    exit_cap_rate_pct: f64,
    year5_exit_valuation: f64,
    year10_exit_valuation: f64,
}

#[allow(clippy::too_many_arguments)]
fn compute_multi_year_hold(
    inp: &CreUnderwritingInput,
    base_egi: f64,
    base_tax: f64,
    base_non_tax_opex: f64,
    tax_growth_cap_opt: Option<f64>,
    exit_cap_rate_pct: f64,
    loan_tranches: &[(f64, f64, u32)], // (principal, rate_pct, amort_years)
    total_equity_invested: f64,
) -> HoldProjectionMetrics {
    let rent_g = inp.annual_rent_growth_pct / 100.0;
    let exp_g = inp.annual_expense_growth_pct / 100.0;
    let tax_g = match tax_growth_cap_opt {
        Some(cap) => exp_g.min(cap / 100.0),
        None => exp_g,
    };
    let annual_debt_service: f64 = loan_tranches
        .iter()
        .map(|&(p, r, yrs)| amortizing_monthly_payment(p, r, yrs) * 12.0)
        .sum();

    let stabilized_bump = if inp.value_add_capex > 0.0 || inp.stabilized_rent_upside_pct > 0.0 {
        inp.stabilized_rent_upside_pct / 100.0
    } else {
        0.0
    };

    // Compute NOI for Years 1..=11
    let mut noi_by_year = vec![0.0_f64; 12];
    for (yr, slot) in noi_by_year.iter_mut().enumerate().skip(1) {
        let yr_idx = (yr - 1) as i32;
        let bump = if yr >= 2 { stabilized_bump } else { 0.0 };
        let egi_t = base_egi * (1.0 + bump) * (1.0 + rent_g).powi(yr_idx);
        let tax_t = base_tax * (1.0 + tax_g).powi(yr_idx);
        let other_opex_t = base_non_tax_opex * (1.0 + exp_g).powi(yr_idx);
        *slot = egi_t - (tax_t + other_opex_t);
    }

    let disposition_cost_pct = 0.02; // 2.0% broker + transfer closing costs at exit
    let exit_cap_dec = (exit_cap_rate_pct / 100.0).max(0.03);
    let total_acquisition_cost =
        inp.purchase_price * (1.0 + inp.closing_cost_pct / 100.0) + inp.value_add_capex;

    let build_horizon = |horizon_yrs: usize| -> (Option<f64>, f64, Option<f64>, f64) {
        let forward_noi = noi_by_year[horizon_yrs + 1];
        let gross_exit_val = (forward_noi / exit_cap_dec).max(0.0);
        let net_sale_before_debt = gross_exit_val * (1.0 - disposition_cost_pct);
        let rem_balance: f64 = loan_tranches
            .iter()
            .map(|&(p, r, yrs)| remaining_loan_balance(p, r, yrs, horizon_yrs as u32))
            .sum();
        let net_equity_reversion = net_sale_before_debt - rem_balance;

        let mut lev_cfs = Vec::with_capacity(horizon_yrs + 1);
        let mut unlev_cfs = Vec::with_capacity(horizon_yrs + 1);
        lev_cfs.push(-total_equity_invested.max(1.0));
        unlev_cfs.push(-total_acquisition_cost.max(1.0));

        let mut pos_inflows = 0.0_f64;
        let mut neg_calls = 0.0_f64;
        for yr in 1..=horizon_yrs {
            let mut l_cf = noi_by_year[yr] - annual_debt_service;
            let mut u_cf = noi_by_year[yr];
            if yr == horizon_yrs {
                l_cf += net_equity_reversion;
                u_cf += net_sale_before_debt;
            }
            if l_cf > 0.0 {
                pos_inflows += l_cf;
            } else if l_cf < 0.0 {
                neg_calls += -l_cf;
            }
            lev_cfs.push(l_cf);
            unlev_cfs.push(u_cf);
        }

        let total_equity_calls = total_equity_invested + neg_calls;
        let em = if total_equity_calls > 0.0 {
            round_n(pos_inflows / total_equity_calls, 2)
        } else {
            0.0
        };
        (
            solve_irr_pct(&lev_cfs),
            em,
            solve_irr_pct(&unlev_cfs),
            gross_exit_val.round(),
        )
    };

    let (irr5, em5, uirr5, val5) = build_horizon(5);
    let (irr10, em10, uirr10, val10) = build_horizon(10);

    HoldProjectionMetrics {
        five_yr_levered_irr_pct: irr5,
        five_yr_equity_multiple: em5,
        ten_yr_levered_irr_pct: irr10,
        ten_yr_equity_multiple: em10,
        five_yr_unlevered_irr_pct: uirr5,
        ten_yr_unlevered_irr_pct: uirr10,
        exit_cap_rate_pct,
        year5_exit_valuation: val5,
        year10_exit_valuation: val10,
    }
}

#[derive(Debug, Clone)]
struct DebtProgramEvaluation {
    id: &'static str,
    label: &'static str,
    json: Value,
    loan_amount: f64,
    down_payment: f64,
    annual_debt_service: f64,
    dscr: f64,
    debt_yield_pct: f64,
    year1_cash_on_cash_pct: f64,
    tranches: Vec<(f64, f64, u32)>,
}

/// Executes deterministic institutional CRE underwriting across income, expenses, reassessment shock,
/// NNN/MG/Gross lease structures, cap-rate valuation matrix, and 4-way commercial debt stack.
pub fn run_cre_underwriting(inp: &CreUnderwritingInput) -> ModuleOutput {
    let mut warnings = Vec::new();
    let sources = vec![
        SourceItem::new(
            "U.S. Small Business Administration — SBA 504 & 7(a) Commercial Real Estate Loan Standards",
            "https://www.sba.gov/funding-programs/loans/504-loans",
        ),
        SourceItem::new(
            "Fannie Mae Multifamily DUS & Freddie Mac Small Balance Loan (SBL) Underwriting Guidelines",
            "https://multifamily.fanniemae.com/",
        ),
    ];

    let tax_model = model_property_tax_reassessment(inp);
    if tax_model.reassessment_shock_pct >= 25.0 {
        warnings.push(format!(
            "Property tax reassessment shock: modeled post-sale taxes (${:.0}/yr) are +{:.1}% (+${:.0}/yr) above in-place taxes (${:.0}/yr) under {}.",
            tax_model.post_acquisition_annual,
            tax_model.reassessment_shock_pct,
            tax_model.reassessment_shock_annual,
            tax_model.in_place_annual,
            tax_model.regime_name
        ));
    }

    let area_basis = if matches!(
        inp.asset_class,
        CreAssetClass::CommercialLand | CreAssetClass::AgriculturalFarmLand
    ) {
        inp.lot_sqft.max(1_000.0)
    } else {
        inp.gross_sqft.max(500.0)
    };
    let lot_acres = (inp.lot_sqft / 43_560.0).max(1.0);

    // Commercial Property & General Liability Insurance
    let cat_state_mult = match inp.state.as_str() {
        "FL" | "LA" | "TX" => 1.55,
        "CA" | "OK" | "CO" => 1.25,
        _ => 1.0,
    };
    let insurance_annual = inp.explicit_insurance_annual.unwrap_or_else(|| {
        let base_ins = match inp.asset_class {
            CreAssetClass::Multifamily5Plus => (inp.units as f64) * 780.0,
            CreAssetClass::Industrial => area_basis * 0.42,
            CreAssetClass::Retail => area_basis * 0.68,
            CreAssetClass::Office => area_basis * 0.58,
            CreAssetClass::MixedUse => area_basis * 0.72,
            CreAssetClass::Hospitality => (inp.units as f64) * 950.0,
            CreAssetClass::CommercialLand => 1_850.0,
            CreAssetClass::AgriculturalFarmLand => (lot_acres * 14.0).clamp(950.0, 12_000.0),
        };
        let min_ins = if inp.asset_class == CreAssetClass::AgriculturalFarmLand {
            950.0
        } else {
            1_500.0
        };
        (base_ins * cat_state_mult).max(min_ins).round()
    });

    // Utilities & Common Area Maintenance (CAM)
    let utilities_cam_annual =
        inp.explicit_utilities_cam_annual
            .unwrap_or_else(|| match inp.asset_class {
                CreAssetClass::Multifamily5Plus => ((inp.units as f64) * 1_150.0).round(),
                CreAssetClass::Industrial => (area_basis * 0.75).round(),
                CreAssetClass::Retail => (area_basis * 1.85).round(),
                CreAssetClass::Office => (area_basis * 2.65).round(),
                CreAssetClass::MixedUse => (area_basis * 1.95).round(),
                CreAssetClass::Hospitality => ((inp.units as f64) * 2_100.0).round(),
                CreAssetClass::CommercialLand => (area_basis * 0.08).max(900.0).round(),
                CreAssetClass::AgriculturalFarmLand => {
                    (lot_acres * 18.0).clamp(600.0, 15_000.0).round()
                }
            });

    // Repairs & Maintenance (vintage-calibrated)
    let age_factor = inp
        .year_built
        .map(|yb| {
            if yb < 1975 {
                1.25
            } else if yb < 1995 {
                1.10
            } else if yb >= 2012 {
                0.88
            } else {
                1.0
            }
        })
        .unwrap_or(1.0);

    let repairs_maint_annual = inp.explicit_repairs_maint_annual.unwrap_or_else(|| {
        let base_rm = match inp.asset_class {
            CreAssetClass::Multifamily5Plus => (inp.units as f64) * 850.0,
            CreAssetClass::Industrial => area_basis * 0.45,
            CreAssetClass::Retail => area_basis * 0.85,
            CreAssetClass::Office => area_basis * 1.15,
            CreAssetClass::MixedUse => area_basis * 1.05,
            CreAssetClass::Hospitality => (inp.units as f64) * 1_250.0,
            CreAssetClass::CommercialLand => 750.0,
            CreAssetClass::AgriculturalFarmLand => (lot_acres * 15.0).clamp(500.0, 12_000.0),
        };
        (base_rm * age_factor).round()
    });

    // Replacement CapEx Reserves ($/unit/yr or $/sqft/yr)
    let capex_reserves_annual = if let Some(per_u) = inp.explicit_capex_per_unit_yr {
        (per_u * (inp.units as f64)).round()
    } else if let Some(per_sf) = inp.explicit_capex_per_sqft_yr {
        (per_sf * area_basis).round()
    } else {
        match inp.asset_class {
            CreAssetClass::Multifamily5Plus => {
                let per_u = if inp.year_built.unwrap_or(1990) < 1980 {
                    375.0
                } else {
                    300.0
                };
                (per_u * (inp.units as f64)).round()
            }
            CreAssetClass::Industrial => (area_basis * 0.22).round(),
            CreAssetClass::Retail => (area_basis * 0.30).round(),
            CreAssetClass::Office => (area_basis * 0.40).round(),
            CreAssetClass::MixedUse => (area_basis * 0.35).round(),
            CreAssetClass::Hospitality => (inp.gpr_annual * 0.04).round(),
            CreAssetClass::CommercialLand => 500.0,
            CreAssetClass::AgriculturalFarmLand => {
                (lot_acres * 12.0).clamp(400.0, 10_000.0).round()
            }
        }
    };

    let capex_per_unit_yr = round_n(capex_reserves_annual / (inp.units.max(1) as f64), 2);
    let capex_per_sqft_yr = if inp.gross_sqft > 0.0 {
        round_n(capex_reserves_annual / inp.gross_sqft, 2)
    } else {
        0.0
    };

    // Compute all 3 lease structures (NNN, Modified Gross, Full-Service Gross)
    let mut nnn_scen = compute_lease_scenario(
        CreLeaseStructure::Nnn,
        inp.gpr_annual,
        inp.other_income_annual,
        inp.vacancy_pct,
        tax_model.post_acquisition_annual,
        insurance_annual,
        utilities_cam_annual,
        repairs_maint_annual,
        capex_reserves_annual,
        inp.management_pct,
        if inp.lease_structure == CreLeaseStructure::Nnn {
            inp.explicit_reimbursements_annual
        } else {
            None
        },
    );
    let mut mg_scen = compute_lease_scenario(
        CreLeaseStructure::ModifiedGross,
        inp.gpr_annual,
        inp.other_income_annual,
        inp.vacancy_pct,
        tax_model.post_acquisition_annual,
        insurance_annual,
        utilities_cam_annual,
        repairs_maint_annual,
        capex_reserves_annual,
        inp.management_pct,
        if inp.lease_structure == CreLeaseStructure::ModifiedGross {
            inp.explicit_reimbursements_annual
        } else {
            None
        },
    );
    let mut fsg_scen = compute_lease_scenario(
        CreLeaseStructure::FullServiceGross,
        inp.gpr_annual,
        inp.other_income_annual,
        inp.vacancy_pct,
        tax_model.post_acquisition_annual,
        insurance_annual,
        utilities_cam_annual,
        repairs_maint_annual,
        capex_reserves_annual,
        inp.management_pct,
        if inp.lease_structure == CreLeaseStructure::FullServiceGross {
            inp.explicit_reimbursements_annual
        } else {
            None
        },
    );

    let active_scen = match inp.lease_structure {
        CreLeaseStructure::Nnn => nnn_scen.clone(),
        CreLeaseStructure::ModifiedGross => mg_scen.clone(),
        CreLeaseStructure::FullServiceGross => fsg_scen.clone(),
    };

    // Compute market-equivalent $/sqft/yr quote required under each lease structure to deliver active NOI
    if inp.gross_sqft > 0.0 {
        let occ_mgmt_factor =
            (1.0 - (inp.vacancy_pct / 100.0).clamp(0.0, 0.85)) * (1.0 - inp.management_pct / 100.0);
        if occ_mgmt_factor > 0.1 {
            for sc in [&mut nnn_scen, &mut mg_scen, &mut fsg_scen] {
                let delta_noi_needed = active_scen.noi - sc.noi;
                let req_gpr = (inp.gpr_annual + delta_noi_needed / occ_mgmt_factor).max(0.0);
                sc.equivalent_rent_per_sqft_yr_for_base_noi =
                    Some(round_n(req_gpr / inp.gross_sqft, 2));
            }
        }
    }

    let scen_to_json = |s: &LeaseScenarioResult| -> Value {
        json!({
            "leaseStructure": s.structure.label(),
            "code": s.structure.code(),
            "grossPotentialRent": s.gpr_annual.round() as i64,
            "otherIncome": s.other_income_annual.round() as i64,
            "expenseReimbursements": s.expense_reimbursements_annual.round() as i64,
            "vacancyAndCreditLoss": s.vacancy_loss_annual.round() as i64,
            "effectiveGrossIncome": s.effective_gross_income.round() as i64,
            "totalOperatingExpenses": s.total_opex_annual.round() as i64,
            "netUnrecoveredOpex": s.net_unrecovered_opex_annual.round() as i64,
            "noiBeforeReserves": s.noi_before_reserves.round() as i64,
            "noi": s.noi.round() as i64,
            "expenseRatioPct": s.expense_ratio_pct,
            "equivalentRentPerSqftYrToMatchActiveNoi": s.equivalent_rent_per_sqft_yr_for_base_noi,
        })
    };

    // Cap Rate Valuation Matrix + 5-Point Sensitivity Grid (-100, -50, Base, +50, +100 bps)
    let mkt_cap_pct = inp.explicit_market_cap_rate_pct.unwrap_or_else(|| {
        benchmark_cap_rate_pct(inp.asset_class, &inp.metro_tier, inp.mort30_benchmark_pct)
    });

    let in_place_cap_rate_pct = if inp.purchase_price > 0.0 {
        round_n((active_scen.noi / inp.purchase_price) * 100.0, 2)
    } else {
        0.0
    };

    let sensitivity_bps: [i32; 5] = [-100, -50, 0, 50, 100];
    let mut sensitivity_table = Vec::with_capacity(5);
    for bps in sensitivity_bps {
        let cap = round_n((mkt_cap_pct + (bps as f64) / 100.0).max(2.0), 2);
        let implied_val = if active_scen.noi > 0.0 {
            (active_scen.noi / (cap / 100.0)).round()
        } else {
            0.0
        };
        let per_sqft = if inp.gross_sqft > 0.0 {
            Some(round_n(implied_val / inp.gross_sqft, 2))
        } else if inp.lot_sqft > 0.0 {
            Some(round_n(implied_val / inp.lot_sqft, 2))
        } else {
            None
        };
        let per_unit = if inp.units > 0 {
            Some((implied_val / (inp.units as f64)).round() as i64)
        } else {
            None
        };
        let vs_price_pct = if inp.purchase_price > 0.0 {
            round_n(
                ((implied_val - inp.purchase_price) / inp.purchase_price) * 100.0,
                1,
            )
        } else {
            0.0
        };
        let label = match bps {
            -100 => "-100 bps",
            -50 => "-50 bps",
            0 => "Base",
            50 => "+50 bps",
            100 => "+100 bps",
            _ => "Custom",
        };
        sensitivity_table.push(json!({
            "label": label,
            "spreadBps": bps,
            "capRatePct": cap,
            "impliedValue": implied_val as i64,
            "perSqft": per_sqft,
            "perUnit": per_unit,
            "deltaVsPurchasePricePct": vs_price_pct,
        }));
    }

    let closing_costs_dollar = (inp.purchase_price * (inp.closing_cost_pct / 100.0)).round();
    let exit_cap_pct = round_n(mkt_cap_pct + inp.exit_cap_spread_bps / 100.0, 2);
    let base_non_tax_opex = active_scen.total_opex_annual - tax_model.post_acquisition_annual;

    // -------------------------------------------------------------------------
    // Commercial Debt & Return Stack Comparison (4 Loan Programs Side-by-Side)
    // -------------------------------------------------------------------------
    let size_dscr_constrained = |target_ltv_pct: f64,
                                 min_dscr_req: f64,
                                 rate_pct: f64,
                                 amort_yrs: u32,
                                 statutory_cap: Option<f64>|
     -> (f64, f64, f64, &'static str, bool) {
        let max_by_ltv = (inp.purchase_price * (target_ltv_pct / 100.0)).round();
        let annual_const = amortizing_monthly_payment(1.0, rate_pct, amort_yrs) * 12.0;
        let max_by_dscr = if active_scen.noi > 0.0 && min_dscr_req > 0.0 && annual_const > 0.0 {
            ((active_scen.noi / min_dscr_req) / annual_const).floor()
        } else {
            0.0
        };
        let mut actual = max_by_ltv.min(max_by_dscr);
        let mut binding = if max_by_dscr < max_by_ltv {
            "DSCR"
        } else {
            "LTV"
        };
        if let Some(cap) = statutory_cap {
            if cap < actual {
                actual = cap;
                binding = "STATUTORY_MAX";
            }
        }
        let dscr_constrained = max_by_dscr < max_by_ltv;
        (
            actual.max(0.0),
            max_by_ltv,
            max_by_dscr.max(0.0),
            binding,
            dscr_constrained,
        )
    };

    // 1. Conventional CRE Bank / CMBS Loan (65% LTV, 25-yr amort, 5/7/10-yr balloon, min 1.25x DSCR)
    let conv_eval = {
        let (loan_amt, max_ltv, max_dscr, binding, dscr_capped) = size_dscr_constrained(
            inp.conventional_ltv_pct,
            inp.min_dscr,
            inp.conventional_rate_pct,
            inp.conventional_amort_years,
            None,
        );
        if dscr_capped {
            warnings.push(format!(
                "Debt sizing is DSCR-constrained on Conventional CRE loan: {:.2}x min DSCR caps max loan at ${:.0} ({:.1}% actual LTV vs. {:.1}% max LTV).",
                inp.min_dscr,
                loan_amt,
                if inp.purchase_price > 0.0 { (loan_amt / inp.purchase_price) * 100.0 } else { 0.0 },
                inp.conventional_ltv_pct
            ));
        }
        let down_pmt = (inp.purchase_price - loan_amt).max(0.0);
        let total_eq = down_pmt + closing_costs_dollar + inp.value_add_capex;
        let ads = amortizing_monthly_payment(
            loan_amt,
            inp.conventional_rate_pct,
            inp.conventional_amort_years,
        ) * 12.0;
        let dscr = if ads > 0.0 {
            round_n(active_scen.noi / ads, 2)
        } else {
            0.0
        };
        let dy = if loan_amt > 0.0 {
            round_n((active_scen.noi / loan_amt) * 100.0, 2)
        } else {
            0.0
        };
        let coc = if total_eq > 0.0 {
            round_n(((active_scen.noi - ads) / total_eq) * 100.0, 2)
        } else {
            0.0
        };
        let tranches = vec![(
            loan_amt,
            inp.conventional_rate_pct,
            inp.conventional_amort_years,
        )];
        let hold = compute_multi_year_hold(
            inp,
            active_scen.effective_gross_income,
            tax_model.post_acquisition_annual,
            base_non_tax_opex,
            tax_model.annual_tax_growth_cap_pct,
            exit_cap_pct,
            &tranches,
            total_eq,
        );
        let actual_ltv = if inp.purchase_price > 0.0 {
            round_n((loan_amt / inp.purchase_price) * 100.0, 1)
        } else {
            0.0
        };
        let balloon_bal = remaining_loan_balance(
            loan_amt,
            inp.conventional_rate_pct,
            inp.conventional_amort_years,
            inp.conventional_term_years,
        );

        DebtProgramEvaluation {
            id: "conventional_cre",
            label: "Conventional CRE Bank / CMBS Loan",
            json: json!({
                "id": "conventional_cre",
                "program": "Conventional CRE Bank / CMBS Loan",
                "eligible": true,
                "targetMaxLtvPct": inp.conventional_ltv_pct,
                "actualLtvPct": actual_ltv,
                "minRequiredDscr": inp.min_dscr,
                "maxLoanByLtv": max_ltv.round() as i64,
                "maxLoanByDscr": max_dscr.round() as i64,
                "bindingConstraint": binding,
                "dscrConstrained": dscr_capped,
                "loanAmount": loan_amt.round() as i64,
                "downPayment": down_pmt.round() as i64,
                "downPaymentPct": round_n(100.0 - actual_ltv, 1),
                "totalEquityRequired": total_eq.round() as i64,
                "interestRatePct": inp.conventional_rate_pct,
                "amortizationYears": inp.conventional_amort_years,
                "termYears": inp.conventional_term_years,
                "balloonOptionsYears": [5, 7, 10],
                "balloonBalanceAtMaturity": balloon_bal.round() as i64,
                "annualDebtService": ads.round() as i64,
                "monthlyDebtService": (ads / 12.0).round() as i64,
                "dscr": dscr,
                "debtYieldPct": dy,
                "year1CashFlowBeforeTax": (active_scen.noi - ads).round() as i64,
                "year1CashOnCashPct": coc,
                "fiveYearLeveredIrrPct": hold.five_yr_levered_irr_pct,
                "fiveYearEquityMultiple": hold.five_yr_equity_multiple,
                "tenYearLeveredIrrPct": hold.ten_yr_levered_irr_pct,
                "tenYearEquityMultiple": hold.ten_yr_equity_multiple,
            }),
            loan_amount: loan_amt,
            down_payment: down_pmt,
            annual_debt_service: ads,
            dscr,
            debt_yield_pct: dy,
            year1_cash_on_cash_pct: coc,
            tranches,
        }
    };

    // 2. SBA 504 Owner-User Commercial Loan (50% Bank 1st + 40% CDC Debenture + 10% Buyer Down)
    let sba504_eval = {
        let asset_compat = inp.asset_class.is_sba_owner_user_compatible();
        let owner_qualifies_now = asset_compat && inp.owner_occupancy_pct >= 51.0;
        let cdc_statutory_max = if inp.asset_class == CreAssetClass::Industrial {
            5_500_000.0
        } else {
            5_000_000.0
        };
        let bank_first = (inp.purchase_price * 0.50).round();
        let cdc_second = (inp.purchase_price * 0.40).min(cdc_statutory_max).round();
        let total_loan = bank_first + cdc_second;
        let down_pmt = (inp.purchase_price - total_loan).max(0.0).round();
        let down_pct = if inp.purchase_price > 0.0 {
            round_n((down_pmt / inp.purchase_price) * 100.0, 1)
        } else {
            10.0
        };
        let total_eq = down_pmt + closing_costs_dollar + inp.value_add_capex;
        let ads_bank = amortizing_monthly_payment(bank_first, inp.sba_504_bank_rate_pct, 25) * 12.0;
        let ads_cdc = amortizing_monthly_payment(cdc_second, inp.sba_504_cdc_rate_pct, 25) * 12.0;
        let ads = ads_bank + ads_cdc;
        let blended_rate = if total_loan > 0.0 {
            round_n(
                (bank_first * inp.sba_504_bank_rate_pct + cdc_second * inp.sba_504_cdc_rate_pct)
                    / total_loan,
                2,
            )
        } else {
            inp.sba_504_bank_rate_pct
        };
        let dscr = if ads > 0.0 {
            round_n(active_scen.noi / ads, 2)
        } else {
            0.0
        };
        let dy = if total_loan > 0.0 {
            round_n((active_scen.noi / total_loan) * 100.0, 2)
        } else {
            0.0
        };
        let coc = if total_eq > 0.0 {
            round_n(((active_scen.noi - ads) / total_eq) * 100.0, 2)
        } else {
            0.0
        };
        let tranches = vec![
            (bank_first, inp.sba_504_bank_rate_pct, 25),
            (cdc_second, inp.sba_504_cdc_rate_pct, 25),
        ];
        let hold = compute_multi_year_hold(
            inp,
            active_scen.effective_gross_income,
            tax_model.post_acquisition_annual,
            base_non_tax_opex,
            tax_model.annual_tax_growth_cap_pct,
            exit_cap_pct,
            &tranches,
            total_eq,
        );

        DebtProgramEvaluation {
            id: "sba_504",
            label: "SBA 504 Owner-User Commercial Loan (50/40/10)",
            json: json!({
                "id": "sba_504",
                "program": "SBA 504 Owner-User Commercial Loan (50/40/10)",
                "eligibleAssetClass": asset_compat,
                "ownerOccupiedQualifiesNow": owner_qualifies_now,
                "requiredOwnerOccupancyPct": 51.0,
                "currentOwnerOccupancyPct": inp.owner_occupancy_pct,
                "eligibilityNote": if !asset_compat {
                    "Passive multifamily/land investments are ineligible for SBA 504; requires an operating business occupying >=51% of an existing commercial building."
                } else if owner_qualifies_now {
                    "Qualifies for 10%-down SBA 504 financing (owner occupies >=51% of building; operating business cash flow covers debt service)."
                } else {
                    "Available with 10% buyer down payment if buyer's operating business occupies >=51% of rentable sq ft (currently modeled as passive investor occupancy)."
                },
                "bankFirstLienLoan": bank_first as i64,
                "bankFirstLienPct": 50.0,
                "bankFirstRatePct": inp.sba_504_bank_rate_pct,
                "cdcDebentureSecondLoan": cdc_second as i64,
                "cdcDebenturePct": round_n(100.0 - 50.0 - down_pct, 1),
                "cdcDebentureRatePct": inp.sba_504_cdc_rate_pct,
                "cdcStatutoryCap": cdc_statutory_max as i64,
                "loanAmount": total_loan.round() as i64,
                "actualLtvPct": round_n(100.0 - down_pct, 1),
                "downPayment": down_pmt as i64,
                "downPaymentPct": down_pct,
                "totalEquityRequired": total_eq.round() as i64,
                "blendedInterestRatePct": blended_rate,
                "amortizationYears": 25,
                "annualDebtService": ads.round() as i64,
                "monthlyDebtService": (ads / 12.0).round() as i64,
                "dscr": dscr,
                "debtYieldPct": dy,
                "year1CashFlowBeforeTax": (active_scen.noi - ads).round() as i64,
                "year1CashOnCashPct": coc,
                "fiveYearLeveredIrrPct": hold.five_yr_levered_irr_pct,
                "fiveYearEquityMultiple": hold.five_yr_equity_multiple,
                "tenYearLeveredIrrPct": hold.ten_yr_levered_irr_pct,
                "tenYearEquityMultiple": hold.ten_yr_equity_multiple,
            }),
            loan_amount: total_loan,
            down_payment: down_pmt,
            annual_debt_service: ads,
            dscr,
            debt_yield_pct: dy,
            year1_cash_on_cash_pct: coc,
            tranches,
        }
    };

    // 3. SBA 7(a) Commercial Real Estate Loan (up to $5.0M statutory cap, 85% target LTV, 25-yr amort)
    let sba7a_eval = {
        let asset_compat = inp.asset_class.is_sba_owner_user_compatible();
        let owner_qualifies_now = asset_compat && inp.owner_occupancy_pct >= 51.0;
        let statutory_max = 5_000_000.0_f64;
        let target_7a = (inp.purchase_price * 0.85).min(statutory_max).round();
        let capped_by_5m = inp.purchase_price * 0.85 > statutory_max;
        let down_pmt = (inp.purchase_price - target_7a).max(0.0);
        let actual_ltv = if inp.purchase_price > 0.0 {
            round_n((target_7a / inp.purchase_price) * 100.0, 1)
        } else {
            0.0
        };
        let total_eq = down_pmt + closing_costs_dollar + inp.value_add_capex;
        let ads = amortizing_monthly_payment(target_7a, inp.sba_7a_rate_pct, 25) * 12.0;
        let dscr = if ads > 0.0 {
            round_n(active_scen.noi / ads, 2)
        } else {
            0.0
        };
        let dy = if target_7a > 0.0 {
            round_n((active_scen.noi / target_7a) * 100.0, 2)
        } else {
            0.0
        };
        let coc = if total_eq > 0.0 {
            round_n(((active_scen.noi - ads) / total_eq) * 100.0, 2)
        } else {
            0.0
        };
        let tranches = vec![(target_7a, inp.sba_7a_rate_pct, 25)];
        let hold = compute_multi_year_hold(
            inp,
            active_scen.effective_gross_income,
            tax_model.post_acquisition_annual,
            base_non_tax_opex,
            tax_model.annual_tax_growth_cap_pct,
            exit_cap_pct,
            &tranches,
            total_eq,
        );

        DebtProgramEvaluation {
            id: "sba_7a",
            label: "SBA 7(a) Commercial Loan (Up to $5M)",
            json: json!({
                "id": "sba_7a",
                "program": "SBA 7(a) Commercial Loan (Up to $5M)",
                "eligibleAssetClass": asset_compat,
                "eligibleWithin5MCap": asset_compat && !capped_by_5m,
                "ownerOccupiedQualifiesNow": owner_qualifies_now && inp.purchase_price <= 6_500_000.0,
                "requiredOwnerOccupancyPct": 51.0,
                "statutoryMaxLoan": statutory_max as i64,
                "cappedAtStatutoryMax": capped_by_5m,
                "loanAmount": target_7a as i64,
                "actualLtvPct": actual_ltv,
                "downPayment": down_pmt.round() as i64,
                "downPaymentPct": round_n(100.0 - actual_ltv, 1),
                "totalEquityRequired": total_eq.round() as i64,
                "interestRatePct": inp.sba_7a_rate_pct,
                "amortizationYears": 25,
                "prepaymentNote": "3-year declining SBA 7(a) prepayment penalty only (5% Yr 1, 3% Yr 2, 1% Yr 3, 0% Yr 4+).",
                "annualDebtService": ads.round() as i64,
                "monthlyDebtService": (ads / 12.0).round() as i64,
                "dscr": dscr,
                "debtYieldPct": dy,
                "year1CashFlowBeforeTax": (active_scen.noi - ads).round() as i64,
                "year1CashOnCashPct": coc,
                "fiveYearLeveredIrrPct": hold.five_yr_levered_irr_pct,
                "fiveYearEquityMultiple": hold.five_yr_equity_multiple,
                "tenYearLeveredIrrPct": hold.ten_yr_levered_irr_pct,
                "tenYearEquityMultiple": hold.ten_yr_equity_multiple,
            }),
            loan_amount: target_7a,
            down_payment: down_pmt,
            annual_debt_service: ads,
            dscr,
            debt_yield_pct: dy,
            year1_cash_on_cash_pct: coc,
            tranches,
        }
    };

    // 4. Agency Multifamily (Fannie Mae DUS / Freddie Mac SBL — 5+ Units, 30-yr amort, 75% LTV, 1.25x DSCR)
    let agency_eval = {
        let eligible = inp.asset_class.is_agency_multifamily_eligible(inp.units);
        let (loan_amt, max_ltv, max_dscr, binding, dscr_capped) =
            size_dscr_constrained(75.0, 1.25, inp.agency_rate_pct, 30, None);
        let down_pmt = (inp.purchase_price - loan_amt).max(0.0);
        let actual_ltv = if inp.purchase_price > 0.0 {
            round_n((loan_amt / inp.purchase_price) * 100.0, 1)
        } else {
            0.0
        };
        let total_eq = down_pmt + closing_costs_dollar + inp.value_add_capex;
        let ads = amortizing_monthly_payment(loan_amt, inp.agency_rate_pct, 30) * 12.0;
        let dscr = if ads > 0.0 {
            round_n(active_scen.noi / ads, 2)
        } else {
            0.0
        };
        let dy = if loan_amt > 0.0 {
            round_n((active_scen.noi / loan_amt) * 100.0, 2)
        } else {
            0.0
        };
        let coc = if total_eq > 0.0 {
            round_n(((active_scen.noi - ads) / total_eq) * 100.0, 2)
        } else {
            0.0
        };
        let tranches = vec![(loan_amt, inp.agency_rate_pct, 30)];
        let hold = compute_multi_year_hold(
            inp,
            active_scen.effective_gross_income,
            tax_model.post_acquisition_annual,
            base_non_tax_opex,
            tax_model.annual_tax_growth_cap_pct,
            exit_cap_pct,
            &tranches,
            total_eq,
        );

        let sub_program = if loan_amt <= 7_500_000.0 {
            "Freddie Mac SBL / Fannie Mae Small Loan ($1M–$7.5M)"
        } else {
            "Fannie Mae DUS / Freddie Mac Conventional Multifamily"
        };

        DebtProgramEvaluation {
            id: "agency_multifamily",
            label: "Agency Multifamily (Fannie Mae DUS / Freddie Mac SBL)",
            json: json!({
                "id": "agency_multifamily",
                "program": "Agency Multifamily (Fannie Mae DUS / Freddie Mac SBL)",
                "subProgram": sub_program,
                "eligible": eligible,
                "eligibilityNote": if eligible {
                    "Eligible 5+ unit multifamily property — qualifies for non-recourse 30-year Agency amortization."
                } else {
                    "Requires 5+ residential apartment units (commercial retail/office/industrial parcels use Conventional/CMBS or SBA 504/7a)."
                },
                "targetMaxLtvPct": 75.0,
                "actualLtvPct": actual_ltv,
                "minRequiredDscr": 1.25,
                "maxLoanByLtv": max_ltv.round() as i64,
                "maxLoanByDscr": max_dscr.round() as i64,
                "bindingConstraint": binding,
                "dscrConstrained": dscr_capped,
                "loanAmount": loan_amt.round() as i64,
                "downPayment": down_pmt.round() as i64,
                "downPaymentPct": round_n(100.0 - actual_ltv, 1),
                "totalEquityRequired": total_eq.round() as i64,
                "interestRatePct": inp.agency_rate_pct,
                "amortizationYears": 30,
                "termYears": 10,
                "nonRecourse": true,
                "annualDebtService": ads.round() as i64,
                "monthlyDebtService": (ads / 12.0).round() as i64,
                "dscr": dscr,
                "debtYieldPct": dy,
                "year1CashFlowBeforeTax": (active_scen.noi - ads).round() as i64,
                "year1CashOnCashPct": coc,
                "fiveYearLeveredIrrPct": hold.five_yr_levered_irr_pct,
                "fiveYearEquityMultiple": hold.five_yr_equity_multiple,
                "tenYearLeveredIrrPct": hold.ten_yr_levered_irr_pct,
                "tenYearEquityMultiple": hold.ten_yr_equity_multiple,
            }),
            loan_amount: loan_amt,
            down_payment: down_pmt,
            annual_debt_service: ads,
            dscr,
            debt_yield_pct: dy,
            year1_cash_on_cash_pct: coc,
            tranches,
        }
    };

    // Select primary debt program for headline Investor Metrics
    let primary_debt = if let Some(ref sel) = inp.selected_loan_program {
        let s = sel.to_ascii_lowercase();
        if s.contains("504") {
            &sba504_eval
        } else if s.contains("7a") || s.contains("7(a)") {
            &sba7a_eval
        } else if s.contains("agency") || s.contains("fannie") || s.contains("freddie") {
            &agency_eval
        } else {
            &conv_eval
        }
    } else if inp.owner_occupancy_pct >= 51.0 && inp.asset_class.is_sba_owner_user_compatible() {
        &sba504_eval
    } else if inp.asset_class == CreAssetClass::Multifamily5Plus && inp.units >= 5 {
        &agency_eval
    } else {
        &conv_eval
    };

    let primary_total_equity =
        primary_debt.down_payment + closing_costs_dollar + inp.value_add_capex;
    let primary_hold = compute_multi_year_hold(
        inp,
        active_scen.effective_gross_income,
        tax_model.post_acquisition_annual,
        base_non_tax_opex,
        tax_model.annual_tax_growth_cap_pct,
        exit_cap_pct,
        &primary_debt.tranches,
        primary_total_equity,
    );

    // Stabilized Yield-on-Cost & Break-Even Occupancy %
    let total_cost_basis = inp.purchase_price + closing_costs_dollar + inp.value_add_capex;
    let stabilized_egi =
        active_scen.effective_gross_income * (1.0 + inp.stabilized_rent_upside_pct / 100.0);
    let stabilized_mgmt = stabilized_egi * (inp.management_pct / 100.0);
    let stabilized_opex =
        (active_scen.total_opex_annual - active_scen.management_annual) + stabilized_mgmt;
    let stabilized_noi = stabilized_egi - stabilized_opex;
    let stabilized_yield_on_cost_pct = if total_cost_basis > 0.0 {
        round_n((stabilized_noi / total_cost_basis) * 100.0, 2)
    } else {
        0.0
    };

    // Gross Potential Total Revenue = GPR + Other Income + Full-Occupancy Reimbursements
    let occ_factor = (1.0 - (inp.vacancy_pct / 100.0).clamp(0.0, 0.85)).max(0.15);
    let full_occ_reimb = active_scen.expense_reimbursements_annual / occ_factor;
    let gross_potential_total_revenue = inp.gpr_annual + inp.other_income_annual + full_occ_reimb;
    let break_even_occupancy_pct = if gross_potential_total_revenue > 0.0 {
        round_n(
            ((active_scen.total_opex_annual + primary_debt.annual_debt_service)
                / gross_potential_total_revenue)
                * 100.0,
            1,
        )
    } else {
        0.0
    };

    let price_per_sqft = if inp.gross_sqft > 0.0 {
        Some(round_n(inp.purchase_price / inp.gross_sqft, 2))
    } else if inp.lot_sqft > 0.0 {
        Some(round_n(inp.purchase_price / inp.lot_sqft, 2))
    } else {
        None
    };
    let price_per_unit = if inp.units > 0 {
        Some((inp.purchase_price / (inp.units as f64)).round() as i64)
    } else {
        None
    };

    let data = json!({
        "basis": {
            "assetClass": inp.asset_class.label(),
            "assetClassCode": inp.asset_class.code(),
            "activeLeaseStructure": inp.lease_structure.label(),
            "activeLeaseStructureCode": inp.lease_structure.code(),
            "purchasePrice": inp.purchase_price.round() as i64,
            "purchasePriceBasis": inp.purchase_price_basis,
            "closingCosts": closing_costs_dollar as i64,
            "closingCostPct": inp.closing_cost_pct,
            "valueAddCapex": inp.value_add_capex.round() as i64,
            "totalProjectCost": total_cost_basis.round() as i64,
            "grossSqft": inp.gross_sqft.round() as i64,
            "lotSqft": inp.lot_sqft.round() as i64,
            "units": inp.units,
            "pricePerSqft": price_per_sqft,
            "pricePerUnit": price_per_unit,
            "state": inp.state,
            "metroTier": inp.metro_tier,
            "ownerOccupancyPct": inp.owner_occupancy_pct,
        },
        "income": {
            "grossPotentialRent": active_scen.gpr_annual.round() as i64,
            "gprBasis": inp.gpr_basis,
            "rentPerSqftYr": if inp.gross_sqft > 0.0 { Some(round_n(active_scen.gpr_annual / inp.gross_sqft, 2)) } else { None },
            "monthlyRentPerUnit": if inp.units > 0 { Some((active_scen.gpr_annual / (inp.units as f64) / 12.0).round() as i64) } else { None },
            "otherIncome": active_scen.other_income_annual.round() as i64,
            "expenseReimbursements": active_scen.expense_reimbursements_annual.round() as i64,
            "vacancyAndCreditLossPct": inp.vacancy_pct,
            "vacancyAndCreditLoss": active_scen.vacancy_loss_annual.round() as i64,
            "effectiveGrossIncome": active_scen.effective_gross_income.round() as i64,
        },
        "operatingExpenses": {
            "propertyTaxes": {
                "regime": tax_model.regime_name,
                "effectivePostSaleRatePct": tax_model.effective_post_sale_rate_pct,
                "inPlaceAnnual": tax_model.in_place_annual.round() as i64,
                "postAcquisitionAnnual": tax_model.post_acquisition_annual.round() as i64,
                "reassessmentShockAnnual": tax_model.reassessment_shock_annual.round() as i64,
                "reassessmentShockPct": tax_model.reassessment_shock_pct,
                "year1SupplementalBillEstimate": tax_model.year1_supplemental_bill_est.round() as i64,
                "annualGrowthCapPct": tax_model.annual_tax_growth_cap_pct,
                "splitRollRiskNote": tax_model.split_roll_risk_note,
                "notes": tax_model.assessment_notes,
            },
            "insurance": {
                "annual": insurance_annual.round() as i64,
                "perSqftYr": if inp.gross_sqft > 0.0 { Some(round_n(insurance_annual / inp.gross_sqft, 2)) } else { None },
                "perUnitYr": (insurance_annual / (inp.units.max(1) as f64)).round() as i64,
            },
            "propertyManagement": {
                "pctOfEgi": inp.management_pct,
                "annual": active_scen.management_annual.round() as i64,
            },
            "utilitiesAndCam": {
                "annual": utilities_cam_annual.round() as i64,
                "perSqftYr": if inp.gross_sqft > 0.0 { Some(round_n(utilities_cam_annual / inp.gross_sqft, 2)) } else { None },
            },
            "repairsAndMaintenance": {
                "annual": repairs_maint_annual.round() as i64,
                "perSqftYr": if inp.gross_sqft > 0.0 { Some(round_n(repairs_maint_annual / inp.gross_sqft, 2)) } else { None },
                "perUnitYr": (repairs_maint_annual / (inp.units.max(1) as f64)).round() as i64,
            },
            "replacementCapexReserves": {
                "annual": capex_reserves_annual.round() as i64,
                "perUnitYr": capex_per_unit_yr,
                "perSqftYr": capex_per_sqft_yr,
            },
            "totalBeforeReservesAnnual": active_scen.opex_ex_reserves_annual.round() as i64,
            "totalAnnual": active_scen.total_opex_annual.round() as i64,
            "expenseRatioPct": active_scen.expense_ratio_pct,
        },
        "netOperatingIncome": {
            "noiBeforeReserves": active_scen.noi_before_reserves.round() as i64,
            "noi": active_scen.noi.round() as i64,
            "stabilizedNoi": stabilized_noi.round() as i64,
            "noiPerSqft": if inp.gross_sqft > 0.0 { Some(round_n(active_scen.noi / inp.gross_sqft, 2)) } else { None },
            "noiPerUnit": (active_scen.noi / (inp.units.max(1) as f64)).round() as i64,
            "expenseRatioPct": active_scen.expense_ratio_pct,
        },
        "leaseStructureComparison": {
            "active": inp.lease_structure.code(),
            "nnn": scen_to_json(&nnn_scen),
            "modifiedGross": scen_to_json(&mg_scen),
            "fullServiceGross": scen_to_json(&fsg_scen),
        },
        "capRateMatrix": {
            "benchmarkCapRatePct": mkt_cap_pct,
            "inPlaceCapRatePct": in_place_cap_rate_pct,
            "spreadVsBenchmarkBps": ((in_place_cap_rate_pct - mkt_cap_pct) * 100.0).round() as i64,
            "sensitivityTable": sensitivity_table,
        },
        "debtComparison": {
            "selectedProgramId": primary_debt.id,
            "selectedProgramLabel": primary_debt.label,
            "conventionalCre": conv_eval.json.clone(),
            "sba504": sba504_eval.json.clone(),
            "sba7a": sba7a_eval.json.clone(),
            "agencyMultifamily": agency_eval.json.clone(),
            "programs": [
                conv_eval.json,
                sba504_eval.json,
                sba7a_eval.json,
                agency_eval.json,
            ],
        },
        "investorMetrics": {
            "selectedDebtProgram": primary_debt.label,
            "inPlaceCapRatePct": in_place_cap_rate_pct,
            "stabilizedYieldOnCostPct": stabilized_yield_on_cost_pct,
            "dscr": primary_debt.dscr,
            "debtYieldPct": primary_debt.debt_yield_pct,
            "loanAmount": primary_debt.loan_amount.round() as i64,
            "totalEquityInvested": primary_total_equity.round() as i64,
            "annualDebtService": primary_debt.annual_debt_service.round() as i64,
            "year1BeforeTaxCashFlow": (active_scen.noi - primary_debt.annual_debt_service).round() as i64,
            "year1CashOnCashPct": primary_debt.year1_cash_on_cash_pct,
            "breakEvenOccupancyPct": break_even_occupancy_pct,
            "fiveYearLeveredIrrPct": primary_hold.five_yr_levered_irr_pct,
            "fiveYearEquityMultiple": primary_hold.five_yr_equity_multiple,
            "tenYearLeveredIrrPct": primary_hold.ten_yr_levered_irr_pct,
            "tenYearEquityMultiple": primary_hold.ten_yr_equity_multiple,
            "fiveYearUnleveredIrrPct": primary_hold.five_yr_unlevered_irr_pct,
            "tenYearUnleveredIrrPct": primary_hold.ten_yr_unlevered_irr_pct,
            "exitCapRatePct": primary_hold.exit_cap_rate_pct,
            "year5ExitValuation": primary_hold.year5_exit_valuation as i64,
            "year10ExitValuation": primary_hold.year10_exit_valuation as i64,
        },
        "disclaimer": CRE_UNDERWRITING_DISCLAIMER,
    });

    ModuleOutput {
        data,
        sources,
        warnings,
    }
}
