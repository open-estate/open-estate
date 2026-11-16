//! The documented 0–100 utility curves of "Your Match" (plan v3, section 5.4), ported from
//! `match_prototype_v3.py`. All curves are v0 proposals, to be calibrated.
//!
//! | Factor | Curve |
//! |---|---|
//! | Monthly cost ÷ budget | 100 at 0.80 or less, 70 at 1.00, 0 at 1.25 |
//! | Commute minutes (free-flow × traffic buffer) vs. maximum T | 100 at T/2, 60 at T, 0 at 1.75 T |
//! | Beds or baths vs. need | 100 if one more than needed, 85 if equal, 40 if one short, else 0 |
//! | Size and lot vs. target | linear up to the target, capped at 100 |
//! | Effective age | 100 at 5 years, 75 at 20, 50 at 50, 30 at 80 |
//! | Schools | mean public-data value (0–100) of the assigned schools at the selected levels |
//! | Style | 100 × Σ probability × preference (love 1.0, fine 0.6, avoid 0); unknown mass counts as fine |
//! | Crime | 100 − 34 × (0.6 × violent ÷ national + 0.4 × property ÷ national), kept within 20–96 |

use super::catalog::{Dimension, Feature, LoanType, StyleFamily};
use super::factors::{Factor, HomeFactors};
use super::profile::{Profile, DEFAULT_DOWN_PAYMENT_PCT};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const COST_CURVE: [(f64, f64); 3] = [(0.80, 100.0), (1.00, 70.0), (1.25, 0.0)];
pub const AGE_CURVE: [(f64, f64); 4] = [(5.0, 100.0), (20.0, 75.0), (50.0, 50.0), (80.0, 30.0)];
/// Preference value for "fine" and for style probability mass that no evidence explains.
pub const NEUTRAL_STYLE: f64 = 0.6;
/// Share of the style-match sub-factor when the buyer also wished for features (v0).
pub const STYLE_MATCH_SHARE: f64 = 0.6;

/// Home-fit sub-factor weights (prototype v3).
pub const HOMEFIT_BEDS: f64 = 0.25;
pub const HOMEFIT_BATHS: f64 = 0.15;
pub const HOMEFIT_SPACE: f64 = 0.15;
pub const HOMEFIT_LOT: f64 = 0.15;
pub const HOMEFIT_CONDITION: f64 = 0.15;
pub const HOMEFIT_LAYOUT: f64 = 0.15;
/// Layout weight for a multigenerational household.
pub const HOMEFIT_LAYOUT_MULTIGEN: f64 = 0.40;
/// Parking weight for a multigenerational household when explicit parking count is known.
pub const HOMEFIT_PARKING_MULTIGEN: f64 = 0.10;

/// Why a dimension value is an estimate rather than real data for this home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateReason {
    /// No data for this home; the median of the search is used.
    ImputedMedian,
    /// No style evidence; unknown style counts as "fine".
    NoStyleEvidence,
    /// Drive time from a road-distance estimate (router down or outside coverage).
    RoadDistanceEstimate,
    /// The source marked its own value as an estimate.
    SourceEstimate,
}

/// One dimension's utility for one home before the engine fills gaps: `value` is `None` when the
/// home has no data for it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Cell {
    pub value: Option<f64>,
    pub estimated: Option<EstimateReason>,
}

impl Cell {
    fn real(v: Option<f64>) -> Cell {
        Cell { value: v.filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 100.0)), estimated: None }
    }

    fn from_factor(f: &Option<Factor<f64>>) -> Cell {
        match f {
            Some(f) => Cell {
                estimated: if f.estimated { Some(EstimateReason::SourceEstimate) } else { None },
                ..Cell::real(Some(f.value))
            },
            None => Cell::default(),
        }
    }
}

/// Piecewise-linear interpolation through `pts` (sorted by x), flat beyond both ends.
pub fn lerp(x: f64, pts: &[(f64, f64)]) -> f64 {
    let Some(&(x0, y0)) = pts.first() else {
        return 0.0;
    };
    if x <= x0 {
        return y0;
    }
    for w in pts.windows(2) {
        let ((xa, ya), (xb, yb)) = (w[0], w[1]);
        if x <= xb {
            if xb <= xa {
                return yb;
            }
            return ya + (yb - ya) * (x - xa) / (xb - xa);
        }
    }
    pts[pts.len() - 1].1
}

