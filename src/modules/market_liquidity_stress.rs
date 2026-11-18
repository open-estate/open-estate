//! Track 6C Native Buyer Extension (`market_liquidity_stress`):
//! Combines keyless Freddie Mac Primary Mortgage Market Survey (PMMS) direct CSV
//! (`https://www.freddiemac.com/pmms/docs/PMMS_history.csv`, with FRED `MORTGAGE30US` fallback),
//! canonical 2026 FHFA county loan limits (`registry::loan_limits`), and ZIP/comp market depth
//! into:
//! 1. **5-Step Rate Shock Matrix** (`-1.0%`, `base`, `+0.5%`, `+1.0%`, `+1.5%` -> P&I, PITI,
//!    28/36% & 43% DTI qualifying income, and constant-payment purchasing power erosion).
//! 2. **Conforming / High-Balance / Jumbo Cliff** (exact dollar down-payment bridge to escape
//!    non-conforming Jumbo underwriting).
//! 3. **Price-Tier Liquidity Segmentation** (`Entry <0.85x`, `Core 0.85–1.35x`, `Upper >1.35x`
//!    and `>2.0x` ZIP median home value).

use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Instant;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind, ExecutionMode,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    GeoScopeProto, LicenseClass, ManifestSourceAttributionProto, MeteringSpecProto, ProvenanceTier,
    RateLimitSpecProto, ShareScope, SharingSpecProto, SourceAttributionProto,
};
use crate::modules::tod_cell_radar::is_valid_us_state_or_territory;
use crate::registry::loan_limits::{
    classify_loan_tier, infer_residential_units_from_type, resolve_county_loan_limits,
};
use crate::shared::{monthly_pi, now_iso, round_n, EnvConfig, HttpClient};

pub const MARKET_LIQUIDITY_STRESS_EXT_ID: &str = "market_liquidity_stress";
pub const FREDDIE_MAC_PMMS_CSV_URL: &str =
    "https://www.freddiemac.com/pmms/docs/PMMS_history.csv";
pub const FRED_MORTGAGE30_CSV_URL: &str =
    "https://fred.stlouisfed.org/graph/fredgraph.csv?id=MORTGAGE30US";
pub const FRED_MORTGAGE15_CSV_URL: &str =
    "https://fred.stlouisfed.org/graph/fredgraph.csv?id=MORTGAGE15US";

