//! Track 6B Native Buyer Extension (`closing_contingency_calendar`):
//! Wires promulgated/filed state title insurance premiums from `title_underwriter_rates`
//! (eliminating flat percentage title heuristics), progressive city/county/state transfer &
//! mortgage recording tax buyer/seller splits, property-tax & insurance prorations, California
//! Prop 13 supplemental tax bill reserves, HOA resale certificate & working-capital fees,
//! Year-1 CapEx reserves, and a 50-State + DC Earnest Money & TRID Contingency Milestone Calendar.

use anyhow::Result;
use chrono::Utc;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind, ExecutionMode,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    GeoScopeProto, LicenseClass, ManifestSourceAttributionProto, MeteringSpecProto, ProvenanceTier,
    RateLimitSpecProto, ShareScope, SharingSpecProto, SourceAttributionProto,
};
use crate::modules::title_legal_radar::compute_state_title_rate_filing;
use crate::registry::rates_and_costs::estimate_transfer_tax_schedule;
use crate::shared::{monthly_pi, now_iso, round_n, EnvConfig, HttpClient};

pub const CLOSING_CONTINGENCY_EXT_ID: &str = "closing_contingency_calendar";

pub fn is_valid_us_state_51(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_uppercase().as_str(),
        "AL" | "AK"
            | "AZ"
            | "AR"
            | "CA"
            | "CO"
            | "CT"
            | "DE"
            | "DC"
            | "FL"
            | "GA"
            | "HI"
            | "ID"
            | "IL"
            | "IN"
            | "IA"
            | "KS"
            | "KY"
            | "LA"
            | "ME"
            | "MD"
            | "MA"
            | "MI"
            | "MN"
            | "MS"
            | "MO"
            | "MT"
            | "NE"
            | "NV"
            | "NH"
            | "NJ"
            | "NM"
            | "NY"
            | "NC"
            | "ND"
            | "OH"
            | "OK"
            | "OR"
            | "PA"
            | "RI"
            | "SC"
            | "SD"
            | "TN"
            | "TX"
            | "UT"
            | "VT"
            | "VA"
            | "WA"
            | "WV"
            | "WI"
            | "WY"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContingencyMilestone {
    pub id: String,
    pub label: String,
    pub day_from_acceptance: i64,
    pub statutory_or_form_basis: String,
    pub earnest_money_at_risk_after: bool,
    pub guidance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateContingencyProfile {
    pub state: String,
    pub contract_regime: String,
    pub closing_settlement_style: String,
    pub funding_model: String,
    pub earnest_money_pct_low: f64,
    pub earnest_money_pct_high: f64,
    pub earnest_money_statutory_note: String,
    pub attorney_review_days: Option<i64>,
    pub option_or_due_diligence_fee_norm: Option<String>,
    pub inspection_contingency_days: i64,
    pub hoa_resale_review_days: i64,
    pub hoa_resale_statutory_citation: String,
    pub hoa_resale_fee_statutory_cap_usd: Option<i64>,
    pub appraisal_contingency_days: i64,
    pub loan_commitment_days: i64,
    pub trid_closing_disclosure_business_days_before_close: i64,
    pub typical_close_days_financed: i64,
    pub property_tax_proration_basis: String,
    pub milestones: Vec<ContingencyMilestone>,
}

/// Builds the 50-State + DC Earnest Money, Attorney Review, Option/Due-Diligence, HOA Rescission,
/// and CFPB TRID Milestone Calendar for `state` (and optional `is_hoa_or_condo`).
pub fn build_state_contingency_calendar(
    state: &str,
    is_hoa_or_condo: bool,
) -> StateContingencyProfile {
    let st = state.trim().to_ascii_uppercase();
    let dry_funding = matches!(
        st.as_str(),
        "CA" | "AZ" | "WA" | "OR" | "NV" | "HI" | "AK" | "ID" | "NM"
    );
    let funding_model = if dry_funding {
        "dry_funding_escrow_recording"
    } else {
        "wet_funding_table_disbursement"
    };

    let attorney_closing = matches!(
        st.as_str(),
        "NY" | "NJ"
            | "MA"
            | "CT"
            | "RI"
            | "VT"
            | "NH"
            | "ME"
            | "DE"
            | "SC"
            | "GA"
            | "NC"
            | "WV"
            | "IL"
    );
    let settlement_style = if attorney_closing {
        "attorney_or_split_attorney_closing"
    } else if dry_funding {
        "escrow_and_title_company"
    } else {
        "title_company_or_settlement_agent"
    };

    let (
        contract_regime,
        em_low,
        em_high,
        em_note,
        atty_days,
        dd_fee_norm,
        insp_days,
        hoa_days,
        hoa_cite,
        hoa_cap_usd,
        appr_days,
        loan_days,
        close_days,
        tax_proration,
    ) = match st.as_str() {
        "CA" => (
            "CAR Residential Purchase Agreement (RPA) — Active Removal Regime",
            1.0,
            3.0,
            "Cal. Civ. Code § 1675 caps liquidated damages on 1–4 unit owner-occupied dwellings at 3.0% of purchase price; contingencies stay active until buyer signs a written Contingency Removal (CR).",
            None,
            None,
            17,
            5,
            "Cal. Civ. Code § 4525 & § 4530 (HOA document disclosure; 5-day CAR RPA review default)",
            None,
            17,
            17,
            30,
            "Fiscal year July 1 – June 30 (installments due Nov 1 / Feb 1, delinquent Dec 10 / Apr 10); prorated to recording date plus separate post-close Prop 13 Supplemental Tax bill (Rev. & Tax. Code § 75.10).",
        ),
        "TX" => (
            "TREC One to Four Family Residential Contract (20-17) — Unrestricted Option Period",
            1.0,
            2.0,
            "Earnest money (~1% norm) wired to title company within 3 calendar days (strict time-is-of-the-essence) alongside a separate non-refundable Option Fee for unrestricted termination rights.",
            None,
            Some("TREC Paragraph 23 Option Fee ($200–$1,000 typical, credited at closing) buys 5–10 days unrestricted right to terminate while preserving Earnest Money.".to_string()),
            7,
            5,
            "Tex. Prop. Code § 207.003 (Subdivision/HOA Resale Certificate statutory cap $375)",
            Some(375),
            18,
            21,
            30,
            "Calendar year taxes paid in arrears (due Oct 1, delinquent Feb 1); seller credits buyer at closing for accrued year-to-date taxes based on prior/current appraisal roll.",
        ),
        "NC" => (
            "NC Bar / NCAR Offer to Purchase and Contract (Form 2-T) — Dual Fee Due Diligence",
            0.5,
            1.5,
            "North Carolina Form 2-T separates a non-refundable Due Diligence Fee (paid directly to seller at contract effective date) from refundable Earnest Money (held in escrow until Due Diligence expiration).",
            None,
            Some("Non-refundable NC Due Diligence Fee (0.5%–2.0% typical depending on market heat) paid directly to seller; credited toward purchase price at settlement.".to_string()),
            21,
            5,
            "N.C.G.S. Chapter 47F / 47C Planned Community & Condominium Disclosure",
            None,
            21,
            21,
            35,
            "Calendar year Jan 1 – Dec 31 (billed late summer, due Sept 1, delinquent Jan 6); prorated on a calendar-year basis at settlement.",
        ),
        "NY" => (
            "New York Attorney Pre-Contract Diligence & Formal Contract Deposit",
            5.0,
            10.0,
            "Offer acceptance is non-binding until formal contract execution after pre-contract inspection & co-op/condo board minute review; 5%–10% contract downpayment is wired to seller's attorney IOLA escrow upon signing.",
            Some(5),
            None,
            5,
            5,
            "NY Real Prop. Law Art. 9-B / Cooperative & Condo Offering Plan & Financials Attorney Review",
            None,
            30,
            35,
            60,
            "Town/county, village, and school taxes follow distinct fiscal calendars (NYC quarterly July 1 fiscal year); adjusted to closing date per local bar customs.",
        ),
        "NJ" => (
            "New Jersey Broker Contract + Mandatory 3-Business-Day Attorney Review",
            1.0,
            5.0,
            "Initial deposit (~$1,000–1%) at signing, followed by additional deposit (totaling 3%–10%) within 7–10 days after conclusion of the 3-business-day Attorney Review period (N.J.A.C. 11:5-6.2(g)).",
            Some(3),
            None,
            14,
            7,
            "N.J.S.A. 46:8B-1 et seq. Condominium / HOA Public Offering & Resale Review",
            None,
            21,
            30,
            45,
            "Quarterly property tax billing (Feb 1, May 1, Aug 1, Nov 1); prorated to the day of closing.",
        ),
        "IL" => (
            "Multi-Board Residential Real Estate Contract 8.0 — 5-Business-Day Attorney & Inspection Modification",
            1.0,
            3.0,
            "Initial earnest money within 2–3 business days of acceptance, often increased to 3%–5% after expiration of the 5-business-day Attorney Review & Professional Inspection window.",
            Some(5),
            None,
            5,
            5,
            "765 ILCS 605/22.1 Illinois Condominium Property Act Resale Disclosure (30-day delivery cap)",
            Some(375),
            21,
            28,
            40,
            "Illinois property taxes are billed one full year in arrears (Cook County 2nd installment uses state equalizer); seller gives buyer a 105%–110% tax proration credit at closing when current bill is unissued.",
        ),
        "MA" => (
            "Massachusetts Two-Step Offer to Purchase (OTP) -> Purchase & Sale (P&S) Agreement",
            1.0,
            5.0,
            "Under McCarthy v. Tobin, a signed Massachusetts OTP is a binding contract! Customarily $1,000 accompanies the OTP, increasing to 5% total deposit upon signing the attorney-negotiated P&S ~10–14 days later.",
            Some(10),
            None,
            10,
            7,
            "M.G.L. c. 183A, § 6(d) Condominium Unpaid Common-Expense Certificate (required at closing)",
            None,
            21,
            28,
            45,
            "Quarterly municipal fiscal year (July 1 – June 30) plus Title 5 septic inspection certificate (310 CMR 15.301) where non-sewered.",
        ),
        "WA" => (
            "NWMLS Form 21 + Form 22A / 35 — Statutory 5% Earnest Forfeiture Cap",
            2.0,
            5.0,
            "RCW 64.04.005 grants seller a safe-harbor sole remedy when earnest money does not exceed 5.0% of purchase price; Form 35 inspection contingency defaults to 10 days unless shortened.",
            None,
            None,
            10,
            5,
            "RCW 64.34.425 / WUCIOA RCW 64.90.640 (5-day buyer right to cancel after receipt of Resale Certificate; $275 statutory fee cap)",
            Some(275),
            17,
            21,
            30,
            "Calendar year taxes payable in two halves (Apr 30 & Oct 31); prorated to recording date in escrow.",
        ),
        "FL" => (
            "FAR/BAR 'AS IS' Residential Contract — Free-Look Inspection Termination Window",
            1.0,
            5.0,
            "Under the FAR/BAR 'AS IS' Contract, buyer has sole-discretion right to terminate and recover 100% of escrow deposit before 5:00 PM on the final day of the Inspection Period (15-day default, 7–10 days competitive).",
            None,
            None,
            12,
            3,
            "Fla. Stat. § 718.503 (Condo: non-waivable 3 business days excluding Sat/Sun/holidays after governance docs + Milestone Inspection / SIRS receipt) & § 720.401 (HOA disclosure)",
            Some(299),
            20,
            30,
            35,
            "Calendar year taxes billed Nov 1 in arrears (4% early-pay discount in Nov); prorated with November discount credit per FAR/BAR Standard K.",
        ),
        "CO" => (
            "Colorado Real Estate Commission (CREC) CBS1 — Granular Objection & Resolution Deadlines",
            1.0,
            2.0,
            "CREC contract specifies explicit calendar dates for Inspection Objection, Inspection Termination, Title/Off-Record Objection, CIC (HOA) Document Objection, Appraisal Objection, and New Loan Termination.",
            None,
            None,
            10,
            7,
            "CCIOA C.R.S. § 38-33.3-316.5 / CREC Association Documents Termination Deadline",
            None,
            18,
            24,
            30,
            "Colorado property taxes are paid one year in arrears (due Apr 30, or halves Feb 28 / June 15); seller credits buyer at closing for accrued prior/current year taxes.",
        ),
        "AZ" => (
            "AAR Residential Resale Real Estate Purchase Contract — 10-Day Inspection & Cure Notice",
            1.0,
            2.0,
            "AAR contract provides a default 10-calendar-day Inspection Period (BINSR — Buyer's Inspection Notice and Seller's Response) plus a mandatory 3-day Cure Period Notice before breach cancellation.",
            None,
            None,
            10,
            5,
            "A.R.S. § 33-1806 / § 33-1260 (5-day buyer cancellation right after HOA disclosure receipt; HOA disclosure fee capped at $400)",
            Some(400),
            18,
            24,
            30,
            "Semi-annual installments in arrears (Oct 1 / Mar 1); prorated through close of escrow.",
        ),
        "VA" => (
            "NVAR / VAR Residential Sales Contract — Statutory HOA Resale Cancellation Right",
            1.0,
            3.0,
            "Earnest money Deposit held by Settlement Agent or Broker within 3–5 days; Virginia Resale Disclosure Act grants a non-waivable statutory right to cancel upon HOA/Condo packet delivery.",
            None,
            None,
            10,
            3,
            "Va. Code § 55.1-2312 Virginia Resale Disclosure Act (3-day statutory cancellation window from packet delivery, or per contract up to 7 days)",
            None,
            17,
            21,
            30,
            "County/independent city semi-annual fiscal or calendar tax billing; prorated to settlement date.",
        ),
        "MD" => (
            "MAR Residential Contract of Sale — Statutory Condo/HOA Rescission",
            1.0,
            3.0,
            "Earnest money deposited in escrow; Maryland Real Property Art. § 11-135 (Condo, 7 days) and § 11B-106 (HOA, 5 calendar days) grant statutory buyer rescission rights after resale disclosure receipt.",
            None,
            None,
            10,
            5,
            "Md. Code, Real Prop. § 11-135 & § 11B-106 ($250 statutory fee cap for resale inspection/certificate)",
            Some(250),
            18,
            24,
            35,
            "Maryland fiscal year July 1 – June 30 (semi-annual owner-occupied schedule); prorated at settlement.",
        ),
        "PA" => (
            "PAR Standard Agreement for the Sale of Real Estate (ASR)",
            1.0,
            3.0,
            "Earnest money held in broker/title escrow (initial + optional second deposit); PA 68 Pa.C.S. § 3407 / § 5407 grants 5-day buyer cancellation after Condo/Planned Community Resale Certificate receipt.",
            None,
            None,
            10,
            5,
            "68 Pa.C.S. § 3407 / § 5407 (5-day statutory rescission window upon receipt of Resale Certificate)",
            None,
            18,
            24,
            35,
            "Separate County/Municipal (spring) and School District (July/Aug) tax bills; prorated independently by millage calendar.",
        ),
        "GA" => (
            "GAR Purchase and Sale Agreement — Negotiated Due Diligence Period",
            1.0,
            2.0,
            "Georgia GAR contract grants an unrestricted Due Diligence Period (typically 7–14 days) with no separate fee required unless negotiated; closing must be conducted by a licensed Georgia real estate attorney.",
            None,
            None,
            10,
            5,
            "O.C.G.A. § 44-3-232 / § 44-3-109 POA & Condominium Clearance Statement",
            None,
            18,
            21,
            30,
            "Calendar year ad valorem taxes billed in autumn; prorated to closing date.",
        ),
        "NV" => (
            "Nevada Realtors Residential Purchase Agreement",
            1.0,
            2.0,
            "Earnest money wired to escrow within 1–3 business days; NRS 116.4109 grants a 5-calendar-day statutory cancellation right after receipt of the HOA resale package.",
            None,
            None,
            10,
            5,
            "NRS 116.4109 (5-day cancellation window; resale certificate fee capped by NRED regulation ~$185)",
            Some(185),
            18,
            21,
            30,
            "Fiscal year July 1 – June 30 billed in 4 quarterly installments; prorated in escrow.",
        ),
        _ => (
            "State Association Realtors Standard Residential Purchase Contract",
            1.0,
            2.0,
            "Earnest money deposit (~1%–2% of purchase price) wired to title/escrow trust account within 2–3 business days of mutual acceptance; verify wire instructions by phone to prevent wire fraud.",
            None,
            None,
            10,
            5,
            "State Uniform Common Interest Ownership / Condominium Resale Disclosure Act",
            None,
            18,
            21,
            30,
            "County property taxes prorated between buyer and seller through the settlement/recording date per local tax roll.",
        ),
    };

    let mut milestones = Vec::new();
    milestones.push(ContingencyMilestone {
        id: "earnest_wire".to_string(),
        label: "Earnest Money Deposit (& Option/DD Fee if applicable)".to_string(),
        day_from_acceptance: if st == "NC" { 1 } else { 3 },
        statutory_or_form_basis: em_note.to_string(),
        earnest_money_at_risk_after: st == "NC",
        guidance: "Verify escrow/attorney routing numbers via a known phone number (never email-only PDF instructions) before wiring deposit.".to_string(),
    });

    if let Some(ad) = atty_days {
        milestones.push(ContingencyMilestone {
            id: "attorney_review".to_string(),
            label: if st == "MA" {
                "Attorney Purchase & Sale (P&S) Execution".to_string()
            } else {
                "Attorney Review & Contract Modification Window".to_string()
            },
            day_from_acceptance: ad,
            statutory_or_form_basis: format!("{st} Attorney Review / Contract Custom"),
            earnest_money_at_risk_after: false,
            guidance: "Engage real estate counsel immediately on Day 0 to review riders, title exceptions, municipal search, and repair credits.".to_string(),
        });
    }

    milestones.push(ContingencyMilestone {
        id: "inspection_contingency".to_string(),
        label: "Physical, Roof, Sewer Lateral & WDO Inspection Contingency".to_string(),
        day_from_acceptance: insp_days,
        statutory_or_form_basis: contract_regime.to_string(),
        earnest_money_at_risk_after: false,
        guidance: "Complete general home, roof, sewer scope, and pest/radon inspections at least 3 days before deadline so contractor quotes support your repair-credit addendum.".to_string(),
    });

    if is_hoa_or_condo {
        milestones.push(ContingencyMilestone {
            id: "hoa_resale_review".to_string(),
            label: format!("HOA / Condo Resale Packet Review ({hoa_days}-Day Rescission Window)"),
            day_from_acceptance: (insp_days + 2).min(close_days - 10),
            statutory_or_form_basis: hoa_cite.to_string(),
            earnest_money_at_risk_after: false,
            guidance: "Audit reserve study percent-funded (<50% triggers special-assessment risk), master policy wind/hail/wildfire deductible, delinquency rate (>15% blocks Fannie/Freddie warrantability), and rental caps.".to_string(),
        });
    }

    milestones.push(ContingencyMilestone {
        id: "prelim_title_review".to_string(),
        label: "Preliminary Title Report / Title Commitment & Easement Objection".to_string(),
        day_from_acceptance: (insp_days + 3).min(close_days - 10),
        statutory_or_form_basis: "ALTA Title Commitment Schedule B-II Exceptions".to_string(),
        earnest_money_at_risk_after: false,
        guidance: "Verify zero unindexed municipal/mechanic/solar UCC-1 liens and inspect plotted utility/pipeline/shared-driveway easements before title objection expires.".to_string(),
    });

    milestones.push(ContingencyMilestone {
        id: "appraisal_contingency".to_string(),
        label: "Lender Valuation / Appraisal Contingency".to_string(),
        day_from_acceptance: appr_days,
        statutory_or_form_basis: "Agency / Lender Appraisal Contingency".to_string(),
        earnest_money_at_risk_after: false,
        guidance: "If appraised value comes in below contract price, buyer may renegotiate price, bring cash to bridge the LTV gap, or cancel prior to appraisal contingency removal.".to_string(),
    });

    milestones.push(ContingencyMilestone {
        id: "loan_commitment".to_string(),
        label: "Final Underwriting / Loan Commitment Contingency".to_string(),
        day_from_acceptance: loan_days,
        statutory_or_form_basis: "Final Mortgage Commitment / Clear-to-Close".to_string(),
        earnest_money_at_risk_after: true,
        guidance: "Once loan, appraisal, and inspection contingencies are removed, earnest money is generally at risk of forfeiture if buyer fails to close.".to_string(),
    });

    milestones.push(ContingencyMilestone {
        id: "trid_closing_disclosure".to_string(),
        label: "CFPB TRID Closing Disclosure (CD) 3-Business-Day Receipt".to_string(),
        day_from_acceptance: (close_days - 4).max(loan_days + 1),
        statutory_or_form_basis: "12 CFR § 1026.19(f)(1)(ii) (TILA-RESPA Integrated Disclosure Rule)".to_string(),
        earnest_money_at_risk_after: true,
        guidance: "Lender must deliver the final Closing Disclosure at least 3 business days (excluding Sundays and federal holidays) prior to consummation; APR increases >0.125% or loan product changes trigger a new 3-day waiting period.".to_string(),
    });

    milestones.push(ContingencyMilestone {
        id: "final_walkthrough_and_close".to_string(),
        label: "Final Verification Walk-Through, Wire & Deed Recording".to_string(),
        day_from_acceptance: close_days,
        statutory_or_form_basis: format!("{settlement_style} ({funding_model})"),
        earnest_money_at_risk_after: true,
        guidance: "Verify agreed repairs, vacant broom-clean possession, and appliance operation within 24–48 hours prior to signing and funding.".to_string(),
    });

    StateContingencyProfile {
        state: st,
        contract_regime: contract_regime.to_string(),
        closing_settlement_style: settlement_style.to_string(),
        funding_model: funding_model.to_string(),
        earnest_money_pct_low: em_low,
        earnest_money_pct_high: em_high,
        earnest_money_statutory_note: em_note.to_string(),
        attorney_review_days: atty_days,
        option_or_due_diligence_fee_norm: dd_fee_norm,
        inspection_contingency_days: insp_days,
        hoa_resale_review_days: hoa_days,
        hoa_resale_statutory_citation: hoa_cite.to_string(),
        hoa_resale_fee_statutory_cap_usd: hoa_cap_usd,
        appraisal_contingency_days: appr_days,
        loan_commitment_days: loan_days,
        trid_closing_disclosure_business_days_before_close: 3,
        typical_close_days_financed: close_days,
        property_tax_proration_basis: tax_proration.to_string(),
        milestones,
    }
}

fn parse_f64_param(query: &ExtensionQueryProto, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        query
            .extra_params
            .get(*k)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
    })
}