/// Cost utility from monthly cost ÷ monthly budget (or price ÷ maximum price).
pub fn cost_utility(ratio: f64) -> f64 {
    lerp(ratio, &COST_CURVE)
}

/// Commute utility for `minutes` (already multiplied by the traffic buffer) against a maximum.
pub fn commute_utility(minutes: f64, max_minutes: f64) -> f64 {
    lerp(minutes, &[(0.5 * max_minutes, 100.0), (max_minutes, 60.0), (1.75 * max_minutes, 0.0)])
}

/// Beds or baths against a need.
pub fn need_fit(have: f64, need: f64) -> f64 {
    if have >= need + 1.0 {
        100.0
    } else if have >= need {
        85.0
    } else if have >= need - 1.0 {
        40.0
    } else {
        0.0
    }
}

/// Size or lot against a target: linear up to the target, capped at 100.
pub fn size_fit(have: f64, target: f64) -> f64 {
    if target <= 0.0 {
        return 100.0;
    }
    (100.0 * have / target).clamp(0.0, 100.0)
}

/// Condition from effective age in years.
pub fn age_utility(age_years: f64) -> f64 {
    lerp(age_years, &AGE_CURVE)
}

/// Layout: 40, plus 25 for an existing ADU (or 10 for strong ADU potential), plus 20 for a
/// single-story home; at most 100.
pub fn layout_utility(has_adu: bool, strong_adu_potential: bool, single_story: bool) -> f64 {
    let adu: f64 = if has_adu {
        25.0
    } else if strong_adu_potential {
        10.0
    } else {
        0.0
    };
    let level: f64 = if single_story { 20.0 } else { 0.0 };
    (40.0_f64 + adu + level).min(100.0)
}

/// Style match: 100 × (Σ probability × preference + unexplained mass × 0.6).
pub fn style_match(probabilities: &BTreeMap<StyleFamily, f64>, preference: impl Fn(StyleFamily) -> f64) -> f64 {
    let known: f64 = probabilities.values().sum::<f64>().clamp(0.0, 1.0);
    let matched: f64 = probabilities.iter().map(|(f, p)| p * preference(*f)).sum();
    (100.0 * (matched + (1.0 - known) * NEUTRAL_STYLE)).clamp(0.0, 100.0)
}

/// Share of wished features found, 0–100.
pub fn feature_match(found: &BTreeSet<Feature>, wishes: &[Feature]) -> f64 {
    let wished: BTreeSet<Feature> = wishes.iter().copied().collect();
    if wished.is_empty() {
        return 0.0;
    }
    100.0 * wished.intersection(found).count() as f64 / wished.len() as f64
}

/// The report's agency crime formula (`summary.rs`), for inputs that arrive as raw rates.
pub fn crime_score(violent_ratio_to_national: f64, property_ratio_to_national: f64) -> f64 {
    (100.0 - (0.6 * violent_ratio_to_national + 0.4 * property_ratio_to_national) * 34.0).clamp(20.0, 96.0)
}

/// California School Dashboard tiers (1–5) to 0–100: mean tier × 20.
pub fn dashboard_tier_score(tiers: &[f64]) -> Option<f64> {
    let t: Vec<f64> = tiers.iter().copied().filter(|t| (1.0..=5.0).contains(t)).collect();
    if t.is_empty() {
        None
    } else {
        Some(t.iter().sum::<f64>() / t.len() as f64 * 20.0)
    }
}