const PMMS_TIMEOUT_MS: u64 = 7_500;
const PMMS_CACHE_TTL_SECS: i64 = 43_200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PmmsRateSnapshot {
    pub as_of_date: String,
    pub mort30_pct: f64,
    pub mort15_pct: Option<f64>,
    pub mort30_year_ago_pct: Option<f64>,
    pub week52_low_30yr_pct: Option<f64>,
    pub week52_high_30yr_pct: Option<f64>,
    pub source_name: String,
    pub source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateShockStep {
    pub step_key: String,
    pub label: String,
    pub rate_delta_pct: f64,
    pub rate_pct: f64,
    pub monthly_pi_per_100k_usd: f64,
    pub monthly_pi_usd: Option<i64>,
    pub monthly_pi_delta_usd: Option<i64>,
    pub monthly_piti_usd: Option<i64>,
    pub monthly_piti_delta_usd: Option<i64>,
    pub qualifying_income_28pct_front_end_usd: Option<i64>,
    pub qualifying_income_36pct_with_500_debt_usd: Option<i64>,
    pub qualifying_income_43pct_qm_cap_usd: Option<i64>,
    pub purchasing_power_at_base_pi_usd: Option<i64>,
    pub purchasing_power_delta_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceTierLiquiditySegment {
    pub tier: String,
    pub label: String,
    pub is_upper_luxury_2x: bool,
    pub subject_price_usd: i64,
    pub zip_median_home_value_usd: i64,
    pub price_to_zip_median_ratio: f64,
    pub buyer_pool_profile: String,
    pub absorption_and_negotiation_note: String,
}

fn normalize_pmms_date(raw: &str) -> Option<String> {
    let s = raw.trim().trim_matches('"');
    if s.is_empty() {
        return None;
    }
    if let Some((y, rest)) = s.split_once('-') {
        if let Some((m, d)) = rest.split_once('-') {
            let yr = y.parse::<u32>().ok()?;
            let mo = m.parse::<u32>().ok()?;
            let dy = d.parse::<u32>().ok()?;
            if (1970..=2100).contains(&yr) && (1..=12).contains(&mo) && (1..=31).contains(&dy) {
                return Some(format!("{yr:04}-{mo:02}-{dy:02}"));
            }
        }
    }
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 3 {
        let mo = parts[0].trim().parse::<u32>().ok()?;
        let dy = parts[1].trim().parse::<u32>().ok()?;
        let yr = parts[2].trim().parse::<u32>().ok()?;
        if (1970..=2100).contains(&yr) && (1..=12).contains(&mo) && (1..=31).contains(&dy) {
            return Some(format!("{yr:04}-{mo:02}-{dy:02}"));
        }
    }
    None
}

/// Parses Freddie Mac's official `PMMS_history.csv` (`date,pmms30,pmms30p,pmms15,...`).
pub fn parse_freddie_mac_pmms_csv(csv_text: &str) -> Option<PmmsRateSnapshot> {
    let mut rows: Vec<(String, f64, Option<f64>)> = Vec::new();
    for (idx, line) in csv_text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if idx == 0 && trimmed.to_ascii_lowercase().starts_with("date") {
            continue;
        }
        let cols: Vec<&str> = trimmed.split(',').collect();
        if cols.len() < 2 {
            continue;
        }
        let Some(iso_date) = normalize_pmms_date(cols[0]) else {
            continue;
        };
        let Ok(r30) = cols[1].trim().trim_matches('"').parse::<f64>() else {
            continue;
        };
        if !r30.is_finite() || !(1.0..=25.0).contains(&r30) {
            continue;
        }
        let r15 = cols
            .get(3)
            .and_then(|c| c.trim().trim_matches('"').parse::<f64>().ok())
            .filter(|v| v.is_finite() && (1.0..=25.0).contains(v));
        rows.push((iso_date, r30, r15));
    }

    let (latest_date, latest_30, latest_15) = rows.last()?.clone();
    let tail_52 = &rows[rows.len().saturating_sub(52)..];
    let w52_low = tail_52
        .iter()
        .map(|(_, r, _)| *r)
        .reduce(f64::min)
        .map(|v| round_n(v, 2));
    let w52_high = tail_52
        .iter()
        .map(|(_, r, _)| *r)
        .reduce(f64::max)
        .map(|v| round_n(v, 2));
    let yr_ago = rows
        .len()
        .checked_sub(53)
        .map(|i| round_n(rows[i].1, 2));

    Some(PmmsRateSnapshot {
        as_of_date: latest_date,
        mort30_pct: round_n(latest_30, 2),
        mort15_pct: latest_15.map(|v| round_n(v, 2)),
        mort30_year_ago_pct: yr_ago,
        week52_low_30yr_pct: w52_low,
        week52_high_30yr_pct: w52_high,
        source_name: "Freddie Mac Primary Mortgage Market Survey (PMMS Direct CSV)".to_string(),
        source_url: "https://www.freddiemac.com/pmms".to_string(),
    })
}

fn parse_fred_single_series(csv_text: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for (idx, line) in csv_text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || (idx == 0 && trimmed.to_ascii_lowercase().contains("date")) {
            continue;
        }
        let Some((d, v)) = trimmed.split_once(',') else {
            continue;
        };
        let Some(iso) = normalize_pmms_date(d) else {
            continue;
        };
        if let Ok(rate) = v.trim().trim_matches('"').parse::<f64>() {
            if rate.is_finite() && (1.0..=25.0).contains(&rate) {
                out.push((iso, rate));
            }
        }
    }
    out
}

/// Fetches live Freddie Mac PMMS 30-yr / 15-yr rates from `PMMS_history.csv` first, falling back
/// to FRED `MORTGAGE30US` / `MORTGAGE15US` when needed. Never fabricates rates when offline.
pub async fn fetch_live_pmms_rates(
    http: &HttpClient,
    env: &EnvConfig,
) -> Result<Option<PmmsRateSnapshot>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    let pmms_url = env
        .get("FREDDIE_MAC_PMMS_CSV_URL")
        .unwrap_or(FREDDIE_MAC_PMMS_CSV_URL);
    let fred_base = env
        .get("FRED_CSV_BASE_URL")
        .map(|b| b.trim_end_matches('/').to_string());
    let fred30_url = fred_base
        .as_ref()
        .map(|b| format!("{b}?id=MORTGAGE30US"))
        .unwrap_or_else(|| FRED_MORTGAGE30_CSV_URL.to_string());
    let fred15_url = fred_base
        .as_ref()
        .map(|b| format!("{b}?id=MORTGAGE15US"))
        .unwrap_or_else(|| FRED_MORTGAGE15_CSV_URL.to_string());

    fetch_live_pmms_rates_with_urls(http, pmms_url, &fred30_url, &fred15_url).await
}