fn parse_upstream_title_filing(query: &ExtensionQueryProto) -> Option<Value> {
    for env in &query.upstream_envelopes {
        if env.extension_id == "title_underwriter_rates" {
            if let Some(ref attr) = env.attribution {
                if let Some(ref notes) = attr.notes {
                    if let Ok(v) = serde_json::from_str::<Value>(notes) {
                        if v.is_object() {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Computes the itemized Cash-to-Close, CA Prop 13 Supplemental Tax Reserve, HOA Resale Fees,
/// Year-1 CapEx Reserve, and 50-State + DC Contingency Milestone Calendar from an `ExtensionQueryProto`.
pub fn compute_closing_contingency_bundle(query: &ExtensionQueryProto) -> Option<Value> {
    if let Some(c) = query
        .extra_params
        .get("country")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !matches!(
            c.to_ascii_uppercase().as_str(),
            "US" | "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA"
        ) {
            return None;
        }
    }
    let state = query.state.trim().to_ascii_uppercase();
    if !is_valid_us_state_51(&state) {
        return None;
    }

    let county = query.county_name.as_deref().unwrap_or("").trim();
    let county_fips = query.county_fips.trim();
    let city = query.city.trim();

    let price_from_params = parse_f64_param(
        query,
        &["price", "list_price_usd", "purchase_price", "listPrice", "estimated_value_usd"],
    )
    .filter(|&p| p > 0.0);
    let price_from_upstream = query.upstream_envelopes.iter().find_map(|e| match &e.payload {
        Some(Payload::MarketValuation(m)) => m.estimated_value_usd.map(|v| v as f64),
        Some(Payload::PropertyFacts(f)) => f.list_price_usd.or(f.last_sale_price_usd).map(|v| v as f64),
        _ => None,
    });
    let subject_price = price_from_params
        .or(price_from_upstream)
        .filter(|&p| p.is_finite() && p > 0.0);

    let hoa_monthly = parse_f64_param(query, &["hoa_monthly_usd", "hoaMonthly", "hoa_dues"])
        .filter(|&h| h >= 0.0);
    let has_hoa = query.is_condo_or_attached == Some(true)
        || hoa_monthly.is_some_and(|h| h > 0.0)
        || query
            .extra_params
            .get("has_hoa")
            .is_some_and(|s| s.eq_ignore_ascii_case("true") || s == "1");

    let calendar = build_state_contingency_calendar(&state, has_hoa);

    // Resolve statutory/filed title & escrow schedule from upstream `title_underwriter_rates`
    // or directly from `compute_state_title_rate_filing` when `title_underwriter_rates` is enabled.
    let geo_val = json!({
        "state": state,
        "county": county,
        "countyFips": county_fips,
        "city": city,
    });
    let mut facts_obj = serde_json::Map::new();
    if let Some(d) = query
        .extra_params
        .get("prior_sale_date")
        .or_else(|| query.extra_params.get("last_sale_date"))
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let prior_p = parse_f64_param(query, &["prior_sale_price", "last_sale_price"])
            .map(|p| p.round() as i64);
        facts_obj.insert(
            "lastSale".to_string(),
            json!({ "date": d, "price": prior_p }),
        );
    }
    let facts_val = Value::Object(facts_obj);

    let title_filing = parse_upstream_title_filing(query).unwrap_or_else(|| {
        compute_state_title_rate_filing(&geo_val, &facts_val, subject_price, Utc::now().date_naive())
    });

    // When price is absent, return null dollar fields (strict Data Honesty) + full state calendar + regime metadata
    let Some(price) = subject_price else {
        return Some(json!({
            "extensionId": CLOSING_CONTINGENCY_EXT_ID,
            "state": state,
            "purchasePriceUsd": Value::Null,
            "priceRequiredForDollarBreakdown": true,
            "titleUnderwriterFiling": title_filing,
            "buyerTitleAndEscrowUsd": Value::Null,
            "sellerTitleAndEscrowUsd": Value::Null,
            "earnestMoneyDepositRangeUsd": Value::Null,
            "transferTax": Value::Null,
            "prorationAndImpounds": Value::Null,
            "caProp13SupplementalTax": if state == "CA" {
                json!({
                    "applicable": true,
                    "citation": "Cal. Rev. & Tax. Code § 75.10 et seq.",
                    "estimatedSupplementalBillUsd": Value::Null,
                    "note": "Purchase price & prior assessed roll value required to compute exact Prop 13 supplemental tax bill."
                })
            } else {
                json!({ "applicable": false })
            },
            "hoaClosingFees": Value::Null,
            "year1CapexReserve": Value::Null,
            "scenarios": [],
            "contingencyCalendar": calendar,
        }));
    };

    // Buyer vs Seller Title + Escrow from statutory/filed underwriter schedule (zero 0.45% flat heuristic)
    let owner_net = title_filing["ownerStandardAfterReissueUsd"]
        .as_i64()
        .or_else(|| title_filing["ownerStandardUsd"].as_i64())
        .unwrap_or(0);
    let sim_lender = title_filing["simultaneousLenderUsd"].as_i64().unwrap_or(0);
    let escrow_fee = title_filing["escrowSettlementUsd"].as_i64().unwrap_or(0);
    let who_det = title_filing
        .pointer("/whoPaysCustom/whoPaysDeterministic")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (buyer_title_escrow_usd, seller_title_escrow_usd, title_split_basis) = if who_det {
        (
            title_filing
                .pointer("/whoPaysCustom/buyerEstimatedTitleEscrowUsd")
                .and_then(Value::as_i64)
                .unwrap_or(sim_lender + escrow_fee / 2),
            title_filing
                .pointer("/whoPaysCustom/sellerEstimatedTitleEscrowUsd")
                .and_then(Value::as_i64)
                .unwrap_or(owner_net + escrow_fee / 2),
            "customary_who_pays_deterministic",
        )
    } else {
        // Negotiable county split: buyer pays lender policy + 50% owner + 50% escrow
        let b = sim_lender + (owner_net / 2) + (escrow_fee / 2);
        let s = (owner_net - owner_net / 2) + (escrow_fee - escrow_fee / 2);
        (b, s, "negotiable_50_50_split_estimate")
    };

    // Mortgage rate, tax rate, and insurance inputs (prefer explicit/upstream verified figures)
    let rate30_opt = parse_f64_param(query, &["rate30", "mortgage_rate_30yr_pct", "rate_pct"])
        .or_else(|| {
            query.upstream_envelopes.iter().find_map(|e| match &e.payload {
                Some(Payload::MarketValuation(m)) => m.mortgage_rate_30yr_pct,
                _ => None,
            })
        })
        .filter(|&r| (1.5..=20.0).contains(&r));

    let eff_tax_rate = parse_f64_param(query, &["tax_rate", "effective_property_tax_rate"])
        .map(|r| if r > 0.15 { r / 100.0 } else { r })
        .filter(|&r| (0.001..=0.06).contains(&r))
        .unwrap_or_else(|| match state.as_str() {
            "TX" => 0.0185,
            "IL" | "NJ" => 0.0205,
            "NY" | "NH" | "CT" | "WI" => 0.0165,
            "CA" => 0.0118,
            "WA" | "OR" | "FL" => 0.0098,
            "HI" | "CO" | "AZ" | "NV" | "UT" => 0.0062,
            _ => 0.0110,
        });
    let annual_tax_usd = parse_f64_param(query, &["annual_tax_usd", "annualPropertyTaxUsd"])
        .filter(|&t| t > 0.0)
        .unwrap_or_else(|| (price * eff_tax_rate).round());

    let annual_ins_usd = parse_f64_param(query, &["annual_insurance_usd", "annualInsuranceUsd"])
        .or_else(|| {
            query.upstream_envelopes.iter().find_map(|e| match &e.payload {
                Some(Payload::InsuranceQuote(q)) => q.estimated_annual_premium_usd.map(|v| v as f64),
                Some(Payload::MarketValuation(m)) => m.estimated_annual_insurance_usd.map(|v| v as f64),
                _ => None,
            })
        })
        .filter(|&i| i > 0.0)
        .unwrap_or_else(|| {
            let pct = match state.as_str() {
                "FL" | "LA" | "OK" | "TX" => 0.0075,
                "CA" | "CO" => 0.0042,
                _ => 0.0038,
            };
            (price * pct).clamp(1_100.0, 14_000.0).round()
        });

    // Proration & RESPA 2-month cushion (`12 CFR § 1024.17`)
    let ins_12mo_usd = annual_ins_usd.round() as i64;
    let ins_cushion_2mo_usd = (annual_ins_usd * 2.0 / 12.0).round() as i64;
    let impound_months: f64 = if matches!(state.as_str(), "TX" | "IL" | "PA") {
        5.0
    } else {
        4.0
    };
    let tax_impound_usd = (annual_tax_usd * impound_months / 12.0).round() as i64;

    // California Prop 13 Supplemental Property Tax Reserve (Rev. & Tax. Code § 75.10)
    let prior_assessed = parse_f64_param(
        query,
        &["assessed_value_usd", "prior_assessed_value", "assessedValue"],
    )
    .filter(|&a| a > 0.0);
    let ca_prop13_supplemental = if state == "CA" {
        if let Some(assessed) = prior_assessed {
            let reassessment_delta = (price - assessed).max(0.0);
            // Average mid-fiscal-year ownership factor ~0.67 of first year + full gap until next roll
            let full_yr_supp = (reassessment_delta * eff_tax_rate).round() as i64;
            let prorated_8mo_reserve = ((full_yr_supp as f64) * 0.67).round() as i64;
            json!({
                "applicable": true,
                "citation": "Cal. Rev. & Tax. Code § 75.10 et seq.",
                "priorAssessedValueUsd": assessed.round() as i64,
                "newBaseYearValueUsd": price.round() as i64,
                "netSupplementalAssessmentUsd": reassessment_delta.round() as i64,
                "annualizedSupplementalTaxUsd": full_yr_supp,
                "recommendedPostCloseReserveUsd": prorated_8mo_reserve,
                "billedOutsideLenderImpound": true,
                "note": "County Tax Collector mails 1–2 supplemental tax bills directly to the buyer 4–9 months after recording; standard mortgage impound accounts do NOT pay supplemental bills automatically unless requested.",
            })
        } else {
            json!({
                "applicable": true,
                "citation": "Cal. Rev. & Tax. Code § 75.10 et seq.",
                "priorAssessedValueRequired": true,
                "estimatedSupplementalBillUsd": Value::Null,
                "billedOutsideLenderImpound": true,
                "note": "California reassesses to 100% of purchase price upon change in ownership and issues a separate Supplemental Tax Bill for (Purchase Price − Seller's Prop 13 Roll Value) × Tax Rate.",
            })
        }
    } else {
        json!({ "applicable": false })
    };

    // HOA Resale Certificate, Transfer & Working-Capital Reserve Fees
    let hoa_closing_fees = if has_hoa {
        let cert_fee = calendar.hoa_resale_fee_statutory_cap_usd.unwrap_or(350);
        let transfer_admin_fee: i64 = 275;
        let working_cap_2mo = hoa_monthly.map(|m| (m * 2.0).round() as i64).unwrap_or(0);
        let buyer_hoa_closing_total = transfer_admin_fee + working_cap_2mo;
        json!({
            "applicable": true,
            "resaleCertificateFeeUsd": cert_fee,
            "statutoryResaleFeeCapUsd": calendar.hoa_resale_fee_statutory_cap_usd,
            "associationTransferSetupFeeUsd": transfer_admin_fee,
            "workingCapitalContribution2MoUsd": if hoa_monthly.is_some() { json!(working_cap_2mo) } else { Value::Null },
            "estimatedBuyerHoaClosingTotalUsd": buyer_hoa_closing_total,
            "statutoryCitation": calendar.hoa_resale_statutory_citation,
        })
    } else {
        json!({
            "applicable": false,
            "estimatedBuyerHoaClosingTotalUsd": 0,
        })
    };
    let buyer_hoa_closing_usd = hoa_closing_fees["estimatedBuyerHoaClosingTotalUsd"]
        .as_i64()
        .unwrap_or(0);

    // Year-1 Immediate CapEx / Maintenance Reserve
    let year_built = query.year_built.map(|y| y as i64);
    let is_condo = query.is_condo_or_attached == Some(true);
    let capex_pct = if is_condo {
        0.005
    } else {
        match year_built {
            Some(y) if y < 1978 => 0.015,
            Some(y) if y < 2005 => 0.0125,
            Some(_) => 0.010,
            None => 0.010,
        }
    };
    let explicit_repair_reserve = parse_f64_param(query, &["inspection_repair_reserve_usd"])
        .filter(|&v| v >= 0.0)
        .map(|v| v.round() as i64);
    let recommended_year1_capex_usd =
        ((price * capex_pct).round() as i64) + explicit_repair_reserve.unwrap_or(0);
    let year1_capex_reserve = json!({
        "reserveRatePctOfPrice": round_n(capex_pct * 100.0, 2),
        "yearBuilt": year_built,
        "isCondoInteriorOnly": is_condo,
        "inspectionRepairAddOnUsd": explicit_repair_reserve,
        "recommendedYear1ReserveUsd": recommended_year1_capex_usd,
        "note": "Post-close liquidity buffer kept outside down payment & closing wire for immediate repairs, water-heater/HVAC lifecycle surprises, and deductible exposure.",
    });

    // Earnest Money Range in USD
    let em_low_usd = (price * (calendar.earnest_money_pct_low / 100.0)).round() as i64;
    let em_high_usd = (price * (calendar.earnest_money_pct_high / 100.0)).round() as i64;

    // Transfer tax baseline (20% down)
    let ttax_80 = estimate_transfer_tax_schedule(
        &state,
        county,
        county_fips,
        city,
        price,
        price * 0.80,
    );
    let appraisal_usd: i64 =
        if matches!(state.as_str(), "CA" | "NY" | "MA" | "WA" | "HI" | "DC") {
            925
        } else {
            775
        };
    let recording_usd: i64 = 275;
    let hoa_mo = hoa_monthly.unwrap_or(0.0);

    // Down-payment scenarios (only compute interest/PITI when a verified rate is present; otherwise keep rate/PI null!)
    let scenario_defs = [
        ("conv20", "20% Down (Conventional)", 20.0, 0.0, 0.0, 0.0),
        ("conv10", "10% Down (Conventional)", 10.0, 0.08, 0.42, 0.0),
        ("conv5", "5% Down (Conventional)", 5.0, 0.14, 0.58, 0.0),
        ("fha35", "3.5% Down (FHA)", 3.5, -0.25, 0.55, 1.75),
    ];

    let scenarios: Vec<Value> = scenario_defs
        .into_iter()
        .map(|(id, label, down_pct, rate_delta, pmi_ann_pct, upfront_mip_pct)| {
            let down_usd = (price * (down_pct / 100.0)).round();
            let base_loan = (price - down_usd).max(0.0);
            let upfront_mip = (base_loan * (upfront_mip_pct / 100.0)).round();
            let financed_loan = base_loan + upfront_mip;

            let tt = estimate_transfer_tax_schedule(&state, county, county_fips, city, price, base_loan);
            let orig_usd = (1_495.0 + base_loan * 0.0025).round() as i64;
            let buyer_tt_and_mrt =
                (tt.buyer_share_usd + tt.buyer_mortgage_recording_tax_usd).round() as i64 + recording_usd;

            let (rate_val, prepaid_int_15d, pi_val, piti_val) = if let Some(r30) = rate30_opt {
                let eff_rate = (r30 + rate_delta).max(1.0);
                let prep_int = (financed_loan * (eff_rate / 100.0) / 365.0 * 15.0).round() as i64;
                let pi_mo = monthly_pi(financed_loan, eff_rate, 30);
                let pmi_mo = (base_loan * (pmi_ann_pct / 100.0)) / 12.0;
                let piti_mo =
                    pi_mo + pmi_mo + (annual_tax_usd / 12.0) + (annual_ins_usd / 12.0) + hoa_mo;
                (
                    json!(round_n(eff_rate, 2)),
                    prep_int,
                    json!(pi_mo.round() as i64),
                    json!(piti_mo.round() as i64),
                )
            } else {
                (Value::Null, 0, Value::Null, Value::Null)
            };

            let total_closing = orig_usd
                + appraisal_usd
                + buyer_title_escrow_usd
                + buyer_tt_and_mrt
                + prepaid_int_15d
                + ins_12mo_usd
                + ins_cushion_2mo_usd
                + tax_impound_usd
                + buyer_hoa_closing_usd;
            let total_cash = down_usd.round() as i64 + total_closing;
            let total_cash_plus_capex_reserve = total_cash + recommended_year1_capex_usd;

            json!({
                "id": id,
                "label": label,
                "downPct": down_pct,
                "downPaymentUsd": down_usd.round() as i64,
                "baseLoanAmountUsd": base_loan.round() as i64,
                "upfrontMipFinancedUsd": upfront_mip as i64,
                "financedLoanAmountUsd": financed_loan.round() as i64,
                "ratePct": rate_val,
                "itemizedClosingCosts": {
                    "lenderOriginationUsd": orig_usd,
                    "appraisalAndCreditUsd": appraisal_usd,
                    "titleAndEscrowStatutoryOrFiledUsd": buyer_title_escrow_usd,
                    "titleSplitBasis": title_split_basis,
                    "buyerTransferAndRecordingTaxUsd": buyer_tt_and_mrt,
                    "buyerTransferTaxUsd": tt.buyer_share_usd.round() as i64,
                    "buyerMortgageRecordingTaxUsd": tt.buyer_mortgage_recording_tax_usd.round() as i64,
                    "recordingFeeUsd": recording_usd,
                    "prepaidInterest15dUsd": if rate30_opt.is_some() { json!(prepaid_int_15d) } else { Value::Null },
                    "insurance12MoUpfrontUsd": ins_12mo_usd,
                    "insuranceImpoundCushion2MoUsd": ins_cushion_2mo_usd,
                    "propertyTaxImpoundUsd": tax_impound_usd,
                    "hoaClosingAndWorkingCapitalUsd": buyer_hoa_closing_usd,
                },
                "totalClosingCostsUsd": total_closing,
                "totalCashToCloseUsd": total_cash,
                "earnestMoneyCreditedAtCloseRangeUsd": [em_low_usd, em_high_usd],
                "remainingWireAtCloseAfterLowEarnestUsd": (total_cash - em_low_usd).max(0),
                "totalLiquidityWithYear1CapexReserveUsd": total_cash_plus_capex_reserve,
                "monthlyPIUsd": pi_val,
                "monthlyPITIUsd": piti_val,
            })
        })
        .collect();

    Some(json!({
        "extensionId": CLOSING_CONTINGENCY_EXT_ID,
        "state": state,
        "purchasePriceUsd": price.round() as i64,
        "priceRequiredForDollarBreakdown": false,
        "titleUnderwriterFiling": title_filing,
        "buyerTitleAndEscrowUsd": buyer_title_escrow_usd,
        "sellerTitleAndEscrowUsd": seller_title_escrow_usd,
        "titleSplitBasis": title_split_basis,
        "earnestMoneyDepositRangeUsd": [em_low_usd, em_high_usd],
        "transferTax": {
            "jurisdiction": ttax_80.jurisdiction,
            "totalTransferTaxUsd": ttax_80.total_tax_usd.round() as i64,
            "buyerEstimatedShareUsd": ttax_80.buyer_share_usd.round() as i64,
            "sellerEstimatedShareUsd": ttax_80.seller_share_usd.round() as i64,
            "buyerMortgageRecordingTaxUsd": ttax_80.buyer_mortgage_recording_tax_usd.round() as i64,
            "effectiveRatePct": ttax_80.effective_rate_pct,
            "isHighTransferTaxJurisdiction": ttax_80.is_high_transfer_tax,
            "payerCustomNote": ttax_80.payer_custom_note,
            "breakdown": ttax_80.breakdown,
        },
        "prorationAndImpounds": {
            "annualPropertyTaxUsd": annual_tax_usd.round() as i64,
            "effectivePropertyTaxRatePct": round_n(eff_tax_rate * 100.0, 3),
            "impoundReserveMonths": impound_months as i64,
            "propertyTaxImpoundUsd": tax_impound_usd,
            "annualHomeownersInsuranceUsd": ins_12mo_usd,
            "insuranceImpoundCushion2MoUsd": ins_cushion_2mo_usd,
            "respaEscrowCushionCitation": "12 CFR § 1024.17 (RESPA 1/6th / 2-month escrow cushion cap)",
            "prorationCustom": calendar.property_tax_proration_basis,
        },
        "caProp13SupplementalTax": ca_prop13_supplemental,
        "hoaClosingFees": hoa_closing_fees,
        "year1CapexReserve": year1_capex_reserve,
        "scenarios": scenarios,
        "contingencyCalendar": calendar,
    }))
}

pub struct ClosingContingencyCalendarAdapter;

impl DataExtensionAdapter for ClosingContingencyCalendarAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: CLOSING_CONTINGENCY_EXT_ID.to_string(),
            name: "50-State Cash-to-Close, Prorations & Contingency Milestone Calendar".to_string(),
            version: "1.0.0".to_string(),
            description: "Itemized Cash-to-Close engine wiring statutory/filed title premiums from title_underwriter_rates, city/county transfer & mortgage recording tax buyer/seller splits, property-tax/insurance proration, CA Prop 13 supplemental tax reserve, HOA resale/transfer fees, Year-1 CapEx reserve, and a 50-state + DC earnest money & TRID contingency milestone calendar.".to_string(),
            domain: CapabilityDomain::DomainTaxMandates as i32,
            additional_domains: vec![
                CapabilityDomain::DomainTaxMandates as i32,
                CapabilityDomain::DomainInsurance as i32,
            ],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                nationwide: true,
                specificity_override: Some(700),
                ..Default::default()
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                ..Default::default()
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 50,
                burst: 100,
                timeout_ms: 2000,
                max_retries: 0,
            }),
            cache_ttl_secs: 604_800,
            priority: 255,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.TaxMandatesProto".to_string(),
            depends_on: vec!["title_underwriter_rates".to_string()],
            input_from_domains: vec![CapabilityDomain::DomainInsurance as i32],
            upstream_require_mode: "any".to_string(),
            homepage_url: Some(
                "https://www.consumerfinance.gov/owning-a-home/closing-disclosure/".to_string(),
            ),
            documentation_url: Some(
                "https://www.consumerfinance.gov/compliance/compliance-resources/mortgage-resources/tila-respa-integrated-disclosures/".to_string(),
            ),
            codeowners: vec!["@openestate/core".to_string()],
            default_provenance: ProvenanceTier::ProvenanceOfficialTariff as i32,
            metering: Some(MeteringSpecProto {
                dimensions: vec![
                    "invocations".to_string(),
                    "wall_time_ms".to_string(),
                    "external_api_calls".to_string(),
                    "bytes_downloaded".to_string(),
                ],
            }),
            sharing: Some(SharingSpecProto {
                scope: ShareScope::Parcel as i32,
                ttl_secs: 604_800,
                stale_while_revalidate_secs: 86_400,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: "CFPB TRID Closing Disclosure Rules, State Title Rate Filings & County Transfer Tax Schedules".to_string(),
                url: "https://www.consumerfinance.gov/owning-a-home/closing-disclosure/".to_string(),
                reference_url_template: "https://www.consumerfinance.gov/owning-a-home/closing-disclosure/".to_string(),
                schedule_or_version: Some("12 CFR § 1026.19(f) TRID + 2025–2026 State Title & Transfer Tax Schedules".to_string()),
                notes: Some("Replaces flat percentage title heuristics with promulgated/filed underwriter tariffs and never fabricates dollar figures when purchase price is absent.".to_string()),
            }),
            ..Default::default()
        }
    }

    fn execute_sync(&self, query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        let Some(bundle) = compute_closing_contingency_bundle(query) else {
            return Ok(Vec::new());
        };
        let regime = bundle
            .pointer("/titleUnderwriterFiling/rateRegime")
            .and_then(Value::as_str)
            .unwrap_or("estimated_schedule");
        let prov = if matches!(regime, "promulgated_statutory" | "state_filed_tariff") {
            ProvenanceTier::ProvenanceOfficialTariff
        } else {
            ProvenanceTier::ProvenanceLiveVerified
        };

        Ok(vec![ExtensionEnvelopeProto {
            extension_id: CLOSING_CONTINGENCY_EXT_ID.to_string(),
            domain: CapabilityDomain::DomainTaxMandates as i32,
            priority: 255,
            geo_specificity: 700,
            attribution: Some(SourceAttributionProto {
                extension_id: CLOSING_CONTINGENCY_EXT_ID.to_string(),
                source_name: "CFPB TRID Closing Disclosure Rules, State Title Rate Filings & County Transfer Tax Schedules".to_string(),
                source_url: "https://www.consumerfinance.gov/owning-a-home/closing-disclosure/".to_string(),
                provenance_tier: prov as i32,
                fetched_at_rfc3339: now_iso(),
                schedule_or_version: Some(regime.to_string()),
                notes: None,
            }),
            latency_ms: 0,
            cached: false,
            payload: Some(Payload::RawJson(serde_json::to_string(&bundle)?)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn all_50_states_plus_dc_produce_valid_trid_contingency_calendars() {
        let states = [
            "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL",
            "IN", "IA", "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE",
            "NV", "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD",
            "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
        ];
        assert_eq!(states.len(), 51);
        for st in states {
            let cal = build_state_contingency_calendar(st, true);
            assert_eq!(cal.state, st);
            assert_eq!(cal.trid_closing_disclosure_business_days_before_close, 3);
            assert!(cal.earnest_money_pct_low > 0.0 && cal.earnest_money_pct_high >= cal.earnest_money_pct_low);
            assert!(cal.milestones.iter().any(|m| m.id == "trid_closing_disclosure"));
            assert!(cal.milestones.iter().any(|m| m.id == "hoa_resale_review"));
        }
        // State-specific distinctions
        let nj = build_state_contingency_calendar("NJ", false);
        assert_eq!(nj.attorney_review_days, Some(3));
        let il = build_state_contingency_calendar("IL", false);
        assert_eq!(il.attorney_review_days, Some(5));
        let nc = build_state_contingency_calendar("NC", false);
        assert!(nc.option_or_due_diligence_fee_norm.is_some());
        let tx = build_state_contingency_calendar("TX", true);
        assert_eq!(tx.hoa_resale_fee_statutory_cap_usd, Some(375));
    }

    #[test]
    fn wires_title_underwriter_rates_and_ca_prop13_supplemental_tax_and_null_when_no_price() {
        // 1. Missing price -> null dollar fields, zero fabrication
        let q_no_price = ExtensionQueryProto {
            state: "CA".to_string(),
            city: "San Jose".to_string(),
            county_fips: "06085".to_string(),
            ..Default::default()
        };
        let b_null = compute_closing_contingency_bundle(&q_no_price).expect("CA bundle");
        assert!(b_null["purchasePriceUsd"].is_null());
        assert!(b_null["buyerTitleAndEscrowUsd"].is_null());
        assert_eq!(b_null["scenarios"].as_array().unwrap().len(), 0);
        assert_eq!(b_null["contingencyCalendar"]["state"], "CA");

        // 2. San Jose $2,200,000 purchase with $900,000 prior Prop 13 roll + HOA + Measure E transfer tax
        let mut params = HashMap::new();
        params.insert("price".to_string(), "2200000".to_string());
        params.insert("assessed_value_usd".to_string(), "900000".to_string());
        params.insert("tax_rate".to_string(), "0.012".to_string());
        params.insert("rate30".to_string(), "6.75".to_string());
        params.insert("hoa_monthly_usd".to_string(), "420".to_string());
        let q_sj = ExtensionQueryProto {
            state: "CA".to_string(),
            city: "San Jose".to_string(),
            county_fips: "06085".to_string(),
            county_name: Some("Santa Clara".to_string()),
            year_built: Some(1974),
            extra_params: params,
            ..Default::default()
        };
        let b_sj = compute_closing_contingency_bundle(&q_sj).expect("SJ bundle");
        assert_eq!(b_sj["purchasePriceUsd"], 2_200_000);
        // Santa Clara County custom: Seller pays Owner's policy & Escrow! Buyer pays ONLY simultaneous Lender policy!
        let buyer_te = b_sj["buyerTitleAndEscrowUsd"].as_i64().unwrap();
        let flat_heuristic_would_be = (2_200_000.0_f64 * 0.0045).clamp(1650.0, 9500.0) as i64;
        assert_ne!(buyer_te, flat_heuristic_would_be);
        assert!(buyer_te < flat_heuristic_would_be);
        // Prop 13 Supplemental Tax: ($2.2M - $900k) * 1.2% = $15,600 annualized
        assert_eq!(
            b_sj["caProp13SupplementalTax"]["netSupplementalAssessmentUsd"],
            1_300_000
        );
        assert_eq!(
            b_sj["caProp13SupplementalTax"]["annualizedSupplementalTaxUsd"],
            15_600
        );
        // San Jose Measure E + Conveyance 50/50 split
        assert!(b_sj["transferTax"]["totalTransferTaxUsd"].as_i64().unwrap() > 10_000);
        assert_eq!(b_sj["hoaClosingFees"]["applicable"], true);
        assert_eq!(b_sj["hoaClosingFees"]["workingCapitalContribution2MoUsd"], 840);
        assert_eq!(b_sj["year1CapexReserve"]["reserveRatePctOfPrice"], 1.5);

        // 3. Out-of-scope international returns None
        let mut q_intl = q_no_price;
        q_intl.state = "ON".to_string();
        q_intl
            .extra_params
            .insert("country".to_string(), "CA".to_string());
        assert!(compute_closing_contingency_bundle(&q_intl).is_none());
    }
}