/// Monthly cost for this buyer's financing (`Conventional`, `Fha`, `Va`, `Cash`).
/// Uses `valuation.ownership.scenarios` (`monthly_by_loan`) when available and falls back to
/// scaling principal & interest from the assumed 20% down 30-year baseline.
pub fn monthly_cost_for(profile: &Profile, home: &HomeFactors) -> Option<f64> {
    let by_loan = home.cost.monthly_by_loan.as_ref();
    if profile.loan_type == LoanType::Cash {
        if let Some(cash) = by_loan.and_then(|m| m.cash) {
            return Some(cash.max(0.0));
        }
    }
    let total = home.cost.monthly_total.as_ref()?.value;
    let assumed = home.cost.assumed_down_payment_pct.unwrap_or(DEFAULT_DOWN_PAYMENT_PCT);
    let Some(pi) = home.cost.monthly_principal_interest else {
        return Some(match profile.loan_type {
            LoanType::Conventional => by_loan.and_then(|m| m.conventional).unwrap_or(total),
            LoanType::Fha => by_loan.and_then(|m| m.fha).unwrap_or(total),
            LoanType::Va => by_loan.and_then(|m| m.va).unwrap_or(total),
            LoanType::Cash => by_loan.and_then(|m| m.cash).unwrap_or(total),
        });
    };
    if profile.loan_type == LoanType::Cash {
        return Some((total - pi).max(0.0));
    }
    let down = profile.down_payment_pct.clamp(0.0, 100.0);
    let is_default_down = (down - assumed).abs() < 1e-9;
    match profile.loan_type {
        LoanType::Conventional => {
            let base_total = by_loan.and_then(|m| m.conventional).unwrap_or(total);
            if is_default_down || assumed >= 100.0 {
                Some(base_total)
            } else {
                let scaled = pi * (1.0 - down / 100.0) / (1.0 - assumed / 100.0);
                Some((base_total - pi + scaled).max(0.0))
            }
        }
        LoanType::Fha => {
            if let Some(fha_tot) = by_loan.and_then(|m| m.fha) {
                if is_default_down || (down - 3.5).abs() < 1e-9 {
                    return Some(fha_tot.max(0.0));
                }
            }
            let eff_down = if is_default_down { 3.5 } else { down };
            let ratio = if assumed >= 100.0 {
                0.0
            } else {
                (1.0 - eff_down / 100.0) / (1.0 - assumed / 100.0)
            };
            let fha_pi = pi * ratio * 1.0175 * 0.975;
            let base_loan = home
                .cost
                .price
                .as_ref()
                .or(home.cost.estimate.as_ref())
                .map(|f| f.value * (1.0 - eff_down / 100.0))
                .unwrap_or(0.0);
            let fha_mi = (base_loan * 0.0055) / 12.0;
            Some((total - pi + fha_pi + fha_mi).max(0.0))
        }
        LoanType::Va => {
            if let Some(va_tot) = by_loan.and_then(|m| m.va) {
                if is_default_down || down.abs() < 1e-9 {
                    return Some(va_tot.max(0.0));
                }
            }
            let eff_down = if is_default_down { 0.0 } else { down };
            let ratio = if assumed >= 100.0 {
                0.0
            } else {
                (1.0 - eff_down / 100.0) / (1.0 - assumed / 100.0)
            };
            let va_pi = pi * ratio * 1.0215 * 0.97;
            Some((total - pi + va_pi).max(0.0))
        }
        LoanType::Cash => unreachable!(),
    }
}

fn cost_value(profile: &Profile, home: &HomeFactors) -> Option<f64> {
    let mut parts = Vec::with_capacity(2);
    if let (Some(budget), Some(monthly)) = (profile.budget.monthly.filter(|b| *b > 0.0), monthly_cost_for(profile, home)) {
        parts.push(cost_utility(monthly / budget));
    }
    if let (Some(max), Some(price)) = (profile.budget.max_price.filter(|m| *m > 0.0), home.cost.price_for_budget()) {
        parts.push(cost_utility(price / max));
    }
    mean(&parts)
}

fn schools_value(profile: &Profile, home: &HomeFactors) -> Option<f64> {
    let vals: Vec<f64> = profile
        .effective_school_levels()
        .into_iter()
        .filter_map(|l| home.schools.get(l))
        .map(|f| f.value.score)
        .collect();
    mean(&vals)
}

fn val(f: &Option<Factor<f64>>) -> Option<f64> {
    f.as_ref().map(|f| f.value).filter(|v| v.is_finite())
}