pub async fn fetch_live_pmms_rates_with_urls(
    http: &HttpClient,
    pmms_csv_url: &str,
    fred30_url: &str,
    fred15_url: &str,
) -> Result<Option<PmmsRateSnapshot>> {
    if let Ok(txt) = http
        .get_text_opts(pmms_csv_url, PMMS_TIMEOUT_MS, 1, PMMS_CACHE_TTL_SECS, None)
        .await
    {
        if let Some(snap) = parse_freddie_mac_pmms_csv(&txt) {
            return Ok(Some(snap));
        }
    }

    // Fallback to FRED MORTGAGE30US + MORTGAGE15US
    let (r30_res, r15_res) = tokio::join!(
        http.get_text_opts(fred30_url, PMMS_TIMEOUT_MS, 1, PMMS_CACHE_TTL_SECS, None),
        http.get_text_opts(fred15_url, PMMS_TIMEOUT_MS, 0, PMMS_CACHE_TTL_SECS, None),
    );
    let txt30 = r30_res.map_err(|e| anyhow!("PMMS direct CSV and FRED MORTGAGE30US failed: {e}"))?;
    let s30 = parse_fred_single_series(&txt30);
    let Some((latest_date, latest_30)) = s30.last().cloned() else {
        return Ok(None);
    };
    let latest_15 = r15_res
        .ok()
        .and_then(|t| parse_fred_single_series(&t).last().map(|(_, v)| round_n(*v, 2)));
    let tail_52 = &s30[s30.len().saturating_sub(52)..];
    let w52_low = tail_52
        .iter()
        .map(|(_, r)| *r)
        .reduce(f64::min)
        .map(|v| round_n(v, 2));
    let w52_high = tail_52
        .iter()
        .map(|(_, r)| *r)
        .reduce(f64::max)
        .map(|v| round_n(v, 2));
    let yr_ago = s30
        .len()
        .checked_sub(53)
        .map(|i| round_n(s30[i].1, 2));

    Ok(Some(PmmsRateSnapshot {
        as_of_date: latest_date,
        mort30_pct: round_n(latest_30, 2),
        mort15_pct: latest_15,
        mort30_year_ago_pct: yr_ago,
        week52_low_30yr_pct: w52_low,
        week52_high_30yr_pct: w52_high,
        source_name: "Freddie Mac PMMS via FRED (MORTGAGE30US / MORTGAGE15US)".to_string(),
        source_url: "https://fred.stlouisfed.org/series/MORTGAGE30US".to_string(),
    }))
}

/// Segments a property's price relative to its ZIP median home value into:
/// - `Entry <0.85x`
/// - `Core 0.85–1.35x`
/// - `Upper >1.35x` (flagging `>2.0x` Upper-Luxury)
pub fn classify_price_tier_liquidity(
    subject_price_usd: i64,
    zip_median_home_value_usd: i64,
) -> Option<PriceTierLiquiditySegment> {
    if subject_price_usd <= 0 || zip_median_home_value_usd <= 0 {
        return None;
    }
    let ratio = (subject_price_usd as f64) / (zip_median_home_value_usd as f64);
    let rounded_ratio = round_n(ratio, 2);

    if ratio < 0.85 {
        Some(PriceTierLiquiditySegment {
            tier: "entry".to_string(),
            label: "Entry Tier (<0.85× ZIP Median)".to_string(),
            is_upper_luxury_2x: false,
            subject_price_usd,
            zip_median_home_value_usd,
            price_to_zip_median_ratio: rounded_ratio,
            buyer_pool_profile: "Broadest FHA, conventional first-time, downsizer, and investor buyer pool within conforming loan limits.".to_string(),
            absorption_and_negotiation_note: "Entry-tier listings typically absorb fastest in the ZIP with tightest appraisal comp clustering; seller credit leverage rests primarily on documented physical inspection defects rather than extended market time.".to_string(),
        })
    } else if ratio <= 1.35 {
        Some(PriceTierLiquiditySegment {
            tier: "core".to_string(),
            label: "Core Move-Up Tier (0.85–1.35× ZIP Median)".to_string(),
            is_upper_luxury_2x: false,
            subject_price_usd,
            zip_median_home_value_usd,
            price_to_zip_median_ratio: rounded_ratio,
            buyer_pool_profile: "Primary conventional & high-balance owner-occupant family buyer pool.".to_string(),
            absorption_and_negotiation_note: "Core-tier liquidity tracks headline ZIP median days-to-pending and sale-to-list ratios closely; +100 bps rate moves directly shift buyers along the conforming/high-balance boundary.".to_string(),
        })
    } else if ratio > 2.0 {
        Some(PriceTierLiquiditySegment {
            tier: "upper".to_string(),
            label: "Upper-Luxury Tier (>2.00× ZIP Median)".to_string(),
            is_upper_luxury_2x: true,
            subject_price_usd,
            zip_median_home_value_usd,
            price_to_zip_median_ratio: rounded_ratio,
            buyer_pool_profile: "Concentrated jumbo-financed and all-cash discretionary buyer pool.".to_string(),
            absorption_and_negotiation_note: "Above 2.0× ZIP median, comparable sales are sparser with wider $/sqft dispersion and longer marketing tails; buyers typically command stronger inspection/rate-buydown or price-cut leverage once DOM exceeds ZIP median.".to_string(),
        })
    } else {
        Some(PriceTierLiquiditySegment {
            tier: "upper".to_string(),
            label: "Upper Tier (>1.35× ZIP Median)".to_string(),
            is_upper_luxury_2x: false,
            subject_price_usd,
            zip_median_home_value_usd,
            price_to_zip_median_ratio: rounded_ratio,
            buyer_pool_profile: "High-balance and non-conforming Jumbo equity move-up buyers requiring larger post-close PITI reserves.".to_string(),
            absorption_and_negotiation_note: "Above 1.35× ZIP median, non-conforming Jumbo reserve and 43% DTI underwriting narrow the active buyer funnel when mortgage rates rise, increasing seller receptivity to 2-1 buydowns or closing credits.".to_string(),
        })
    }
}

/// Solves for the loan principal whose 30-yr monthly P&I at `shocked_rate_pct` equals `target_monthly_pi`.
fn loan_principal_for_monthly_pi(target_monthly_pi: f64, shocked_rate_pct: f64) -> f64 {
    if target_monthly_pi <= 0.0 {
        return 0.0;
    }
    let unit_pi = monthly_pi(1.0, shocked_rate_pct.max(0.25), 30);
    if unit_pi <= 0.0 {
        0.0
    } else {
        target_monthly_pi / unit_pi
    }
}

/// Computes the 5-step Rate Shock Matrix (`-1.0%`, `base`, `+0.5%`, `+1.0%`, `+1.5%`).
pub fn compute_rate_shock_matrix(
    base_rate_pct: f64,
    subject_price_usd: Option<f64>,
    down_pct: f64,
    monthly_non_pi_carrying_usd: f64,
) -> Vec<RateShockStep> {
    let steps = [
        ("minus_100bps", "-1.0% Refi / Rally", -1.0),
        ("base", "Base Current Rate", 0.0),
        ("plus_50bps", "+0.5% Rate Shock", 0.5),
        ("plus_100bps", "+1.0% Rate Shock", 1.0),
        ("plus_150bps", "+1.5% Severe Shock", 1.5),
    ];

    let clamped_down = down_pct.clamp(0.0, 95.0);
    let down_frac = clamped_down / 100.0;
    let loan_frac = (1.0 - down_frac).max(0.05);

    let price_clean = subject_price_usd.filter(|&p| p.is_finite() && p > 0.0);
    let base_loan_opt = price_clean.map(|p| p * loan_frac);
    let base_pi_opt = base_loan_opt.map(|l| monthly_pi(l, base_rate_pct, 30));
    let base_piti_opt = base_pi_opt.map(|pi| pi + monthly_non_pi_carrying_usd.max(0.0));

    steps
        .into_iter()
        .map(|(key, label, delta)| {
            let r = round_n((base_rate_pct + delta).max(1.0), 2);
            let pi_per_100k = round_n(monthly_pi(100_000.0, r, 30), 2);

            if let (Some(price), Some(loan), Some(base_pi), Some(base_piti)) =
                (price_clean, base_loan_opt, base_pi_opt, base_piti_opt)
            {
                let pi = monthly_pi(loan, r, 30);
                let piti = pi + monthly_non_pi_carrying_usd.max(0.0);
                let pi_i = pi.round() as i64;
                let base_pi_i = base_pi.round() as i64;
                let piti_i = piti.round() as i64;
                let base_piti_i = base_piti.round() as i64;

                let inc_28 = ((piti * 12.0) / 0.28).round() as i64;
                // 36% back-end DTI assuming $500/mo recurring auto/student/installment obligations
                let inc_36 = (((piti + 500.0) * 12.0) / 0.36).round() as i64;
                let inc_43 = ((piti * 12.0) / 0.43).round() as i64;

                let eq_loan = loan_principal_for_monthly_pi(base_pi, r);
                let eq_price = eq_loan + (price * down_frac);
                let pp_delta_pct = round_n(((eq_price - price) / price) * 100.0, 1);

                RateShockStep {
                    step_key: key.to_string(),
                    label: label.to_string(),
                    rate_delta_pct: delta,
                    rate_pct: r,
                    monthly_pi_per_100k_usd: pi_per_100k,
                    monthly_pi_usd: Some(pi_i),
                    monthly_pi_delta_usd: Some(pi_i - base_pi_i),
                    monthly_piti_usd: Some(piti_i),
                    monthly_piti_delta_usd: Some(piti_i - base_piti_i),
                    qualifying_income_28pct_front_end_usd: Some(inc_28),
                    qualifying_income_36pct_with_500_debt_usd: Some(inc_36),
                    qualifying_income_43pct_qm_cap_usd: Some(inc_43),
                    purchasing_power_at_base_pi_usd: Some(eq_price.round() as i64),
                    purchasing_power_delta_pct: Some(pp_delta_pct),
                }
            } else {
                RateShockStep {
                    step_key: key.to_string(),
                    label: label.to_string(),
                    rate_delta_pct: delta,
                    rate_pct: r,
                    monthly_pi_per_100k_usd: pi_per_100k,
                    monthly_pi_usd: None,
                    monthly_pi_delta_usd: None,
                    monthly_piti_usd: None,
                    monthly_piti_delta_usd: None,
                    qualifying_income_28pct_front_end_usd: None,
                    qualifying_income_36pct_with_500_debt_usd: None,
                    qualifying_income_43pct_qm_cap_usd: None,
                    purchasing_power_at_base_pi_usd: None,
                    purchasing_power_delta_pct: None,
                }
            }
        })
        .collect()
}

fn parse_param_f64(query: &ExtensionQueryProto, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        query
            .extra_params
            .get(*k)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
    })
}