/// Home fit: weighted mean of the available sub-factors (prototype v3). With the multigenerational
/// flag an existing ADU adds its size and baths, layout weighs 0.40 instead of 0.15, and
/// ground-floor bed/bath, main-level primary, and parking signals contribute when present.
pub fn homefit_value(profile: &Profile, home: &HomeFactors, reference_year: i32) -> Option<f64> {
    let h = &home.home;
    let adu = h.adu.as_ref().map(|f| &f.value);
    let multigen_adu = if profile.multigenerational { adu } else { None };
    let mut parts: Vec<(f64, f64)> = Vec::with_capacity(7);
    if let (Some(need), Some(have)) = (profile.min_beds, val(&h.beds)) {
        parts.push((HOMEFIT_BEDS, need_fit(have, need)));
    }
    if let (Some(need), Some(have)) = (profile.min_baths, val(&h.baths)) {
        let extra = multigen_adu.and_then(|a| a.baths).unwrap_or(0.0);
        parts.push((HOMEFIT_BATHS, need_fit(have + extra, need)));
    }
    if let (Some(target), Some(have)) = (profile.min_sqft, val(&h.sqft)) {
        let extra = multigen_adu.and_then(|a| a.sqft).unwrap_or(0.0);
        parts.push((HOMEFIT_SPACE, size_fit(have + extra, target)));
    }
    if let (Some(target), Some(have)) = (profile.min_lot_sqft, val(&h.lot_sqft)) {
        parts.push((HOMEFIT_LOT, size_fit(have, target)));
    }
    if let Some(year) = h.effective_year.as_ref().or(h.year_built.as_ref()).map(|f| f.value) {
        let age = f64::from((reference_year - year).max(0));
        parts.push((HOMEFIT_CONDITION, age_utility(age)));
    }

    let has_main_level_primary = home
        .features
        .as_ref()
        .map_or(false, |f| f.value.contains(&Feature::MainLevelPrimary));
    let main_beds = h.main_level_bedrooms.filter(|v| v.is_finite() && *v >= 0.0);
    let main_baths = h.main_level_bathrooms.filter(|v| v.is_finite() && *v >= 0.0);

    if let Some(stories) = val(&h.stories) {
        let single_story = stories < 1.5;
        let mut layout = layout_utility(h.has_adu(), h.strong_adu_potential(), single_story);
        if profile.multigenerational {
            match (main_beds, main_baths) {
                (Some(mb), Some(mba)) if mb >= 1.0 && mba >= 1.0 => {
                    layout = (layout + if single_story { 5.0 } else { 15.0 }).min(100.0);
                }
                (Some(mb), _) if mb >= 1.0 => {
                    layout = (layout + if single_story { 2.0 } else { 8.0 }).min(100.0);
                }
                (_, Some(mba)) if mba >= 1.0 => {
                    layout = (layout + if single_story { 2.0 } else { 6.0 }).min(100.0);
                }
                _ => {}
            }
        }
        let w = if profile.multigenerational { HOMEFIT_LAYOUT_MULTIGEN } else { HOMEFIT_LAYOUT };
        parts.push((w, layout));
    } else if profile.multigenerational
        && (has_main_level_primary
            || main_beds.is_some()
            || main_baths.is_some()
            || h.has_adu()
            || h.strong_adu_potential())
    {
        let mut layout = layout_utility(h.has_adu(), h.strong_adu_potential(), false);
        if main_beds.unwrap_or(0.0) >= 1.0 && main_baths.unwrap_or(0.0) >= 1.0 {
            layout = (layout + 15.0).min(100.0);
        } else if has_main_level_primary || main_beds.unwrap_or(0.0) >= 1.0 {
            layout = (layout + 12.0).min(100.0);
        }
        parts.push((HOMEFIT_LAYOUT_MULTIGEN, layout));
    }
    if profile.multigenerational {
        if let Some(pk) = val(&h.parking_spaces) {
            let parking_u = lerp(
                pk,
                &[(0.0, 20.0), (1.0, 45.0), (2.0, 75.0), (3.0, 92.0), (4.0, 100.0)],
            );
            parts.push((HOMEFIT_PARKING_MULTIGEN, parking_u));
        }
    }
    let wsum: f64 = parts.iter().map(|(w, _)| w).sum();
    if wsum <= 0.0 {
        return None;
    }
    Some(parts.iter().map(|(w, v)| w * v).sum::<f64>() / wsum)
}

/// Style and character. Scored only when the buyer picked a style or wished for a feature. A home
/// with no style evidence (a "likely" era prior does not count) gets a neutral value flagged as an
/// estimate.
pub fn style_cell(profile: &Profile, home: &HomeFactors) -> Cell {
    let has_prefs = !profile.style_preferences.is_empty();
    let has_wishes = !profile.feature_wishes.is_empty();
    let style_part = if has_prefs {
        if home.style.is_evidence_backed() {
            Some(style_match(&home.style.probabilities, |f| profile.style_value(f)))
        } else {
            Some(100.0 * NEUTRAL_STYLE)
        }
    } else {
        None
    };
    let feature_part = if has_wishes {
        home.features.as_ref().map(|f| feature_match(&f.value, &profile.feature_wishes))
    } else {
        None
    };
    let style_estimated =
        if home.style.is_evidence_backed() { None } else { Some(EstimateReason::NoStyleEvidence) };
    match (style_part, feature_part) {
        (Some(s), Some(f)) => Cell {
            value: Some(STYLE_MATCH_SHARE * s + (1.0 - STYLE_MATCH_SHARE) * f),
            estimated: style_estimated,
        },
        (Some(s), None) => Cell { value: Some(s), estimated: style_estimated },
        (None, Some(f)) => Cell::real(Some(f)),
        (None, None) => Cell::default(),
    }
}

fn mean(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        None
    } else {
        Some(v.iter().sum::<f64>() / v.len() as f64)
    }
}

/// Utilities of every dimension except commute (the engine adds it from the commute matrix).
pub fn home_utilities(profile: &Profile, home: &HomeFactors, reference_year: i32) -> [Cell; Dimension::COUNT] {
    let mut c = [Cell::default(); Dimension::COUNT];
    c[Dimension::Cost.index()] = Cell::real(cost_value(profile, home));
    c[Dimension::Schools.index()] = Cell::real(schools_value(profile, home));
    c[Dimension::Convenience.index()] = Cell::from_factor(&home.convenience);
    c[Dimension::Homefit.index()] = Cell::real(homefit_value(profile, home, reference_year));
    c[Dimension::Style.index()] = style_cell(profile, home);
    c[Dimension::Hazards.index()] = Cell::from_factor(&home.hazards);
    c[Dimension::Crime.index()] = Cell::from_factor(&home.crime);
    c[Dimension::Environment.index()] = Cell::from_factor(&home.environment);
    c[Dimension::Investment.index()] = Cell::from_factor(&home.investment);
    c[Dimension::Fengshui.index()] = Cell::from_factor(&home.fengshui);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_match_the_plan() {
        assert_eq!(cost_utility(0.7), 100.0);
        assert_eq!(cost_utility(0.8), 100.0);
        assert!((cost_utility(1.0) - 70.0).abs() < 1e-9);
        assert_eq!(cost_utility(1.3), 0.0);
        assert!((cost_utility(16496.0 / 16000.0) - 61.32).abs() < 1e-9);
        assert_eq!(commute_utility(15.0, 30.0), 100.0);
        assert!((commute_utility(30.0, 30.0) - 60.0).abs() < 1e-9);
        assert_eq!(commute_utility(52.5, 30.0), 0.0);
        assert_eq!(need_fit(5.0, 4.0), 100.0);
        assert_eq!(need_fit(4.0, 4.0), 85.0);
        assert_eq!(need_fit(3.0, 4.0), 40.0);
        assert_eq!(need_fit(2.0, 4.0), 0.0);
        assert_eq!(size_fit(2400.0, 1900.0), 100.0);
        assert!((size_fit(1956.0, 2400.0) - 81.5).abs() < 1e-9);
        assert_eq!(age_utility(3.0), 100.0);
        assert!((age_utility(20.0) - 75.0).abs() < 1e-9);
        assert!((age_utility(66.0) - 39.333_333_333).abs() < 1e-6);
        assert_eq!(age_utility(120.0), 30.0);
        assert_eq!(layout_utility(true, true, true), 85.0);
        assert_eq!(layout_utility(false, true, false), 50.0);
        assert_eq!(crime_score(505.3 / 329.6, 2403.6 / 1552.3).round(), 48.0);
        assert_eq!(dashboard_tier_score(&[4.0, 3.0, 4.0, 5.0, 5.0]), Some(84.0));
    }

    #[test]
    fn style_match_counts_unknown_mass_as_fine() {
        let probs = BTreeMap::from([(StyleFamily::Craftsman, 0.75)]);
        let love = |f: StyleFamily| if f == StyleFamily::Craftsman { 1.0 } else { NEUTRAL_STYLE };
        assert!((style_match(&probs, love) - 90.0).abs() < 1e-9);
        assert!((style_match(&BTreeMap::new(), love) - 60.0).abs() < 1e-9);
    }

    #[test]
    fn era_prior_alone_scores_neutral_and_is_estimated() {
        use super::super::catalog::StylePreference;
        use super::super::factors::{StyleEvidence, StyleSource};

        let mut profile = Profile::default();
        profile.style_preferences.insert(StyleFamily::Traditional, StylePreference::Love);

        let mut home = HomeFactors::default();
        home.style = StyleEvidence {
            probabilities: BTreeMap::from([(StyleFamily::Traditional, 0.30)]),
            basis: Some(StyleSource::EraPrior),
            clues: vec![],
        };
        let cell = style_cell(&profile, &home);
        assert_eq!(cell.value, Some(60.0));
        assert_eq!(cell.estimated, Some(EstimateReason::NoStyleEvidence));
    }
}