/// Builds the complete Market Liquidity & Rate Stress payload from a query and a verified `PmmsRateSnapshot`.
pub fn assemble_market_liquidity_stress(
    query: &ExtensionQueryProto,
    pmms: &PmmsRateSnapshot,
) -> Value {
    let state = query.state.trim().to_ascii_uppercase();
    let county_fips = query.county_fips.trim();
    let zip = query.zip.trim();
    let prop_type = query
        .extra_params
        .get("property_type")
        .map(String::as_str)
        .unwrap_or("");
    let units = infer_residential_units_from_type(prop_type);
    let limits = resolve_county_loan_limits(&state, county_fips, units);

    let subject_price = parse_param_f64(
        query,
        &["price", "list_price_usd", "purchase_price", "listPrice", "estimated_value_usd"],
    )
    .or_else(|| {
        query.upstream_envelopes.iter().find_map(|e| match &e.payload {
            Some(Payload::MarketValuation(m)) => m.estimated_value_usd.map(|v| v as f64),
            Some(Payload::PropertyFacts(f)) => f.list_price_usd.or(f.last_sale_price_usd).map(|v| v as f64),
            _ => None,
        })
    })
    .filter(|&p| p > 0.0);

    let zip_median = parse_param_f64(
        query,
        &["zip_median_usd", "zip_median_home_value_usd", "zhvi_median"],
    )
    .or_else(|| {
        query.upstream_envelopes.iter().find_map(|e| match &e.payload {
            Some(Payload::MarketValuation(m)) => m.zip_median_home_value_usd.map(|v| v as f64),
            _ => None,
        })
    })
    .filter(|&m| m > 0.0);

    let down_pct = parse_param_f64(query, &["down_pct", "downPaymentPct"])
        .filter(|&d| (0.0..=90.0).contains(&d))
        .unwrap_or(20.0);

    let annual_tax_opt = parse_param_f64(query, &["annual_tax_usd", "annualPropertyTaxUsd"])
        .filter(|&t| t >= 0.0)
        .or_else(|| {
            let tr = parse_param_f64(query, &["tax_rate", "effective_property_tax_rate"])
                .or_else(|| {
                    query.upstream_envelopes.iter().find_map(|e| match &e.payload {
                        Some(Payload::MarketValuation(m)) => m.effective_property_tax_rate,
                        _ => None,
                    })
                })
                .map(|r| if r > 0.15 { r / 100.0 } else { r })
                .filter(|&r| (0.001..=0.06).contains(&r))?;
            subject_price.map(|p| p * tr)
        });
    let annual_ins_opt = parse_param_f64(query, &["annual_insurance_usd", "annualInsuranceUsd"])
        .or_else(|| {
            query.upstream_envelopes.iter().find_map(|e| match &e.payload {
                Some(Payload::MarketValuation(m)) => m.estimated_annual_insurance_usd.map(|v| v as f64),
                Some(Payload::InsuranceQuote(q)) => q.estimated_annual_premium_usd.map(|v| v as f64),
                _ => None,
            })
        })
        .filter(|&i| i >= 0.0);
    let hoa_monthly = parse_param_f64(query, &["hoa_monthly_usd", "hoaMonthly"])
        .filter(|&h| h >= 0.0)
        .unwrap_or(0.0);

    let monthly_non_pi = (annual_tax_opt.unwrap_or(0.0) / 12.0)
        + (annual_ins_opt.unwrap_or(0.0) / 12.0)
        + hoa_monthly;

    let base_rate = parse_param_f64(query, &["base_rate_pct", "rate30"])
        .filter(|&r| (1.5..=20.0).contains(&r))
        .unwrap_or(pmms.mort30_pct);

    let shock_matrix =
        compute_rate_shock_matrix(base_rate, subject_price, down_pct, monthly_non_pi);

    let loan_cliff = if let Some(price) = subject_price {
        let base_loan = (price * (1.0 - down_pct / 100.0)).round() as i64;
        let tier = classify_loan_tier(base_loan, &limits);
        let excess_baseline = (base_loan - limits.baseline_limit).max(0);
        let excess_county = (base_loan - limits.conforming_limit).max(0);
        let min_down_avoid_jumbo_pct = if price > (limits.conforming_limit as f64) {
            round_n(
                ((price - limits.conforming_limit as f64) / price) * 100.0,
                1,
            )
        } else {
            0.0
        };
        json!({
            "units": limits.units,
            "baselineConformingLimitUsd": limits.baseline_limit,
            "countyConformingLimitUsd": limits.conforming_limit,
            "countyFhaLimitUsd": limits.fha_limit,
            "isHighCostArea": limits.is_high_cost_area,
            "assumedDownPct": down_pct,
            "baseLoanAmountUsd": base_loan,
            "loanTier": tier,
            "isJumbo": base_loan > limits.conforming_limit,
            "excessOverBaselineUsd": excess_baseline,
            "excessOverCountyConformingUsd": excess_county,
            "minDownPaymentPctToAvoidJumbo": min_down_avoid_jumbo_pct,
        })
    } else {
        json!({
            "units": limits.units,
            "baselineConformingLimitUsd": limits.baseline_limit,
            "countyConformingLimitUsd": limits.conforming_limit,
            "countyFhaLimitUsd": limits.fha_limit,
            "isHighCostArea": limits.is_high_cost_area,
            "baseLoanAmountUsd": Value::Null,
            "loanTier": Value::Null,
        })
    };

    let price_tier_segment = match (subject_price, zip_median) {
        (Some(p), Some(zm)) => classify_price_tier_liquidity(p.round() as i64, zm.round() as i64),
        _ => None,
    };

    let comp_count = parse_param_f64(query, &["comp_count"]).map(|c| c.round() as i64);
    let days_to_pending = parse_param_f64(query, &["days_to_pending"]).map(|d| round_n(d, 1));
    let sale_to_list_ratio = parse_param_f64(query, &["sale_to_list_ratio"]).map(|r| round_n(r, 4));
    let price_cut_share_pct = parse_param_f64(query, &["price_cut_share_pct"]).map(|r| round_n(r, 1));

    json!({
        "extensionId": MARKET_LIQUIDITY_STRESS_EXT_ID,
        "fetchedAt": now_iso(),
        "state": if state.is_empty() { Value::Null } else { json!(state) },
        "zip": if zip.is_empty() { Value::Null } else { json!(zip) },
        "countyFips": if county_fips.is_empty() { Value::Null } else { json!(county_fips) },
        "subjectPriceUsd": subject_price.map(|p| p.round() as i64),
        "zipMedianHomeValueUsd": zip_median.map(|m| m.round() as i64),
        "pmmsRates": pmms,
        "baseNoteRatePct": round_n(base_rate, 2),
        "carryingInputsIncludedInPiti": {
            "annualTaxIncluded": annual_tax_opt.is_some(),
            "annualInsuranceIncluded": annual_ins_opt.is_some(),
            "monthlyNonPiUsd": round_n(monthly_non_pi, 2),
        },
        "rateShockMatrix": shock_matrix,
        "conformingJumboCliff": loan_cliff,
        "priceTierSegmentation": price_tier_segment,
        "observedMarketDepth": {
            "comparableSalesCount": comp_count,
            "metroDaysToPending": days_to_pending,
            "metroSaleToListRatio": sale_to_list_ratio,
            "metroPriceCutSharePct": price_cut_share_pct,
        },
    })
}

/// Convenience helper enriching a full report object (`geo`, `facts`, `market`, `valuation`)
/// when Wave-2 report modules invoke `market_liquidity_stress`.
pub fn compute_market_liquidity_stress_from_report(
    report: &Value,
    live_pmms: Option<&PmmsRateSnapshot>,
) -> Option<Value> {
    let geo = report.get("geo").unwrap_or(&Value::Null);
    let facts = report.get("facts").unwrap_or(&Value::Null);
    let market = report
        .pointer("/modules/market/data")
        .or_else(|| report.get("market"))
        .unwrap_or(&Value::Null);
    let valuation = report
        .pointer("/modules/valuation/data")
        .or_else(|| report.get("valuation"))
        .unwrap_or(&Value::Null);

    let pmms = if let Some(s) = live_pmms {
        s.clone()
    } else {
        let m30 = market
            .pointer("/rates/mort30")
            .and_then(Value::as_f64)
            .filter(|&r| (1.0..=25.0).contains(&r))?;
        let d30 = market
            .pointer("/rates/mort30Date")
            .and_then(Value::as_str)
            .unwrap_or("reported")
            .to_string();
        PmmsRateSnapshot {
            as_of_date: d30,
            mort30_pct: round_n(m30, 2),
            mort15_pct: market
                .pointer("/rates/mort15")
                .and_then(Value::as_f64)
                .map(|r| round_n(r, 2)),
            mort30_year_ago_pct: market
                .pointer("/rates/mort30YearAgo")
                .and_then(Value::as_f64)
                .map(|r| round_n(r, 2)),
            week52_low_30yr_pct: None,
            week52_high_30yr_pct: None,
            source_name: "Report Market Rates (Freddie Mac PMMS)".to_string(),
            source_url: "https://www.freddiemac.com/pmms".to_string(),
        }
    };

    let mut extra = std::collections::HashMap::new();
    if let Some(p) = facts
        .get("listPrice")
        .or_else(|| facts.get("price"))
        .or_else(|| valuation.pointer("/estimate/value"))
        .and_then(Value::as_f64)
        .filter(|&p| p > 0.0)
    {
        extra.insert("price".to_string(), format!("{p}"));
    }
    if let Some(zm) = market
        .pointer("/zhvi/allHomes/latest")
        .or_else(|| market.pointer("/zhvi/sfr/latest"))
        .or_else(|| market.pointer("/acs/zip/medianHomeValue"))
        .and_then(Value::as_f64)
        .filter(|&m| m > 0.0)
    {
        extra.insert("zip_median_usd".to_string(), format!("{zm}"));
    }
    if let Some(pt) = facts.get("propertyType").and_then(Value::as_str) {
        extra.insert("property_type".to_string(), pt.to_string());
    }
    if let Some(comps) = valuation
        .pointer("/comps/items")
        .or_else(|| facts.get("comps"))
        .and_then(Value::as_array)
    {
        extra.insert("comp_count".to_string(), comps.len().to_string());
    }
    if let Some(dom) = market
        .pointer("/temperature/daysToPending")
        .and_then(Value::as_f64)
    {
        extra.insert("days_to_pending".to_string(), format!("{dom}"));
    }
    if let Some(stl) = market
        .pointer("/temperature/saleToList")
        .and_then(Value::as_f64)
    {
        extra.insert("sale_to_list_ratio".to_string(), format!("{stl}"));
    }
    if let Some(pc) = market
        .pointer("/temperature/priceCutSharePct")
        .and_then(Value::as_f64)
    {
        extra.insert("price_cut_share_pct".to_string(), format!("{pc}"));
    }

    let q = ExtensionQueryProto {
        state: geo
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        zip: geo
            .get("zip")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        county_fips: geo
            .get("countyFips")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        extra_params: extra,
        ..Default::default()
    };
    Some(assemble_market_liquidity_stress(&q, &pmms))
}

pub struct MarketLiquidityStressAdapter;

impl DataExtensionAdapter for MarketLiquidityStressAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: MARKET_LIQUIDITY_STRESS_EXT_ID.to_string(),
            name: "Freddie Mac PMMS Rate Shock Matrix & Price-Tier Liquidity Segmentation"
                .to_string(),
            version: "1.0.0".to_string(),
            description: "Keyless Freddie Mac PMMS direct CSV + 2026 FHFA county loan-limit analyzer computing a 5-step Rate Shock Matrix (-1.0%, base, +0.5%, +1.0%, +1.5% PITI, 28/36% DTI qualifying income, conforming/high-balance/jumbo cliff) and Entry (<0.85x) / Core (0.85–1.35x) / Upper (>1.35x) ZIP median liquidity segmentation.".to_string(),
            domain: CapabilityDomain::DomainValuation as i32,
            additional_domains: vec![
                CapabilityDomain::DomainValuation as i32,
                CapabilityDomain::DomainMarketTrends as i32,
            ],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                nationwide: true,
                specificity_override: Some(550),
                ..Default::default()
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                ..Default::default()
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 5,
                burst: 10,
                timeout_ms: 8000,
                max_retries: 1,
            }),
            cache_ttl_secs: 86_400,
            priority: 245,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.MarketValuationProto".to_string(),
            homepage_url: Some("https://www.freddiemac.com/pmms".to_string()),
            documentation_url: Some(
                "https://www.fhfa.gov/data/conforming-loan-limit".to_string(),
            ),
            codeowners: vec!["@openestate/valuation".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            metering: Some(MeteringSpecProto {
                dimensions: vec![
                    "invocations".to_string(),
                    "wall_time_ms".to_string(),
                    "external_api_calls".to_string(),
                    "bytes_downloaded".to_string(),
                ],
            }),
            sharing: Some(SharingSpecProto {
                scope: ShareScope::Zip as i32,
                ttl_secs: 86_400,
                stale_while_revalidate_secs: 21_600,
                background_refresh: true,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: "Freddie Mac Primary Mortgage Market Survey (PMMS) & FHFA Conforming Loan Limits".to_string(),
                url: "https://www.freddiemac.com/pmms".to_string(),
                reference_url_template: "https://www.freddiemac.com/pmms".to_string(),
                schedule_or_version: Some("Weekly PMMS History CSV + 2026 FHFA County Loan Limits".to_string()),
                notes: Some("Fetches live Freddie Mac PMMS direct CSV (with FRED MORTGAGE30US fallback) and never fabricates mortgage rates or home prices when offline.".to_string()),
            }),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if http.is_offline_external_blocked() {
                return Ok(Vec::new());
            }
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
                    return Ok(Vec::new());
                }
            }
            let st = query.state.trim().to_ascii_uppercase();
            if !st.is_empty() && !is_valid_us_state_or_territory(&st) {
                return Ok(Vec::new());
            }

            let started = Instant::now();
            let Some(pmms) = fetch_live_pmms_rates(http, env).await? else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let payload_val = assemble_market_liquidity_stress(query, &pmms);
            let fetched_at = payload_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);

            Ok(vec![ExtensionEnvelopeProto {
                extension_id: MARKET_LIQUIDITY_STRESS_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainValuation as i32,
                priority: 245,
                geo_specificity: 550,
                attribution: Some(SourceAttributionProto {
                    extension_id: MARKET_LIQUIDITY_STRESS_EXT_ID.to_string(),
                    source_name: pmms.source_name,
                    source_url: pmms.source_url,
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some(pmms.as_of_date),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&payload_val)?)),
            }])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[test]
    fn parses_freddie_mac_pmms_direct_csv_with_mdy_dates() {
        let sample = "\
date,pmms30,pmms30p,pmms15,pmms15p,pmms51,pmms51p,pmms51m,pmms51spread
11/13/2025,6.84,,6.02,,,,,
10/29/2026,7.24,,6.60,,,,,
11/5/2026,7.18,,6.57,,,,,
11/12/2026,7.11,,6.45,,,,,
";
        let snap = parse_freddie_mac_pmms_csv(sample).expect("parsed PMMS snapshot");
        assert_eq!(snap.as_of_date, "2026-11-12");
        assert_eq!(snap.mort30_pct, 7.11);
        assert_eq!(snap.mort15_pct, Some(6.45));
        assert_eq!(snap.week52_low_30yr_pct, Some(6.84));
        assert_eq!(snap.week52_high_30yr_pct, Some(7.24));
    }

    #[test]
    fn five_step_rate_shock_jumbo_cliff_and_price_tiers_compute_accurately() {
        let pmms = PmmsRateSnapshot {
            as_of_date: "2026-11-12".to_string(),
            mort30_pct: 7.00,
            mort15_pct: Some(6.35),
            mort30_year_ago_pct: Some(6.80),
            week52_low_30yr_pct: Some(6.10),
            week52_high_30yr_pct: Some(7.25),
            source_name: "Freddie Mac PMMS".to_string(),
            source_url: "https://www.freddiemac.com/pmms".to_string(),
        };

        let mut extra = HashMap::new();
        extra.insert("price".to_string(), "1800000".to_string());
        extra.insert("zip_median_usd".to_string(), "1250000".to_string());
        extra.insert("annual_tax_usd".to_string(), "21600".to_string());
        extra.insert("annual_insurance_usd".to_string(), "3600".to_string());
        let q = ExtensionQueryProto {
            state: "CA".to_string(),
            county_fips: "06085".to_string(),
            zip: "95008".to_string(),
            extra_params: extra,
            ..Default::default()
        };

        let out = assemble_market_liquidity_stress(&q, &pmms);
        let matrix = out["rateShockMatrix"].as_array().unwrap();
        assert_eq!(matrix.len(), 5);
        assert_eq!(matrix[0]["rateDeltaPct"], -1.0);
        assert_eq!(matrix[1]["stepKey"], "base");
        assert_eq!(matrix[1]["monthlyPiDeltaUsd"], 0);
        assert_eq!(matrix[2]["rateDeltaPct"], 0.5);
        assert_eq!(matrix[3]["rateDeltaPct"], 1.0);
        assert_eq!(matrix[4]["rateDeltaPct"], 1.5);
        assert!(matrix[0]["monthlyPitiUsd"].as_i64().unwrap() < matrix[1]["monthlyPitiUsd"].as_i64().unwrap());
        assert!(matrix[4]["monthlyPitiUsd"].as_i64().unwrap() > matrix[1]["monthlyPitiUsd"].as_i64().unwrap());
        assert!(matrix[4]["purchasingPowerDeltaPct"].as_f64().unwrap() < -5.0);

        // Santa Clara 06085 ceiling = $1,249,125; 80% of $1.8M = $1,440,000 -> Jumbo!
        assert_eq!(
            out["conformingJumboCliff"]["countyConformingLimitUsd"],
            1_249_125
        );
        assert_eq!(
            out["conformingJumboCliff"]["loanTier"],
            "Jumbo (Non-Conforming)"
        );
        assert_eq!(
            out["conformingJumboCliff"]["excessOverCountyConformingUsd"],
            190_875
        );

        // $1.8M / $1.25M = 1.44x -> Upper (>1.35x)
        assert_eq!(out["priceTierSegmentation"]["tier"], "upper");
        assert_eq!(out["priceTierSegmentation"]["priceToZipMedianRatio"], 1.44);

        // Verify Entry (<0.85x) and Core (0.85–1.35x) boundaries
        assert_eq!(
            classify_price_tier_liquidity(950_000, 1_250_000).unwrap().tier,
            "entry"
        );
        assert_eq!(
            classify_price_tier_liquidity(1_250_000, 1_250_000).unwrap().tier,
            "core"
        );
        assert!(
            classify_price_tier_liquidity(2_600_000, 1_250_000)
                .unwrap()
                .is_upper_luxury_2x
        );
    }

    #[tokio::test]
    async fn offline_never_fabricates_and_loopback_pmms_csv_executes_cleanly() {
        let http = HttpClient::offline_loopback_only(None);
        let empty_env = EnvConfig::from_map(HashMap::new());
        let adapter = MarketLiquidityStressAdapter;
        let q = ExtensionQueryProto {
            state: "CA".to_string(),
            zip: "95008".to_string(),
            ..Default::default()
        };
        assert!(adapter
            .execute_async(&q, &http, &empty_env)
            .await
            .unwrap()
            .is_empty());

        let app = Router::new().route(
            "/pmms.csv",
            get(|| async {
                "date,pmms30,pmms30p,pmms15,pmms15p\n11/12/2026,7.11,,6.45,\n"
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let snap = fetch_live_pmms_rates_with_urls(
            &http,
            &format!("http://{addr}/pmms.csv"),
            &format!("http://{addr}/missing"),
            &format!("http://{addr}/missing"),
        )
        .await
        .unwrap()
        .expect("loopback pmms");
        assert_eq!(snap.mort30_pct, 7.11);
        assert_eq!(snap.mort15_pct, Some(6.45));
    }
}
