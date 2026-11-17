//! Scoring inputs for one home, extracted from a Open Estate report (plan v3, sections 5.2 and 5.4-5.7).
//!
//! [`HomeFactors::from_report`] reads an allow-list of report fields and nothing else. Every value
//! carries provenance ([`Factor`]): its source, the data year where known, a grade (`listing`, `area`
//! or `report`) and whether it is an estimate.
//!
//! - **Public school data only.** Values come from `modules.civic.data.schools.public.list[]`: the
//!   within-state `percentile` when present, otherwise the California School Dashboard tiers.
//!   `facts.schools.assigned[]` contributes school *names* only; its `rating`, `gsRating` and
//!   `nicheGrade` values are never read, nor are the civic `score` and `staffingScore` fields.
//! - **No resident or demographic data** is read anywhere (plan section 6, rule 2).
//! - The serialized form (`report.factors`) holds no owner, agent or contact data.

use super::catalog::{style_families_for_value, Feature, PropertyKind, SchoolLevel, StyleFamily};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Version of the serialized [`HomeFactors`] document.
pub const FACTORS_VERSION: u32 = 1;

/// Probability mass each style source can explain (plan 5.5: trust high, medium-high, medium, low).
pub const STYLE_TRUST_MLS: f64 = 0.95;
pub const STYLE_TRUST_COUNTY: f64 = 0.85;
/// Listing-text keywords; the prototype used 0.75 for "craftsman".
pub const STYLE_TRUST_TEXT: f64 = 0.75;
/// Era and form prior, shown as "likely".
pub const STYLE_TRUST_ERA: f64 = 0.30;
/// 500 ft in metres, for the `freeway_within_500ft` screen.
pub const FREEWAY_SCREEN_M: f64 = 152.4;

static NULL: Value = Value::Null;

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// How a value was obtained (plan 5.8): from listing fields, from shared area data (tile, tract,
/// city or ZIP level), or computed for this exact home by a full report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    Listing,
    Area,
    #[default]
    Report,
}

/// A value with its provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Factor<T> {
    pub value: T,
    pub source: String,
    /// Data year where known. School years such as 2024-25 are recorded as their ending year.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    pub grade: Grade,
    #[serde(default, skip_serializing_if = "is_false")]
    pub estimated: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl<T> Factor<T> {
    pub fn new(value: T, source: impl Into<String>, grade: Grade) -> Self {
        Factor { value, source: source.into(), year: None, grade, estimated: false }
    }

    pub fn with_year(mut self, year: Option<i32>) -> Self {
        self.year = year;
        self
    }

    pub fn mark_estimated(mut self) -> Self {
        self.estimated = true;
        self
    }
}

/// Every input the "Your Match" utilities need for one home.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeFactors {
    /// Stable id: the report slug, or the listing key when built from a listing.
    pub id: String,
    pub version: u32,
    /// Year the data describes (report generation year); home age is measured against it.
    pub as_of_year: i32,
    pub cost: CostFactors,
    pub schools: SchoolFactors,
    /// City-level crime score, 0-100 (higher = lower reported rates), as in `summary.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crime: Option<Factor<f64>>,
    pub home: HomeFitFactors,
    /// 0.6 x amenity access + 0.4 x walkability, 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convenience: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hazards: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub investment: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fengshui: Option<Factor<f64>>,
    pub gates: GateInputs,
    pub style: StyleEvidence,
    /// Wished-for character features found in MLS fields and listing text. `None` when the home has
    /// no descriptive text at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Factor<BTreeSet<Feature>>>,
}

/// Per-loan-program all-in monthly ownership costs (from `valuation.ownership.scenarios`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoanMonthlyCosts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conventional: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fha: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub va: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cash: Option<f64>,
}

/// Cost inputs. The utility uses the monthly total; the rest is kept for later sub-factors.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostFactors {
    /// All-in monthly cost from the report's ownership model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_total: Option<Factor<f64>>,
    /// Principal and interest inside `monthly_total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_principal_interest: Option<f64>,
    /// Down payment the monthly total assumes, in percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assumed_down_payment_pct: Option<f64>,
    /// Price the cost model is based on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Factor<f64>>,
    /// Open Estate value estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_price: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hoa_monthly: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cash_to_close: Option<f64>,
    /// All-in monthly cost breakdown by loan program (`conventional`, `fha`, `va`, `cash`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_by_loan: Option<LoanMonthlyCosts>,
}

impl CostFactors {
    /// Price compared with a maximum-price budget: list price, else the cost-model price, else the
    /// estimate.
    pub fn price_for_budget(&self) -> Option<f64> {
        self.list_price
            .as_ref()
            .or(self.price.as_ref())
            .or(self.estimate.as_ref())
            .map(|f| f.value)
    }
}

/// How a school value was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchoolMetric {
    /// Within-state percentile of the share meeting standard (plan 5.6).
    StatePercentile,
    /// Mean California School Dashboard tier (1-5) x 20.
    CaDashboardTiers,
}

/// The assigned school at one level and its 0-100 value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchoolScore {
    pub name: String,
    pub score: f64,
    pub metric: SchoolMetric,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchoolFactors {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elementary: Option<Factor<SchoolScore>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middle: Option<Factor<SchoolScore>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<Factor<SchoolScore>>,
}

impl SchoolFactors {
    pub fn get(&self, level: SchoolLevel) -> Option<&Factor<SchoolScore>> {
        match level {
            SchoolLevel::Elementary => self.elementary.as_ref(),
            SchoolLevel::Middle => self.middle.as_ref(),
            SchoolLevel::High => self.high.as_ref(),
        }
    }

    pub fn set(&mut self, level: SchoolLevel, f: Factor<SchoolScore>) {
        match level {
            SchoolLevel::Elementary => self.elementary = Some(f),
            SchoolLevel::Middle => self.middle = Some(f),
            SchoolLevel::High => self.high = Some(f),
        }
    }
}

/// An existing accessory dwelling unit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Adu {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqft: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baths: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeFitFactors {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_type: Option<Factor<PropertyKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beds: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baths: Option<Factor<f64>>,
    /// Living area of the main home, square feet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqft: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lot_sqft: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stories: Option<Factor<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year_built: Option<Factor<i32>>,
    /// Assessor effective year (major remodel); condition uses it instead of the year built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_year: Option<Factor<i32>>,
    /// Existing ADU, when the listing describes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adu: Option<Factor<Adu>>,
    /// ADU potential label from `decisionFactors.lotAndAdu.feasibility`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adu_feasibility: Option<Factor<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_level_bedrooms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_level_bathrooms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parking_spaces: Option<Factor<f64>>,
}

impl HomeFitFactors {
    pub fn has_adu(&self) -> bool {
        self.adu.is_some()
    }

    /// ADU feasibility rated "Strong".
    pub fn strong_adu_potential(&self) -> bool {
        self.adu_feasibility
            .as_ref()
            .map(|f| f.value.starts_with("Strong"))
            .unwrap_or(false)
    }
}

/// Inputs for the deal-breaker and must-have screens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateInputs {
    /// In a FEMA Special Flood Hazard Area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sfha: Option<Factor<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flood_zone: Option<String>,
    /// In a very high fire hazard severity zone (or USFS WHP "Very High").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub very_high_fire: Option<Factor<bool>>,
    /// Distance to the nearest mapped freeway, metres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeway_distance_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeway_within_500ft: Option<Factor<bool>>,
    /// A street points straight at the house (路冲).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_junction: Option<Factor<bool>>,
    /// Street number of the property itself (digits only), for the address-digit screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub house_number: Option<String>,
}

/// Where a style clue came from, in decreasing order of trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StyleSource {
    /// MLS `ArchitecturalStyle` field (high).
    Mls,
    /// County-record style such as assessor `architectureType` (medium-high).
    CountyRecord,
    /// Keywords in listing remarks, features, renovations or property type (medium).
    ListingText,
    /// Build era and form (low; shown as "likely").
    EraPrior,
}

impl StyleSource {
    pub fn trust(self) -> f64 {
        match self {
            StyleSource::Mls => STYLE_TRUST_MLS,
            StyleSource::CountyRecord => STYLE_TRUST_COUNTY,
            StyleSource::ListingText => STYLE_TRUST_TEXT,
            StyleSource::EraPrior => STYLE_TRUST_ERA,
        }
    }
}

/// One piece of style evidence, shown to the buyer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StyleClue {
    pub source: StyleSource,
    /// The raw value or matched keyword ("Craftsman"), or the era description for a prior.
    pub raw: String,
    pub families: Vec<StyleFamily>,
}

/// Probability per style family. Only the most trusted source with evidence sets the probabilities
/// (its trust is the mass it explains, split evenly over the families it names); the rest of the
/// mass stays unknown. An era prior is used only when no other source has evidence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StyleEvidence {
    pub probabilities: BTreeMap<StyleFamily, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<StyleSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clues: Vec<StyleClue>,
}

impl StyleEvidence {
    /// True when the probabilities come from MLS, county records or listing text, not only a prior.
    pub fn is_evidence_backed(&self) -> bool {
        matches!(
            self.basis,
            Some(StyleSource::Mls | StyleSource::CountyRecord | StyleSource::ListingText)
        )
    }

    /// Total explained probability mass, 0-1.
    pub fn known_mass(&self) -> f64 {
        self.probabilities.values().sum::<f64>().clamp(0.0, 1.0)
    }

    pub fn probability(&self, family: StyleFamily) -> f64 {
        self.probabilities.get(&family).copied().unwrap_or(0.0)
    }

    /// Most probable family backed by evidence; `None` when the style is unknown (a prior alone
    /// does not count).
    pub fn top_family(&self) -> Option<StyleFamily> {
        if self.is_evidence_backed() {
            self.likely_family()
        } else {
            None
        }
    }

    /// Most probable family including an era prior ("likely ...").
    pub fn likely_family(&self) -> Option<StyleFamily> {
        let mut best: Option<(StyleFamily, f64)> = None;
        for (f, p) in &self.probabilities {
            if best.map_or(true, |(_, bp)| *p > bp) {
                best = Some((*f, *p));
            }
        }
        best.map(|(f, _)| f)
    }
}

// ---------------------------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------------------------

impl HomeFactors {
    /// Extracts every scoring input from a report. Never panics; missing data stays `None`.
    pub fn from_report(report: &Value) -> HomeFactors {
        let (brand_name, _) = crate::shared::report_brand(report);
        let facts = report.get("facts").filter(|v| v.is_object()).unwrap_or(&NULL);
        let as_of_year = report_year(report);
        let home = extract_home(report, facts, as_of_year);
        let style = extract_style(
            facts,
            home.year_built.as_ref().map(|f| f.value),
            home.stories.as_ref().map(|f| f.value),
        );
        let features = extract_features(facts, home.stories.as_ref().map(|f| f.value));
        HomeFactors {
            id: report
                .pointer("/meta/slug")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            version: FACTORS_VERSION,
            as_of_year,
            cost: extract_cost(report, facts),
            schools: extract_schools(report, facts),
            crime: extract_crime(report),
            convenience: extract_convenience(report),
            hazards: module_num(report, "hazards", "/scores/overall").map(|v| {
                Factor::new(
                    v,
                    format!("{brand_name} natural-hazard score (FEMA flood, USGS seismic, wildfire, FEMA NRI)"),
                    Grade::Report,
                )
            }),
            environment: module_num(report, "environment", "/scores/overall").map(|v| {
                Factor::new(
                    v,
                    format!("{brand_name} environment score (contamination, air, water, noise)"),
                    Grade::Report,
                )
            }),
            investment: report
                .pointer("/summary/scores/investment")
                .and_then(|v| num(Some(v)))
                .map(|v| {
                    Factor::new(
                        v,
                        format!("{brand_name} investment score (ZIP appreciation, yield, price-to-income)"),
                        Grade::Area,
                    )
                }),
            fengshui: module_num(report, "fengshui", "/scores/overall")
                .or_else(|| module_num(report, "fengshui", "/score"))
                .map(|v| {
                    Factor::new(
                        v,
                        format!("{brand_name} feng shui reading (form and flying stars)"),
                        Grade::Report,
                    )
                }),
            gates: extract_gates(report, facts),
            home,
            style,
            features,
        }
    }

    /// Compact JSON for `report.factors`: `None` fields are omitted and no personal data is included.
    pub fn to_report_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    ///Canonical [`ListingRow`](crate::db::ListingRow) -> [`HomeFactors`] conversion for listings
    /// without a full cached report or pre-computed `listing_factors` row.
    ///
    /// Operates directly on typed `ListingRow` columns (zero `raw_json` parsing and zero
    /// intermediate `serde_json::Value` allocation) and aligns provenance cleanly with
    /// [`HomeFactorsRowProto`](crate::db::HomeFactorsRowProto) /
    /// [`FactorProvenanceProto`](crate::db::FactorProvenanceProto).
    pub fn from_listing_row(listing: &crate::db::ListingRow) -> HomeFactors {
        const FACTS_SOURCE: &str = "Report facts (MLS, county records, listing research)";
        let as_of_year = current_year();

        let list_price_num = listing.list_price.or(listing.price).filter(|&p| p > 0);
        let (monthly_total, monthly_principal_interest, assumed_down_payment_pct, model_price) =
            if let Some(p) = list_price_num {
                let (total, pi) = estimate_listing_monthly_ownership(
                    p,
                    listing.hoa_monthly.unwrap_or(0),
                    listing.state.as_deref(),
                );
                let mt = (total > 0.0).then(|| {
                    Factor::new(
                        total,
                        "Open Estate cost model (20% down, 30-year fixed at 6.5%)",
                        Grade::Area,
                    )
                    .mark_estimated()
                });
                let pr = Factor::new(p as f64, "Open Estate cost-model price", Grade::Report);
                (mt, Some(pi), Some(20.0), Some(pr))
            } else {
                let pr = listing
                    .price
                    .filter(|&p| p > 0)
                    .map(|p| Factor::new(p as f64, "Listing price", Grade::Listing));
                (None, None, None, pr)
            };

        let cost = CostFactors {
            monthly_total,
            monthly_principal_interest,
            assumed_down_payment_pct,
            price: model_price,
            estimate: listing
                .estimated_value
                .filter(|&ev| ev > 0)
                .map(|ev| Factor::new(ev as f64, "Open Estate value estimate", Grade::Report)),
            list_price: list_price_num
                .map(|p| Factor::new(p as f64, "MLS list price", Grade::Listing)),
            hoa_monthly: listing
                .hoa_monthly
                .filter(|&h| h >= 0)
                .map(|h| Factor::new(h as f64, "Listing and public records", Grade::Listing)),
            cash_to_close: None,
            monthly_by_loan: None,
        };

        let year_built = listing
            .year_built
            .map(|y| y as i32)
            .filter(|y| (1600..=as_of_year + 2).contains(y))
            .map(|y| Factor::new(y, FACTS_SOURCE, Grade::Listing));
        let stories = listing
            .stories
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| Factor::new(s, FACTS_SOURCE, Grade::Listing));

        let yb_val = year_built.as_ref().map(|f| f.value);
        let stories_val = stories.as_ref().map(|f| f.value);
        let valid_eff_year =
            |y: i32| y >= 1800 && y <= as_of_year + 1 && yb_val.map_or(true, |b| y >= b);
        let effective_year = [
            listing.property_type.as_deref(),
            listing.features_text.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find_map(|t| {
            EFFECTIVE_YEAR_RE
                .captures(t)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .filter(|y| valid_eff_year(*y))
                .map(|y| {
                    Factor::new(
                        y,
                        "Assessor effective year quoted in the listing facts",
                        Grade::Listing,
                    )
                })
        });

        let adu = extract_adu_from_listing_fields(
            listing.features_text.as_deref(),
            listing.property_type.as_deref(),
            listing.has_adu == Some(1),
        );
        let adu_feasibility = (listing.adu_feasible == Some(1)).then(|| {
            Factor::new(
                "Strong".to_string(),
                "Open Estate lot and ADU screen (open lot area, state ADU law)",
                Grade::Listing,
            )
        });

        let home = HomeFitFactors {
            property_type: listing
                .property_type
                .as_deref()
                .and_then(PropertyKind::classify)
                .map(|k| Factor::new(k, FACTS_SOURCE, Grade::Listing)),
            beds: listing
                .beds
                .filter(|b| b.is_finite() && *b >= 0.0)
                .map(|b| Factor::new(b, FACTS_SOURCE, Grade::Listing)),
            baths: listing
                .baths
                .filter(|b| b.is_finite() && *b >= 0.0)
                .map(|b| Factor::new(b, FACTS_SOURCE, Grade::Listing)),
            sqft: listing
                .sqft
                .filter(|&s| s > 0)
                .map(|s| Factor::new(s as f64, FACTS_SOURCE, Grade::Listing)),
            lot_sqft: listing
                .lot_sqft
                .filter(|&s| s > 0)
                .map(|s| Factor::new(s as f64, FACTS_SOURCE, Grade::Listing)),
            stories,
            year_built,
            effective_year,
            adu,
            adu_feasibility,
            main_level_bedrooms: None,
            main_level_bathrooms: None,
            parking_spaces: listing
                .garage_spaces
                .filter(|p| p.is_finite() && *p >= 0.0)
                .map(|p| Factor::new(p, FACTS_SOURCE, Grade::Listing)),
        };

        let style = extract_style_from_listing_fields(
            listing.architectural_style.as_deref(),
            listing.property_type.as_deref(),
            listing.features_text.as_deref(),
            yb_val,
            stories_val,
        );

        let features = extract_features_from_listing_fields(
            listing.features_text.as_deref(),
            listing.has_pool == Some(1),
            listing.has_ev_charger == Some(1),
            listing.has_hardwood_floors == Some(1),
            listing.tags_json.as_deref(),
            stories_val,
        );

        let convenience = listing
            .walk_score
            .filter(|w| w.is_finite() && (0.0..=100.0).contains(w))
            .map(|ws| Factor::new(ws, "EPA National Walkability Index", Grade::Area));

        let crime = listing
            .safety_score
            .filter(|s| s.is_finite() && (0.0..=100.0).contains(s))
            .map(|sc| Factor::new(sc, "FBI Crime Data Explorer, agency rates", Grade::Area));

        let (sfha, flood_zone) = match listing
            .flood_zone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(fz) => {
                let u = fz.to_ascii_uppercase();
                let is_sfha = matches!(
                    u.as_str(),
                    "AE" | "A" | "AO" | "AH" | "VE" | "V" | "AR" | "A99"
                ) || u.starts_with("A ")
                    || u.starts_with("V ");
                (
                    Some(Factor::new(
                        is_sfha,
                        "FEMA National Flood Hazard Layer",
                        Grade::Area,
                    )),
                    Some(fz.to_string()),
                )
            }
            None => (None, None),
        };

        let very_high_fire = listing
            .wildfire_zone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|wf| {
                let low = wf.to_ascii_lowercase();
                let vh = low.contains("very high") && !low.starts_with("not");
                Factor::new(vh, "USFS Wildfire Hazard Potential", Grade::Area)
            });

        let gates = GateInputs {
            sfha,
            flood_zone,
            very_high_fire,
            freeway_distance_m: None,
            freeway_within_500ft: None,
            t_junction: None,
            house_number: house_number_from_address(&listing.address),
        };

        let mut schools = SchoolFactors::default();
        let mut set_school = |lvl: SchoolLevel, name: Option<&str>, rating_10: Option<f64>| {
            if let Some(r) = rating_10.filter(|v| v.is_finite() && *v > 0.0) {
                let pct = (r * 10.0).clamp(0.0, 100.0);
                let nm = name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Assigned Public School");
                schools.set(
                    lvl,
                    Factor::new(
                        SchoolScore {
                            name: nm.to_string(),
                            score: pct,
                            metric: SchoolMetric::StatePercentile,
                        },
                        "Public school record",
                        Grade::Area,
                    ),
                );
            }
        };
        set_school(
            SchoolLevel::Elementary,
            listing.school_elem_name.as_deref(),
            listing.school_elem_rating,
        );
        set_school(
            SchoolLevel::Middle,
            listing.school_mid_name.as_deref(),
            listing.school_mid_rating,
        );
        set_school(
            SchoolLevel::High,
            listing.school_high_name.as_deref(),
            listing.school_high_rating,
        );

        if let Some(sc) = listing.school_score.filter(|v| v.is_finite() && *v > 0.0) {
            let fallback_pct = (sc * 10.0).clamp(0.0, 100.0);
            for (lvl, nm_opt) in [
                (SchoolLevel::Elementary, listing.school_elem_name.as_deref()),
                (SchoolLevel::Middle, listing.school_mid_name.as_deref()),
                (SchoolLevel::High, listing.school_high_name.as_deref()),
            ] {
                if schools.get(lvl).is_none() {
                    let nm = nm_opt
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Assigned Public School");
                    schools.set(
                        lvl,
                        Factor::new(
                            SchoolScore {
                                name: nm.to_string(),
                                score: fallback_pct,
                                metric: SchoolMetric::StatePercentile,
                            },
                            "Public school score (listing record)",
                            Grade::Area,
                        ),
                    );
                }
            }
        }

        HomeFactors {
            id: listing.id.clone(),
            version: FACTORS_VERSION,
            as_of_year,
            cost,
            schools,
            crime,
            convenience,
            hazards: None,
            environment: None,
            investment: None,
            fengshui: None,
            gates,
            home,
            style,
            features,
        }
    }

    /// Narrow [`ListingCoreProto`](crate::db::ListingCoreProto) -> [`HomeFactors`] conversion for
    /// cold-fallback scoring directly off the 42-column core `listings` table without 7-table hydration.
    pub fn from_listing_core(core: &crate::db::ListingCoreProto) -> HomeFactors {
        use crate::db::proto::{feature_tag_bit, property_category_from_code, FeatureTag};

        const FACTS_SOURCE: &str = "Report facts (MLS, county records, listing research)";
        let as_of_year = current_year();

        let list_price_num = core.list_price.or(core.price).filter(|&p| p > 0);
        let hoa_i64 = core.hoa_monthly.map(i64::from);
        let (monthly_total, monthly_principal_interest, assumed_down_payment_pct, model_price) =
            if let Some(am) = core.all_in_monthly.filter(|&m| m > 0) {
                let mt = Factor::new(
                    am as f64,
                    "Open Estate cost model",
                    Grade::Area,
                )
                .mark_estimated();
                let pr = list_price_num
                    .map(|p| Factor::new(p as f64, "Open Estate cost-model price", Grade::Report));
                (Some(mt), None, Some(20.0), pr)
            } else if let Some(p) = list_price_num {
                let (total, pi) = estimate_listing_monthly_ownership(
                    p,
                    hoa_i64.unwrap_or(0),
                    core.state_code.as_deref(),
                );
                let mt = (total > 0.0).then(|| {
                    Factor::new(
                        total,
                        "Open Estate cost model (20% down, 30-year fixed at 6.5%)",
                        Grade::Area,
                    )
                    .mark_estimated()
                });
                let pr = Factor::new(p as f64, "Open Estate cost-model price", Grade::Report);
                (mt, Some(pi), Some(20.0), Some(pr))
            } else {
                let pr = core
                    .price
                    .filter(|&p| p > 0)
                    .map(|p| Factor::new(p as f64, "Listing price", Grade::Listing));
                (None, None, None, pr)
            };

        let cost = CostFactors {
            monthly_total,
            monthly_principal_interest,
            assumed_down_payment_pct,
            price: model_price,
            estimate: core
                .estimated_value
                .filter(|&ev| ev > 0)
                .map(|ev| Factor::new(ev as f64, "Open Estate value estimate", Grade::Report)),
            list_price: list_price_num
                .map(|p| Factor::new(p as f64, "MLS list price", Grade::Listing)),
            hoa_monthly: hoa_i64
                .filter(|&h| h >= 0)
                .map(|h| Factor::new(h as f64, "Listing and public records", Grade::Listing)),
            cash_to_close: None,
            monthly_by_loan: None,
        };

        let year_built = core
            .year_built
            .filter(|y| (1600..=as_of_year + 2).contains(y))
            .map(|y| Factor::new(y, FACTS_SOURCE, Grade::Listing));
        let stories = core
            .stories
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| Factor::new(s, FACTS_SOURCE, Grade::Listing));

        let has_tag = |t: FeatureTag| (core.tag_bits & feature_tag_bit(t)) != 0;
        let known_tag = |t: FeatureTag| (core.known_bits & feature_tag_bit(t)) != 0;

        let home = HomeFitFactors {
            property_type: property_category_from_code(core.property_category)
                .and_then(PropertyKind::classify)
                .map(|k| Factor::new(k, FACTS_SOURCE, Grade::Listing)),
            beds: core
                .beds
                .filter(|b| b.is_finite() && *b >= 0.0)
                .map(|b| Factor::new(b, FACTS_SOURCE, Grade::Listing)),
            baths: core
                .baths
                .filter(|b| b.is_finite() && *b >= 0.0)
                .map(|b| Factor::new(b, FACTS_SOURCE, Grade::Listing)),
            sqft: core
                .sqft
                .filter(|&s| s > 0)
                .map(|s| Factor::new(s as f64, FACTS_SOURCE, Grade::Listing)),
            lot_sqft: core
                .lot_sqft
                .filter(|&s| s > 0)
                .map(|s| Factor::new(s as f64, FACTS_SOURCE, Grade::Listing)),
            stories,
            year_built,
            effective_year: None,
            adu: has_tag(FeatureTag::Adu)
                .then(|| Factor::new(Adu::default(), "Listing features", Grade::Listing)),
            adu_feasibility: has_tag(FeatureTag::AduFeasible).then(|| {
                Factor::new(
                    "Strong".to_string(),
                    "Open Estate lot and ADU screen (open lot area, state ADU law)",
                    Grade::Listing,
                )
            }),
            main_level_bedrooms: None,
            main_level_bathrooms: None,
            parking_spaces: None,
        };

        let yb_val = home.year_built.as_ref().map(|f| f.value);
        let st_val = home.stories.as_ref().map(|f| f.value);
        let style = extract_style_from_listing_fields(None, None, None, yb_val, st_val);

        let mut feat_set = BTreeSet::new();
        for (tag, feat) in [
            (FeatureTag::OpenPlan, Feature::OpenPlan),
            (FeatureTag::HighCeilings, Feature::HighCeilings),
            (FeatureTag::NaturalLight, Feature::NaturalLight),
            (FeatureTag::HardwoodFloors, Feature::Hardwood),
            (FeatureTag::Fireplace, Feature::Fireplace),
            (FeatureTag::Pool, Feature::Pool),
            (FeatureTag::View, Feature::View),
            (FeatureTag::SingleStory, Feature::MainLevelPrimary),
            (FeatureTag::HomeOffice, Feature::Office),
            (FeatureTag::WorkshopGarage, Feature::WorkshopGarage),
            (FeatureTag::EvCharger, Feature::EvReady),
        ] {
            if has_tag(tag) {
                feat_set.insert(feat);
            }
        }
        let features = if !feat_set.is_empty() || core.known_bits != 0 {
            if st_val.map_or(false, |s| s < 1.5) {
                feat_set.insert(Feature::MainLevelPrimary);
            }
            Some(Factor::new(
                feat_set,
                "MLS feature fields and listing text",
                Grade::Listing,
            ))
        } else {
            None
        };

        let mut schools = SchoolFactors::default();
        if let Some(sc) = core.school_score.filter(|v| v.is_finite() && *v > 0.0) {
            let pct = (sc * 10.0).clamp(0.0, 100.0);
            for lvl in SchoolLevel::ALL {
                schools.set(
                    lvl,
                    Factor::new(
                        SchoolScore {
                            name: "Assigned Public School".to_string(),
                            score: pct,
                            metric: SchoolMetric::StatePercentile,
                        },
                        "Public school score (listing record)",
                        Grade::Area,
                    ),
                );
            }
        }

        let sfha = if known_tag(FeatureTag::InSfhaFlood) || has_tag(FeatureTag::InSfhaFlood) {
            Some(Factor::new(
                has_tag(FeatureTag::InSfhaFlood),
                "FEMA National Flood Hazard Layer",
                Grade::Area,
            ))
        } else {
            None
        };
        let very_high_fire =
            if known_tag(FeatureTag::HighWildfire) || has_tag(FeatureTag::HighWildfire) {
                Some(Factor::new(
                    has_tag(FeatureTag::HighWildfire),
                    "USFS Wildfire Hazard Potential",
                    Grade::Area,
                ))
            } else {
                None
            };

        HomeFactors {
            id: core.external_id.clone(),
            version: FACTORS_VERSION,
            as_of_year,
            cost,
            schools,
            crime: core
                .safety_score
                .filter(|s| s.is_finite() && (0.0..=100.0).contains(s))
                .map(|sc| Factor::new(sc, "FBI Crime Data Explorer, agency rates", Grade::Area)),
            convenience: core
                .walk_score
                .filter(|w| w.is_finite() && (0.0..=100.0).contains(w))
                .map(|ws| Factor::new(ws, "EPA National Walkability Index", Grade::Area)),
            hazards: None,
            environment: None,
            investment: None,
            fengshui: None,
            gates: GateInputs {
                sfha,
                flood_zone: has_tag(FeatureTag::InSfhaFlood).then(|| "AE".to_string()),
                very_high_fire,
                freeway_distance_m: None,
                freeway_within_500ft: None,
                t_junction: None,
                house_number: house_number_from_address(&core.address),
            },
            home,
            style,
            features,
        }
    }

    /// Encodes `self` into the typed 52-column [`HomeFactorsRowProto`](crate::db::HomeFactorsRowProto)
    /// and sidecar [`FactorProvenanceProto`](crate::db::FactorProvenanceProto).
    #[allow(clippy::too_many_arguments)]
    pub fn to_proto(
        &self,
        listing_sk: i64,
        external_id: &str,
        source_tier: &str,
        completeness_pct: f64,
        walk_score: Option<f64>,
        school_composite_score: Option<f64>,
        style_family_override: Option<&str>,
        wildfire_zone: Option<&str>,
        zip: Option<&str>,
        lat: Option<f64>,
        lon: Option<f64>,
        scanned_ts: i64,
        updated_ts: i64,
    ) -> (
        crate::db::HomeFactorsRowProto,
        crate::db::FactorProvenanceProto,
    ) {
        crate::db::proto::home_factors_to_proto(
            listing_sk,
            external_id,
            source_tier,
            completeness_pct,
            walk_score,
            school_composite_score,
            style_family_override,
            wildfire_zone,
            zip,
            lat,
            lon,
            scanned_ts,
            updated_ts,
            self,
        )
    }

    /// Reconstructs [`HomeFactors`] directly from [`HomeFactorsRowProto`](crate::db::HomeFactorsRowProto)
    /// and optional [`FactorProvenanceProto`](crate::db::FactorProvenanceProto) with zero JSON parsing.
    pub fn from_proto(
        row: &crate::db::HomeFactorsRowProto,
        prov: Option<&crate::db::FactorProvenanceProto>,
    ) -> Self {
        crate::db::proto::home_factors_from_proto(row, prov)
    }
}

/// Estimates area-baseline monthly ownership `(total_monthly, principal_interest)` for a listing
/// assuming 20% down at 6.5% 30-year fixed plus state effective property tax and homeowners insurance.
pub fn estimate_listing_monthly_ownership(
    price: i64,
    hoa: i64,
    state: Option<&str>,
) -> (f64, f64) {
    let pf = price as f64;
    let loan = pf * 0.80_f64;
    let pi = crate::shared::monthly_pi(loan, 6.5, 30);
    let st = state.unwrap_or("").trim();
    let tax_rate = crate::registry::rates_and_costs::state_effective_property_tax_rate(st);
    let ins_rate = crate::registry::rates_and_costs::state_effective_insurance_pct_of_price(st);
    let tax = pf * tax_rate / 12.0;
    let ins = (pf * ins_rate / 12.0).max(75.0);
    let total = (pi + tax + ins + (std::cmp::Ord::max(hoa, 0) as f64)).round();
    (total, pi.round())
}

fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Utc::now().year()
}

fn report_year(report: &Value) -> i32 {
    report
        .pointer("/meta/generatedAt")
        .and_then(Value::as_str)
        .and_then(year_prefix)
        .unwrap_or_else(current_year)
}

/// Year from an ISO date such as "2026-11-10T19:12:29Z".
fn year_prefix(s: &str) -> Option<i32> {
    s.get(0..4)
        .and_then(|y| y.parse::<i32>().ok())
        .filter(|y| (1800..=2200).contains(y))
}

/// Number from a JSON number or a plain numeric string ("1,990").
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s
            .trim()
            .replace(',', "")
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite()),
        _ => None,
    }
}

fn module<'a>(report: &'a Value, key: &str) -> Option<&'a Value> {
    report
        .get("modules")
        .and_then(|m| m.get(key))
        .and_then(|m| m.get("data"))
        .filter(|d| d.is_object())
}

fn module_num(report: &Value, key: &str, pointer: &str) -> Option<f64> {
    module(report, key).and_then(|d| num(d.pointer(pointer)))
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Strings from a JSON array of strings or a comma/semicolon-separated string (RESO multi-selects).
fn string_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split([',', ';'])
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Every string inside a JSON value (objects and arrays flattened), in document order.
fn collect_strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

/// "2024-25" -> 2025, "2025–26" -> 2026, "2025" -> 2025, 2025 -> 2025.
fn school_year(v: Option<&Value>) -> Option<i32> {
    match v? {
        Value::Number(n) => n.as_i64().map(|y| y as i32),
        Value::String(s) => {
            let groups: Vec<&str> = s
                .split(|c: char| !c.is_ascii_digit())
                .filter(|g| !g.is_empty())
                .collect();
            let first = groups.first()?;
            let y0 = first.parse::<i32>().ok()?;
            match groups.get(1) {
                Some(g) if g.len() == 2 && first.len() == 4 => {
                    Some(y0 / 100 * 100 + g.parse::<i32>().ok()?)
                }
                Some(g) if g.len() == 4 => g.parse::<i32>().ok(),
                _ => Some(y0),
            }
        }
        _ => None,
    }
}

// ----- cost -----------------------------------------------------------------------------------

fn extract_cost(report: &Value, facts: &Value) -> CostFactors {
    let (brand_name, _) = crate::shared::report_brand(report);
    let val = module(report, "valuation");
    let own = val.and_then(|v| v.get("ownership")).filter(|o| o.is_object());
    let rate_year = own.and_then(|o| str_at(o, "/rateDate")).and_then(year_prefix);
    let down = own.and_then(|o| num(o.get("downPaymentPct")));
    let monthly_source = match (down, own.and_then(|o| num(o.get("rate30")))) {
        (Some(d), Some(r)) => format!("{brand_name} cost model ({d}% down, 30-year fixed at {r}%)"),
        _ => format!("{brand_name} cost model"),
    };
    let monthly_by_loan = own.and_then(|o| {
        let conv = num(o.pointer("/scenarios/conventional/total")).filter(|v| *v > 0.0);
        let fha = num(o.pointer("/scenarios/fha/total")).filter(|v| *v > 0.0);
        let va = num(o.pointer("/scenarios/va/total")).filter(|v| *v > 0.0);
        let cash = num(o.pointer("/scenarios/cash/total"))
            .or_else(|| num(o.pointer("/allCash/monthlyTotal")))
            .or_else(|| num(o.pointer("/allCash/total")))
            .filter(|v| *v >= 0.0);
        if conv.is_some() || fha.is_some() || va.is_some() || cash.is_some() {
            Some(LoanMonthlyCosts { conventional: conv, fha, va, cash })
        } else {
            None
        }
    });
    CostFactors {
        monthly_total: own
            .and_then(|o| num(o.pointer("/monthly/total")))
            .filter(|t| *t > 0.0)
            .map(|t| Factor::new(t, monthly_source, Grade::Report).with_year(rate_year)),
        monthly_principal_interest: own.and_then(|o| num(o.pointer("/monthly/principalInterest"))),
        assumed_down_payment_pct: down,
        price: own
            .and_then(|o| num(o.get("price")))
            .filter(|p| *p > 0.0)
            .map(|p| Factor::new(p, format!("{brand_name} cost-model price"), Grade::Report)),
        estimate: val
            .and_then(|v| num(v.pointer("/estimate/value")))
            .filter(|e| *e > 0.0)
            .map(|e| {
                Factor::new(e, format!("{brand_name} value estimate"), Grade::Report)
                    .with_year(val.and_then(|v| str_at(v, "/estimate/asOf")).and_then(year_prefix))
            }),
        list_price: num(facts.pointer("/mlsListing/listPrice"))
            .filter(|p| *p > 0.0)
            .map(|p| Factor::new(p, "MLS list price", Grade::Listing)),
        hoa_monthly: num(facts.get("hoaMonthly"))
            .filter(|h| *h >= 0.0)
            .map(|h| Factor::new(h, "Listing and public records", Grade::Listing))
            .or_else(|| {
                own.and_then(|o| num(o.pointer("/monthly/hoa")))
                    .filter(|h| *h >= 0.0)
                    .map(|h| Factor::new(h, format!("{brand_name} cost model"), Grade::Report))
            }),
        cash_to_close: own.and_then(|o| num(o.pointer("/closingAndSupplemental/cashToClose"))),
        monthly_by_loan,
    }
}

// ----- schools --------------------------------------------------------------------------------

const SCHOOL_GENERIC_WORDS: [&str; 16] = [
    "elementary", "middle", "high", "school", "junior", "senior", "intermediate", "academy", "the",
    "of", "es", "ms", "hs", "jr", "sr", "k",
];

fn name_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn core_tokens(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .filter(|t| !SCHOOL_GENERIC_WORDS.contains(&t.as_str()))
        .cloned()
        .collect()
}

/// Levels served by a grade span such as "K–5", "6–8", "9–12" or "K–8".
fn levels_for_grades(grades: &str) -> Vec<SchoolLevel> {
    let parts: Vec<i32> = grades
        .to_uppercase()
        .split(|c: char| !(c.is_ascii_alphanumeric()))
        .filter(|p| !p.is_empty())
        .filter_map(|p| match p {
            "K" | "TK" | "PK" | "P" | "PREK" | "KG" => Some(0),
            _ => p.parse::<i32>().ok(),
        })
        .collect();
    let (Some(lo), Some(hi)) = (parts.iter().min().copied(), parts.iter().max().copied()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if lo <= 5 {
        out.push(SchoolLevel::Elementary);
    }
    if lo <= 8 && hi >= 6 {
        out.push(SchoolLevel::Middle);
    }
    if hi >= 9 {
        out.push(SchoolLevel::High);
    }
    out
}

fn level_from_name(name: &str) -> Option<SchoolLevel> {
    let n = name.to_lowercase();
    if n.contains("high") {
        Some(SchoolLevel::High)
    } else if n.contains("middle") || n.contains("intermediate") || n.contains("junior") {
        Some(SchoolLevel::Middle)
    } else if n.contains("elementary") || n.contains("primary") {
        Some(SchoolLevel::Elementary)
    } else {
        None
    }
}

fn entry_level(e: &Value) -> Option<SchoolLevel> {
    e.get("level")
        .and_then(Value::as_str)
        .and_then(|l| SchoolLevel::from_key(&l.to_lowercase()))
        .or_else(|| e.get("grades").and_then(Value::as_str).and_then(|g| levels_for_grades(g).first().copied()))
        .or_else(|| e.get("name").and_then(Value::as_str).and_then(level_from_name))
}

fn entry_levels(e: &Value) -> Vec<SchoolLevel> {
    let mut out = e
        .get("grades")
        .and_then(Value::as_str)
        .map(levels_for_grades)
        .unwrap_or_default();
    if let Some(l) = e
        .get("level")
        .and_then(Value::as_str)
        .and_then(|l| SchoolLevel::from_key(&l.to_lowercase()))
    {
        if !out.contains(&l) {
            out.push(l);
        }
    }
    if out.is_empty() {
        if let Some(l) = e.get("name").and_then(Value::as_str).and_then(level_from_name) {
            out.push(l);
        }
    }
    out
}

fn entry_distance(e: &Value) -> f64 {
    num(e.get("distanceMi")).unwrap_or(f64::MAX)
}

/// Finds a civic list entry for an assigned school name: exact name, then the name without generic
/// words ("Elementary", "School", ...), then every remaining word contained in the entry's name.
/// Among several candidates, one at a matching level and then the nearest wins.
fn find_school<'a>(list: &[&'a Value], name: &str, levels: &[SchoolLevel]) -> Option<&'a Value> {
    let target = name_tokens(name);
    let target_core = core_tokens(&target);
    if target.is_empty() {
        return None;
    }
    let level_matches = |e: &Value| -> bool {
        let el = entry_levels(e);
        levels.is_empty() || el.is_empty() || el.iter().any(|l| levels.contains(l))
    };
    let pick = |require_level: bool, pred: &dyn Fn(&[String]) -> bool| -> Option<&'a Value> {
        let mut cands: Vec<&'a Value> = list
            .iter()
            .copied()
            .filter(|e| {
                let toks = name_tokens(e.get("name").and_then(Value::as_str).unwrap_or(""));
                !toks.is_empty() && pred(&toks) && (!require_level || level_matches(e))
            })
            .collect();
        cands.sort_by(|a, b| {
            let la = entry_levels(a).iter().any(|l| levels.contains(l));
            let lb = entry_levels(b).iter().any(|l| levels.contains(l));
            lb.cmp(&la).then_with(|| entry_distance(a).total_cmp(&entry_distance(b)))
        });
        cands.first().copied()
    };
    pick(false, &|toks| toks == target.as_slice())
        .or_else(|| {
            if target_core.is_empty() {
                None
            } else {
                pick(true, &|toks| core_tokens(toks) == target_core)
            }
        })
        .or_else(|| {
            if target_core.is_empty() {
                None
            } else {
                pick(true, &|toks| target_core.iter().all(|t| toks.contains(t)))
            }
        })
}

const DASHBOARD_TIERS: [&str; 5] = ["elaTier", "mathTier", "sciTier", "cciTier", "gradTier"];

/// Public-data value for one civic list entry: the state percentile when present, otherwise the
/// mean California School Dashboard tier x 20. Never reads `score`, `staffingScore` or ratings.
fn school_value(e: &Value) -> Option<Factor<SchoolScore>> {
    let name = e.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    if let Some(p) = num(e.get("percentile")).filter(|p| (0.0..=100.0).contains(p)) {
        let source = e
            .get("metricSource")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Within-state percentile of students meeting standard (public data)")
            .to_string();
        return Some(
            Factor::new(SchoolScore { name, score: p, metric: SchoolMetric::StatePercentile }, source, Grade::Report)
                .with_year(school_year(e.get("metricYear"))),
        );
    }
    let d = e.get("caDashboard").filter(|d| d.is_object())?;
    let tiers: Vec<f64> = DASHBOARD_TIERS
        .iter()
        .filter_map(|k| num(d.get(*k)))
        .filter(|t| (1.0..=5.0).contains(t))
        .collect();
    if tiers.is_empty() {
        return None;
    }
    let score = tiers.iter().sum::<f64>() / tiers.len() as f64 * 20.0;
    Some(
        Factor::new(
            SchoolScore { name, score, metric: SchoolMetric::CaDashboardTiers },
            "California School Dashboard (California Department of Education)",
            Grade::Report,
        )
        .with_year(school_year(d.get("year"))),
    )
}

fn extract_schools(report: &Value, facts: &Value) -> SchoolFactors {
    let mut out = SchoolFactors::default();
    let public = module(report, "civic").and_then(|c| c.pointer("/schools/public"));
    let list: Vec<&Value> = public
        .map(|p| p.get("list").unwrap_or(p))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|e| e.is_object()).collect())
        .unwrap_or_default();
    if list.is_empty() {
        return out;
    }
    // 1. Assigned schools by name (names only; their ratings are never read).
    if let Some(assigned) = facts.pointer("/schools/assigned").and_then(Value::as_array) {
        for a in assigned {
            let Some(name) = a.get("name").and_then(Value::as_str) else {
                continue;
            };
            let mut levels = a
                .get("grades")
                .and_then(Value::as_str)
                .map(levels_for_grades)
                .unwrap_or_default();
            if levels.is_empty() {
                levels.extend(level_from_name(name));
            }
            let Some(entry) = find_school(&list, name, &levels) else {
                continue;
            };
            if levels.is_empty() {
                levels.extend(entry_level(entry));
            }
            let Some(value) = school_value(entry) else {
                continue;
            };
            for l in levels {
                if out.get(l).is_none() {
                    out.set(l, value.clone());
                }
            }
        }
    }
    // 2. Remaining levels: the nearest school whose attendance zone contains the home.
    for level in SchoolLevel::ALL {
        if out.get(level).is_some() {
            continue;
        }
        let mut zoned: Vec<&Value> = list
            .iter()
            .copied()
            .filter(|e| e.get("inAttendanceZone").and_then(Value::as_bool) == Some(true))
            .filter(|e| entry_level(e) == Some(level))
            .collect();
        zoned.sort_by(|a, b| entry_distance(a).total_cmp(&entry_distance(b)));
        if let Some(v) = zoned.into_iter().find_map(school_value) {
            out.set(level, v);
        }
    }
    out
}

// ----- crime, convenience ---------------------------------------------------------------------

fn extract_crime(report: &Value) -> Option<Factor<f64>> {
    let safety = report.pointer("/decisionFactors/safety")?;
    let score = num(safety.get("agencySafetyScore"))?;
    let source = match safety.get("agencyName").and_then(Value::as_str) {
        Some(a) if !a.trim().is_empty() => format!("FBI Crime Data Explorer, agency rates ({a})"),
        _ => "FBI Crime Data Explorer, agency rates".to_string(),
    };
    Some(
        Factor::new(score, source, Grade::Area)
            .with_year(safety.get("year").and_then(Value::as_i64).map(|y| y as i32)),
    )
}

fn extract_convenience(report: &Value) -> Option<Factor<f64>> {
    let access = module(report, "amenities")
        .and_then(|a| num(a.get("accessScore")).or_else(|| num(a.get("amenityScore"))));
    let walk = module(report, "civic").and_then(|c| {
        num(c.pointer("/scores/walkability"))
            .or_else(|| num(c.pointer("/walkability/index")).map(|i| i / 20.0 * 100.0))
    });
    let (value, source) = match (access, walk) {
        (Some(a), Some(w)) => (
            0.6 * a + 0.4 * w,
            "OpenStreetMap amenity access (60%) and EPA National Walkability Index (40%)",
        ),
        (Some(a), None) => (a, "OpenStreetMap amenity access"),
        (None, Some(w)) => (w, "EPA National Walkability Index"),
        (None, None) => return None,
    };
    Some(Factor::new(value, source, Grade::Report))
}

// ----- home fit -------------------------------------------------------------------------------

static EFFECTIVE_YEAR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\beffective\s+(?:year|yr\.?)(?:\s+built)?(?:\s+(?:of|is))?\s*[:=]?\s*((?:18|19|20)\d{2})\b")
        .expect("valid regex")
});

static ADU_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\bj?adus?\b|\baccessory dwelling\b|\bin-?law (?:unit|suite|apartment|quarters)\b|\bsecondary (?:dwelling )?unit\b|\bsecond unit\b|\bgranny (?:unit|flat)\b|\bguest ?house\b|\bcasita\b|\bunidad de vivienda accesoria\b|附属住宅单元",
    )
    .expect("valid regex")
});

/// Words that make an ADU mention hypothetical or negative ("ADU potential", "room for an ADU").
static ADU_NOT_EXISTING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)potential|possib|eligib|feasib|could|room for|space for|plans? for|proposed|buildable|allow|zoned for|opportunit|\bno adu\b|without|not permitted",
    )
    .expect("valid regex")
});

static SQFT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\d{1,3}(?:,\d{3})+|\d+)\s*(?:sq\.?\s*ft\.?|sqft|sf\b|square\s+feet)").expect("valid regex")
});

static BED_BATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(?:bd|br|beds?|bedrooms?)\s*/\s*(\d+(?:\.\d+)?)\s*(?:ba|baths?|bathrooms?)\b")
        .expect("valid regex")
});

fn effective_year(facts: &Value, year_built: Option<i32>, as_of_year: i32) -> Option<Factor<i32>> {
    let valid = |y: i32| y >= 1800 && y <= as_of_year + 1 && year_built.map_or(true, |b| y >= b);
    for p in ["/effectiveYearBuilt", "/effectiveYear", "/buildingRegister/effectiveYearBuilt", "/officialAssessment/effectiveYearBuilt"] {
        if let Some(y) = num(facts.pointer(p)).map(|y| y as i32).filter(|y| valid(*y)) {
            return Some(Factor::new(y, "County assessor effective year", Grade::Listing));
        }
    }
    let mut texts: Vec<&str> = Vec::new();
    for key in ["renovations", "storiesNote", "sqftNote", "propertyType"] {
        if let Some(s) = facts.get(key).and_then(Value::as_str) {
            texts.push(s);
        }
    }
    for key in ["features", "details", "buildingRegister"] {
        if let Some(v) = facts.get(key) {
            collect_strings(v, &mut texts);
        }
    }
    texts.iter().find_map(|t| {
        EFFECTIVE_YEAR_RE
            .captures(t)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .filter(|y| valid(*y))
            .map(|y| Factor::new(y, "Assessor effective year quoted in the listing facts", Grade::Listing))
    })
}

/// Floor of `s.len()`-bounded window that respects UTF-8 boundaries.
fn char_floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// True when the text mentions an existing ADU (not a potential one).
fn mentions_existing_adu(text: &str) -> bool {
    ADU_RE.find_iter(text).any(|m| {
        let start = char_floor(text, m.start().saturating_sub(40));
        let end = char_floor(text, (m.end() + 25).min(text.len()));
        !ADU_NOT_EXISTING_RE.is_match(&text[start..end])
    })
}

/// Existing ADU from `facts.features` (short items: size, beds and baths are read from the same item)
/// or, for presence only, from the property type and renovation notes.
fn extract_adu(facts: &Value) -> Option<Factor<Adu>> {
    let items = string_list(facts.get("features"));
    for item in &items {
        if mentions_existing_adu(item) {
            let sqft = SQFT_RE
                .captures(item)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().replace(',', "").parse::<f64>().ok())
                .filter(|s| *s >= 100.0 && *s <= 3000.0);
            let (beds, baths) = BED_BATH_RE
                .captures(item)
                .map(|c| {
                    (
                        c.get(1).and_then(|m| m.as_str().parse::<f64>().ok()),
                        c.get(2).and_then(|m| m.as_str().parse::<f64>().ok()),
                    )
                })
                .unwrap_or((None, None));
            return Some(Factor::new(Adu { sqft, beds, baths }, "Listing features", Grade::Listing));
        }
    }
    for key in ["propertyType", "renovations"] {
        if let Some(t) = facts.get(key).and_then(Value::as_str) {
            if mentions_existing_adu(t) {
                return Some(Factor::new(Adu::default(), "Listing facts", Grade::Listing));
            }
        }
    }
    None
}

fn stories_from_levels(levels: &[String]) -> Option<f64> {
    let mut best: Option<f64> = None;
    for l in levels {
        let k: String = l.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
        let v = match k.as_str() {
            "one" | "1" | "single" | "onestory" | "singlestory" => Some(1.0),
            "two" | "2" | "twostory" => Some(2.0),
            "threeormore" | "three" | "3" | "threestory" => Some(3.0),
            "multisplit" | "split" | "multi" => Some(2.0),
            _ => None,
        };
        if let Some(v) = v {
            best = Some(best.map_or(v, |b: f64| b.max(v)));
        }
    }
    best
}

fn extract_home(report: &Value, facts: &Value, as_of_year: i32) -> HomeFitFactors {
    const FACTS_SOURCE: &str = "Report facts (MLS, county records, listing research)";
    let br = facts.get("buildingRegister").filter(|b| b.is_object()).unwrap_or(&NULL);
    let positive = |v: Option<&Value>| num(v).filter(|x| *x > 0.0).map(|x| Factor::new(x, FACTS_SOURCE, Grade::Listing));
    let non_negative = |v: Option<&Value>| num(v).filter(|x| *x >= 0.0).map(|x| Factor::new(x, FACTS_SOURCE, Grade::Listing));
    let year_built = num(facts.get("yearBuilt"))
        .or_else(|| num(br.get("yearBuilt")))
        .map(|y| y as i32)
        .filter(|y| (1600..=as_of_year + 2).contains(y))
        .map(|y| Factor::new(y, FACTS_SOURCE, Grade::Listing));
    let stories = num(facts.get("stories"))
        .or_else(|| num(br.get("stories")))
        .or_else(|| stories_from_levels(&string_list(facts.get("levels"))))
        .filter(|s| *s > 0.0)
        .map(|s| Factor::new(s, FACTS_SOURCE, Grade::Listing));
    let effective_year = effective_year(facts, year_built.as_ref().map(|f| f.value), as_of_year);
    HomeFitFactors {
        property_type: facts
            .get("propertyType")
            .and_then(Value::as_str)
            .and_then(PropertyKind::classify)
            .map(|k| Factor::new(k, FACTS_SOURCE, Grade::Listing)),
        beds: non_negative(facts.get("beds")),
        baths: non_negative(facts.get("baths")),
        sqft: positive(facts.get("sqft")).or_else(|| positive(br.get("livingSqft"))),
        lot_sqft: positive(facts.get("lotSqft")).or_else(|| positive(br.get("lotSqft"))),
        stories,
        year_built,
        effective_year,
        adu: extract_adu(facts),
        adu_feasibility: str_at(report, "/decisionFactors/lotAndAdu/feasibility").map(|s| {
            let (brand_name, _) = crate::shared::report_brand(report);
            Factor::new(
                s.to_string(),
                format!("{brand_name} lot and ADU screen (open lot area, state ADU law)"),
                Grade::Report,
            )
        }),
        main_level_bedrooms: num(facts.get("mainLevelBedrooms"))
            .or_else(|| num(facts.get("groundFloorBedrooms")))
            .or_else(|| num(facts.pointer("/mlsListing/mainLevelBedrooms"))),
        main_level_bathrooms: num(facts.get("mainLevelBathrooms"))
            .or_else(|| num(facts.get("groundFloorBathrooms")))
            .or_else(|| num(facts.pointer("/mlsListing/mainLevelBathrooms"))),
        parking_spaces: num(facts.get("parkingSpaces"))
            .or_else(|| num(facts.get("garageSpaces")))
            .or_else(|| num(facts.pointer("/mlsListing/parkingTotal")))
            .or_else(|| num(facts.pointer("/mlsListing/garageSpaces")))
            .or_else(|| num(br.get("parkingSpaces")))
            .filter(|p| *p >= 0.0)
            .map(|p| Factor::new(p, FACTS_SOURCE, Grade::Listing)),
    }
}

// ----- style ----------------------------------------------------------------------------------

/// Style keywords in English, Spanish and Chinese, with words that must not directly precede or
/// follow a match (e.g. "farmhouse sink", "Spanish Colonial" read as Colonial, street names).
struct StyleKeyword {
    re: Regex,
    family: StyleFamily,
    not_before: &'static [&'static str],
    not_after: &'static [&'static str],
}

static STREET_SUFFIX_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^[\s,]*(?:st|street|ave|avenue|rd|road|dr|drive|ct|court|ln|lane|way|pl|place|blvd|boulevard|cir|circle|ter|terrace|pkwy|parkway|hwy|highway|gardens|park|estates|village|school|elementary|middle|high)\b")
        .expect("valid regex")
});

static STYLE_KEYWORDS: Lazy<Vec<StyleKeyword>> = Lazy::new(|| {
    use StyleFamily::*;
    let k = |pat: &str, family, not_before: &'static [&'static str], not_after: &'static [&'static str]| StyleKeyword {
        re: Regex::new(&format!("(?i){pat}")).expect("valid regex"),
        family,
        not_before,
        not_after,
    };
    vec![
        k(r"\bmid[- ]?century\b|\beichler\b|mediados de siglo|中世纪现代", MidCentury, &[], &[]),
        k(r"\bcraftsman\b|\bbungal[oó]w?\b|\bprairie[- ]style\b|\barts (?:and|&) crafts\b|工匠风格|工匠式", Craftsman, &[], &[]),
        k(r"\bsplit[- ]level\b|\btri[- ]level\b|\bbi[- ]level\b|\braised ranch\b|错层", SplitLevel, &[], &[]),
        k(r"\branch[- ]style\b|\branch (?:home|house)\b|\brambler\b|estilo rancho|牧场风格|牧场式", Ranch, &["raised "], &[]),
        k(r"\bfarmhouse\b|农舍风格|农庄风格", Farmhouse, &[], &[" sink", "-style sink", " style sink", " table", " kitchen sink", " apron"]),
        k(r"\bbarndominium\b|\bbarn[- ]style\b|\bgambrel\b|estilo granero|谷仓风格", Barn, &[], &[]),
        k(r"\bcape cod\b|科德角|鳕鱼角", CapeCod, &[], &[]),
        k(r"\bvictorian[oa]?\b|\bqueen anne\b|\bitalianate\b|\bgothic revival\b|维多利亚", Victorian, &[], &[]),
        k(r"\btudor\b|\bfrench (?:provincial|country|normandy)\b|都铎", Tudor, &[], &[]),
        k(
            r"\bmediterranean\b|\bspanish[- ](?:style|colonial|revival|mission)\b|\bmission[- ]style\b|\bmonterey colonial\b|\btuscan[- ]style\b|mediterr[aá]ne[oa]|estilo español|colonial español|西班牙风格|西班牙式|地中海风格|地中海式",
            Mediterranean,
            &[],
            &[],
        ),
        k(r"\bpueblo[- ](?:style|revival)\b|\badobe (?:home|house)\b|\bsouthwest(?:ern)?[- ]style\b|\bsanta fe[- ]style\b|美国西南风格", Southwestern, &[], &[]),
        k(r"\blog (?:cabin|home)\b|\bcabin\b|\ba[- ]frame\b|\bchalet\b|cabaña|小木屋", Cabin, &[], &[]),
        k(r"\bart deco\b|\bgeodesic\b|\bdome home\b|\bearth[- ]?(?:berm|sheltered)\b|装饰艺术风格", OtherDistinctive, &[], &[]),
        k(r"\btraditional[- ]style\b|\btraditional (?:home|house)\b|\bshotgun house\b|\bcreole cottage\b|estilo tradicional|传统风格", Traditional, &[], &[]),
        k(
            r"\bcolonial\b|\bgeorgian\b|\bfederal[- ]style\b|\bgreek revival\b|\bneoclassical\b|\bsaltbox\b|殖民地风格|殖民式",
            Colonial,
            &["spanish ", "spanish-", "monterey "],
            &[" español", " espanol"],
        ),
        k(
            r"\b(?:ultra[- ])?modern (?:home|house|design|architecture|residence|masterpiece)\b|\bcontemporary (?:home|house|architecture|residence|style)\b|\bmodern[- ]style\b|estilo moderno|estilo contempor[aá]neo|现代风格|现代简约|当代风格",
            Modern,
            &["century ", "century-", "mid "],
            &[" office", " automation", " system", " technology", " amenities", " appliances", " kitchen"],
        ),
    ]
});

/// Style families named in free text, with the matched keyword.
fn text_style_matches(texts: &[&str]) -> Vec<(String, StyleFamily)> {
    let mut out: Vec<(String, StyleFamily)> = Vec::new();
    for text in texts {
        for kw in STYLE_KEYWORDS.iter() {
            if out.iter().any(|(_, f)| *f == kw.family) {
                continue;
            }
            for m in kw.re.find_iter(text) {
                let before = text[..m.start()].to_lowercase();
                let after = &text[m.end()..];
                let after_lower = after.to_lowercase();
                if kw.not_before.iter().any(|w| before.ends_with(w))
                    || kw.not_after.iter().any(|w| after_lower.starts_with(w))
                    || STREET_SUFFIX_RE.is_match(after)
                {
                    continue;
                }
                out.push((m.as_str().to_string(), kw.family));
                break;
            }
        }
    }
    out
}

/// Low-trust era and form prior (plan 5.5: "1975, two-story, tract" -> likely ...).
fn era_prior(year_built: Option<i32>, stories: Option<f64>) -> Option<(String, StyleFamily)> {
    let y = year_built?;
    let single = stories.map(|s| s < 1.5);
    let family = match y {
        ..=1899 => StyleFamily::Victorian,
        1900..=1929 => StyleFamily::Craftsman,
        1930..=1944 => StyleFamily::Traditional,
        1945..=1979 => {
            if single == Some(true) {
                StyleFamily::Ranch
            } else {
                StyleFamily::Traditional
            }
        }
        1980..=2009 => StyleFamily::Traditional,
        _ => StyleFamily::Modern,
    };
    let form = match single {
        Some(true) => ", single-story",
        Some(false) => ", multi-story",
        None => "",
    };
    Some((format!("built {y}{form}"), family))
}

fn extract_style(facts: &Value, year_built: Option<i32>, stories: Option<f64>) -> StyleEvidence {
    let mut clues: Vec<StyleClue> = Vec::new();
    for raw in string_list(
        facts
            .get("architecturalStyle")
            .or_else(|| facts.pointer("/mlsListing/architecturalStyle")),
    ) {
        let families = style_families_for_value(&raw);
        clues.push(StyleClue { source: StyleSource::Mls, raw, families });
    }
    for raw in string_list(
        facts
            .get("architectureType")
            .or_else(|| facts.pointer("/buildingRegister/architectureType")),
    ) {
        let families = style_families_for_value(&raw);
        clues.push(StyleClue { source: StyleSource::CountyRecord, raw, families });
    }
    let mut texts: Vec<&str> = Vec::new();
    for p in ["/propertyType", "/renovations", "/mlsListing/remarks", "/remarks", "/publicRemarks", "/description"] {
        if let Some(s) = facts.pointer(p).and_then(Value::as_str) {
            texts.push(s);
        }
    }
    if let Some(f) = facts.get("features") {
        collect_strings(f, &mut texts);
    }
    for (raw, family) in text_style_matches(&texts) {
        clues.push(StyleClue { source: StyleSource::ListingText, raw, families: vec![family] });
    }

    for source in [StyleSource::Mls, StyleSource::CountyRecord, StyleSource::ListingText] {
        let mut families: Vec<StyleFamily> = Vec::new();
        for c in clues.iter().filter(|c| c.source == source) {
            for f in &c.families {
                if !families.contains(f) {
                    families.push(*f);
                }
            }
        }
        if !families.is_empty() {
            let share = source.trust() / families.len() as f64;
            return StyleEvidence {
                probabilities: families.into_iter().map(|f| (f, share)).collect(),
                basis: Some(source),
                clues,
            };
        }
    }
    if let Some((raw, family)) = era_prior(year_built, stories) {
        clues.push(StyleClue { source: StyleSource::EraPrior, raw, families: vec![family] });
        return StyleEvidence {
            probabilities: BTreeMap::from([(family, StyleSource::EraPrior.trust())]),
            basis: Some(StyleSource::EraPrior),
            clues,
        };
    }
    StyleEvidence { probabilities: BTreeMap::new(), basis: None, clues }
}

// ----- wished features ------------------------------------------------------------------------

static FEATURE_PATTERNS: Lazy<Vec<(Feature, Regex)>> = Lazy::new(|| {
    let f = |feature, pat: &str| (feature, Regex::new(&format!("(?i){pat}")).expect("valid regex"));
    vec![
        f(Feature::OpenPlan, r"\bopen[- ](?:floor[- ]?)?plan\b|\bopen[- ]?floorplan\b|\bopen[- ]concept\b|\bgreat room\b|concepto abierto|planta abierta|开放式"),
        f(Feature::HighCeilings, r"\bhigh ceilings?\b|\bvaulted\b|\bcathedral ceilings?\b|\bbeamed ceilings?\b|\bsoaring ceilings?\b|\bvolume ceilings?\b|techos? altos?|techos? abovedados?|挑高|高天花板"),
        f(Feature::NaturalLight, r"\bskylights?\b|\bnatural light\b|\blight[- ]filled\b|\bsun[- ]drenched\b|\bsunroom\b|\bsolarium\b|\bclerestory\b|\babundant light\b|luz natural|tragaluz|采光|天窗"),
        f(Feature::Hardwood, r"\bhardwood\b|madera dura|pisos? de madera|实木地板"),
        f(Feature::Fireplace, r"\bfireplaces?\b|\bwood[- ]burning stove\b|chimenea|壁炉"),
        f(Feature::Pool, r"\bswimming pool\b|\bin-?ground pool\b|\bheated pool\b|\bprivate pool\b|\bpool(?: and| &| with)? spa\b|\bpool\b|piscina|alberca|泳池"),
        f(Feature::View, r"\bviews\b|\bview of\b|\b(?:panoramic|sweeping|ocean|bay|city(?:[- ]lights?)?|hills?|canyon|valley|lake|water|golf(?: course)?|territorial)[- ]views?\b|\bmountain views\b|vista (?:al|a la|a las|panor[aá]mica)|景观|海景|山景|湖景"),
        f(
            Feature::MainLevelPrimary,
            r"\b(?:primary|master|owner'?s?)(?: bedroom| suite| bedroom suite)? (?:is )?(?:on|at) (?:the )?(?:main|ground|first|entry)[- ](?:floor|level)\b|\b(?:main|ground|first)[- ](?:floor|level) (?:primary|master|owner'?s?)\b|\b(?:primary|master)(?: bedroom| suite)? (?:on main|downstairs)\b|\bdownstairs (?:primary|master)\b|rec[aá]mara principal en (?:la )?planta baja|dormitorio principal en (?:la )?planta baja|一楼主卧|主卧在一楼",
        ),
        f(Feature::Office, r"\bhome office\b|\boffice\b|\bden\b|\bstudy\b|\bwork[- ]from[- ]home\b|oficina|estudio|书房|办公室"),
        f(Feature::WorkshopGarage, r"\bworkshop\b|\bwork shop\b|\bworkbench\b|\bshop space\b|\btaller de\b|\bcon taller\b|工作间"),
        f(Feature::EvReady, r"\bev[- ]?charg(?:er|ing)\b|\belectric vehicle\b|\btesla (?:charger|wall connector)\b|\blevel[- ]2 charg|\bnema 14-50\b|cargador (?:para )?(?:veh[ií]culos? el[eé]ctricos?|ev)|充电桩|电动车充电"),
    ]
});

/// Values like "None" in RESO multi-selects mean the feature is absent.
fn meaningful(list: &[String]) -> bool {
    list.iter().any(|s| {
        let k = s.trim().to_lowercase();
        !k.is_empty() && k != "none" && k != "no" && k != "n/a"
    })
}

fn extract_features(facts: &Value, stories: Option<f64>) -> Option<Factor<BTreeSet<Feature>>> {
    let mut texts: Vec<&str> = Vec::new();
    for key in ["features", "details", "interiorFeatures", "exteriorFeatures", "parkingFeatures", "renovations", "garage", "parking", "storiesNote"] {
        if let Some(v) = facts.get(key) {
            collect_strings(v, &mut texts);
        }
    }
    for p in ["/mlsListing/remarks", "/remarks", "/publicRemarks", "/description"] {
        if let Some(s) = facts.pointer(p).and_then(Value::as_str) {
            texts.push(s);
        }
    }
    let pool = string_list(facts.get("poolFeatures"));
    let view = string_list(facts.get("view"));
    let has_mls_lists = !pool.is_empty() || !view.is_empty();
    if texts.is_empty() && !has_mls_lists {
        return None;
    }
    let mut found = BTreeSet::new();
    for (feature, re) in FEATURE_PATTERNS.iter() {
        let hit = texts.iter().any(|t| {
            re.find_iter(t).any(|m| {
                // "pool table" is furniture, not a pool.
                !(*feature == Feature::Pool && t[m.end()..].to_lowercase().starts_with(" table"))
            })
        });
        if hit {
            found.insert(*feature);
        }
    }
    if meaningful(&pool) {
        found.insert(Feature::Pool);
    }
    if meaningful(&view) {
        found.insert(Feature::View);
    }
    if stories.map_or(false, |s| s < 1.5) {
        // Single-level home: the primary suite is on the main level.
        found.insert(Feature::MainLevelPrimary);
    }
    Some(Factor::new(found, "MLS feature fields and listing text", Grade::Listing))
}

// ----- gates ----------------------------------------------------------------------------------

fn fire_flag(hz: Option<&Value>) -> Option<Factor<bool>> {
    let hz = hz?;
    if let Some(ca) = hz.pointer("/wildfire/california").filter(|c| c.is_object()) {
        let class = num(ca.get("fhszClass"));
        let label = ca.get("fhsz").and_then(Value::as_str).unwrap_or_default().to_lowercase();
        if class.is_some() || !label.is_empty() {
            let very_high = class == Some(3.0)
                || (label.contains("very high") && !label.starts_with("not"));
            return Some(Factor::new(
                very_high,
                "CAL FIRE Fire Hazard Severity Zones",
                Grade::Report,
            ));
        }
    }
    let whp = num(hz.pointer("/wildfire/whp/class"))?;
    Some(Factor::new(whp == 5.0, "USFS Wildfire Hazard Potential", Grade::Report))
}

/// T-junction: the structured `tJunction` block when present, else the form entries
/// (`lu-chong`, "points straight at the house").
fn t_junction_flag(fs: Option<&Value>, brand_name: &str) -> Option<Factor<bool>> {
    let fs = fs?;
    let source = format!("{brand_name} feng shui form reading (OpenStreetMap streets)");
    if let Some(tj) = fs.get("tJunction").filter(|t| t.is_object()) {
        if let Some(hits) = tj.get("hits").and_then(Value::as_array) {
            return Some(Factor::new(!hits.is_empty(), source, Grade::Report));
        }
        if let Some(b) = tj.get("hasAlignment").and_then(Value::as_bool) {
            return Some(Factor::new(b, source, Grade::Report));
        }
    }
    if let Some(b) = fs.pointer("/quickScreens/tJunction/hit").and_then(Value::as_bool) {
        return Some(Factor::new(b, source, Grade::Report));
    }
    let form = fs.get("form").and_then(Value::as_array)?;
    let hit = form.iter().any(|x| {
        x.get("key").and_then(Value::as_str) == Some("lu-chong")
            || x.get("en")
                .and_then(Value::as_str)
                .map_or(false, |s| s.contains("points straight at the house"))
    });
    Some(Factor::new(hit, source, Grade::Report))
}

fn house_number_from_address(address: &str) -> Option<String> {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*(\d+)").expect("valid regex"));
    RE.captures(address)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn house_number(report: &Value) -> Option<String> {
    ["/geo/displayAddress", "/geo/matchedAddress", "/meta/displayAddress", "/geo/input", "/meta/input"]
        .iter()
        .filter_map(|p| report.pointer(p).and_then(Value::as_str))
        .find_map(house_number_from_address)
}

fn extract_adu_from_listing_fields(
    features_text: Option<&str>,
    property_type: Option<&str>,
    has_adu_flag: bool,
) -> Option<Factor<Adu>> {
    if let Some(ft) = features_text {
        for raw_item in ft.split([',', ';']) {
            let item = raw_item.trim();
            if item.is_empty() {
                continue;
            }
            if mentions_existing_adu(item) {
                let sqft = SQFT_RE
                    .captures(item)
                    .and_then(|c| c.get(1))
                    .and_then(|m| m.as_str().replace(',', "").parse::<f64>().ok())
                    .filter(|s| (100.0..=3000.0).contains(s));
                let (beds, baths) = BED_BATH_RE
                    .captures(item)
                    .map(|c| {
                        (
                            c.get(1).and_then(|m| m.as_str().parse::<f64>().ok()),
                            c.get(2).and_then(|m| m.as_str().parse::<f64>().ok()),
                        )
                    })
                    .unwrap_or((None, None));
                return Some(Factor::new(
                    Adu { sqft, beds, baths },
                    "Listing features",
                    Grade::Listing,
                ));
            }
        }
    }
    if let Some(pt) = property_type {
        if mentions_existing_adu(pt) {
            return Some(Factor::new(Adu::default(), "Listing facts", Grade::Listing));
        }
    }
    if has_adu_flag {
        return Some(Factor::new(
            Adu::default(),
            "Listing features",
            Grade::Listing,
        ));
    }
    None
}

fn extract_style_from_listing_fields(
    architectural_style: Option<&str>,
    property_type: Option<&str>,
    features_text: Option<&str>,
    year_built: Option<i32>,
    stories: Option<f64>,
) -> StyleEvidence {
    let mut clues: Vec<StyleClue> = Vec::new();
    if let Some(style_str) = architectural_style {
        for raw_part in style_str.split([',', ';']) {
            let raw = raw_part.trim();
            if raw.is_empty() {
                continue;
            }
            let families = style_families_for_value(raw);
            clues.push(StyleClue {
                source: StyleSource::Mls,
                raw: raw.to_string(),
                families,
            });
        }
    }

    let mut texts: Vec<&str> = Vec::with_capacity(2);
    if let Some(pt) = property_type.map(str::trim).filter(|s| !s.is_empty()) {
        texts.push(pt);
    }
    if let Some(ft) = features_text.map(str::trim).filter(|s| !s.is_empty()) {
        texts.push(ft);
    }
    for (raw, family) in text_style_matches(&texts) {
        clues.push(StyleClue {
            source: StyleSource::ListingText,
            raw,
            families: vec![family],
        });
    }

    for source in [
        StyleSource::Mls,
        StyleSource::CountyRecord,
        StyleSource::ListingText,
    ] {
        let mut families: Vec<StyleFamily> = Vec::new();
        for c in clues.iter().filter(|c| c.source == source) {
            for f in &c.families {
                if !families.contains(f) {
                    families.push(*f);
                }
            }
        }
        if !families.is_empty() {
            let share = source.trust() / families.len() as f64;
            return StyleEvidence {
                probabilities: families.into_iter().map(|f| (f, share)).collect(),
                basis: Some(source),
                clues,
            };
        }
    }

    if let Some((raw, family)) = era_prior(year_built, stories) {
        clues.push(StyleClue {
            source: StyleSource::EraPrior,
            raw,
            families: vec![family],
        });
        return StyleEvidence {
            probabilities: BTreeMap::from([(family, StyleSource::EraPrior.trust())]),
            basis: Some(StyleSource::EraPrior),
            clues,
        };
    }

    StyleEvidence {
        probabilities: BTreeMap::new(),
        basis: None,
        clues,
    }
}

fn extract_features_from_listing_fields(
    features_text: Option<&str>,
    has_pool: bool,
    has_ev_charger: bool,
    has_hardwood_floors: bool,
    tags_json: Option<&str>,
    stories: Option<f64>,
) -> Option<Factor<BTreeSet<Feature>>> {
    let ft_clean = features_text.map(str::trim).filter(|s| !s.is_empty());
    let mut tag_features = BTreeSet::new();
    let mut had_tags = false;
    if let Some(tj) = tags_json.map(str::trim).filter(|s| !s.is_empty() && *s != "[]") {
        for raw_tok in tj.split([',', '"', '[', ']']) {
            let tok = raw_tok.trim();
            if tok.is_empty() {
                continue;
            }
            had_tags = true;
            if let Some(ftag) = crate::db::proto::feature_tag_from_slug(tok) {
                use crate::db::proto::FeatureTag;
                match ftag {
                    FeatureTag::OpenPlan => {
                        tag_features.insert(Feature::OpenPlan);
                    }
                    FeatureTag::HighCeilings => {
                        tag_features.insert(Feature::HighCeilings);
                    }
                    FeatureTag::NaturalLight | FeatureTag::Sunroom => {
                        tag_features.insert(Feature::NaturalLight);
                    }
                    FeatureTag::HardwoodFloors => {
                        tag_features.insert(Feature::Hardwood);
                    }
                    FeatureTag::Fireplace => {
                        tag_features.insert(Feature::Fireplace);
                    }
                    FeatureTag::Pool => {
                        tag_features.insert(Feature::Pool);
                    }
                    FeatureTag::View => {
                        tag_features.insert(Feature::View);
                    }
                    FeatureTag::SingleStory => {
                        tag_features.insert(Feature::MainLevelPrimary);
                    }
                    FeatureTag::HomeOffice => {
                        tag_features.insert(Feature::Office);
                    }
                    FeatureTag::WorkshopGarage => {
                        tag_features.insert(Feature::WorkshopGarage);
                    }
                    FeatureTag::EvCharger => {
                        tag_features.insert(Feature::EvReady);
                    }
                    _ => {}
                }
            }
        }
    }

    let has_flag = has_pool || has_ev_charger || has_hardwood_floors || had_tags;
    if ft_clean.is_none() && !has_flag {
        return None;
    }

    let mut found = tag_features;
    if let Some(t) = ft_clean {
        for (feature, re) in FEATURE_PATTERNS.iter() {
            let hit = re.find_iter(t).any(|m| {
                !(*feature == Feature::Pool && t[m.end()..].to_lowercase().starts_with(" table"))
            });
            if hit {
                found.insert(*feature);
            }
        }
    }
    if has_pool {
        found.insert(Feature::Pool);
    }
    if has_ev_charger {
        found.insert(Feature::EvReady);
    }
    if has_hardwood_floors {
        found.insert(Feature::Hardwood);
    }
    if stories.map_or(false, |s| s < 1.5) {
        found.insert(Feature::MainLevelPrimary);
    }
    Some(Factor::new(
        found,
        "MLS feature fields and listing text",
        Grade::Listing,
    ))
}

fn extract_gates(report: &Value, _facts: &Value) -> GateInputs {
    let hz = module(report, "hazards");
    let flood = hz.and_then(|h| h.get("flood")).filter(|f| f.is_object());
    let sfha = flood.and_then(|f| f.get("sfha")).and_then(Value::as_bool).map(|b| {
        Factor::new(b, "FEMA National Flood Hazard Layer", Grade::Report)
            .with_year(flood.and_then(|f| str_at(f, "/firmEffective")).and_then(year_prefix))
    });
    let flood_zone = flood.and_then(|f| str_at(f, "/zone")).map(str::to_string);
    let am = module(report, "amenities");
    let proximity = am.and_then(|a| a.get("proximity")).filter(|p| p.is_object());
    let freeway_distance_m = proximity.and_then(|p| num(p.pointer("/freeway/distM")));
    // A null freeway entry means none was found nearby, but only if the road/line query returned
    // something at all.
    let lines_known = proximity.map_or(false, |p| {
        ["freeway", "expressway", "arterial", "rail", "lightRail", "powerLine", "water"]
            .iter()
            .any(|k| p.get(*k).map_or(false, |v| !v.is_null()))
    });
    let freeway_within_500ft = match freeway_distance_m {
        Some(d) => Some(Factor::new(d < FREEWAY_SCREEN_M, "OpenStreetMap road network", Grade::Report)),
        None if lines_known => Some(Factor::new(false, "OpenStreetMap road network (no freeway nearby)", Grade::Report)),
        None => None,
    };
    let (brand_name, _) = crate::shared::report_brand(report);
    GateInputs {
        sfha,
        flood_zone,
        very_high_fire: fire_flag(hz),
        freeway_distance_m,
        freeway_within_500ft,
        t_junction: t_junction_flag(module(report, "fengshui"), brand_name),
        house_number: house_number(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn grade_spans_map_to_levels() {
        assert_eq!(levels_for_grades("K–5"), vec![SchoolLevel::Elementary]);
        assert_eq!(levels_for_grades("6–8"), vec![SchoolLevel::Middle]);
        assert_eq!(levels_for_grades("9–12"), vec![SchoolLevel::High]);
        assert_eq!(levels_for_grades("K-8"), vec![SchoolLevel::Elementary, SchoolLevel::Middle]);
        assert_eq!(school_year(Some(&json!("2024-25"))), Some(2025));
        assert_eq!(school_year(Some(&json!("2025–26"))), Some(2026));
        assert_eq!(school_year(Some(&json!(2025))), Some(2025));
    }

    #[test]
    fn adu_mentions_distinguish_existing_from_potential() {
        assert!(mentions_existing_adu("Permitted 600 sq ft detached 1BD/1BA ADU (200 Synthparcelbeta Ave #B)"));
        assert!(!mentions_existing_adu("Strong (CA State ADU Eligible)"));
        assert!(!mentions_existing_adu("Large lot with ADU potential"));
        assert!(mentions_existing_adu("Single-family (detached) + Permitted ADU"));
    }

    #[test]
    fn style_keywords_respect_context() {
        let fam = |t: &str| text_style_matches(&[t]).into_iter().map(|(_, f)| f).collect::<Vec<_>>();
        assert_eq!(fam("Remodeled 1925 Craftsman"), vec![StyleFamily::Craftsman]);
        assert_eq!(fam("Kitchen with farmhouse sink"), Vec::<StyleFamily>::new());
        assert_eq!(fam("Charming Spanish Colonial home"), vec![StyleFamily::Mediterranean]);
        assert_eq!(fam("Mid-century modern home"), vec![StyleFamily::MidCentury]);
        assert_eq!(fam("Close to Colonial Dr shops"), Vec::<StyleFamily>::new());
        assert_eq!(fam("craftsmanship throughout"), Vec::<StyleFamily>::new());
        assert_eq!(fam("经典工匠风格住宅"), vec![StyleFamily::Craftsman]);
        assert_eq!(fam("Hermosa casa victoriana"), vec![StyleFamily::Victorian]);
    }

    #[test]
    fn from_listing_row_and_core_roundtrip_proto_without_raw_json() {
        use crate::db::proto::{feature_tag_bit, FeatureTag};
        use crate::db::{
            ListingCoreProto, ListingDescriptionsProto, ListingFeaturesProto, ListingFullView,
            ListingHazardsProto, ListingSchoolsProto,
        };

        let tag_bits = feature_tag_bit(FeatureTag::Pool)
            | feature_tag_bit(FeatureTag::EvCharger)
            | feature_tag_bit(FeatureTag::HardwoodFloors)
            | feature_tag_bit(FeatureTag::AduFeasible);

        let core = ListingCoreProto {
            listing_sk: 4242,
            external_id: "synth-95008-proto".to_string(),
            report_slug: Some("200-synth-ave-campbell-ca-95008".to_string()),
            zip: "95008".to_string(),
            state_code: Some("CA".to_string()),
            city: Some("Campbell".to_string()),
            address: "200 Synth Ave, Campbell, CA 95008".to_string(),
            lat: Some(37.282),
            lon: Some(-121.954),
            status: 1,
            property_category: 1,
            price: Some(1_750_000),
            list_price: Some(1_750_000),
            estimated_value: Some(1_800_000),
            hoa_monthly: Some(0),
            beds: Some(4.0),
            baths: Some(2.5),
            sqft: Some(2_150),
            lot_sqft: Some(7_200),
            year_built: Some(1958),
            stories: Some(1.0),
            school_score: Some(8.6),
            walk_score: Some(72.0),
            safety_score: Some(81.5),
            tag_bits,
            known_bits: tag_bits,
            ..Default::default()
        };

        let view = ListingFullView {
            core: core.clone(),
            schools: Some(ListingSchoolsProto {
                listing_sk: 4242,
                school_score: Some(8.6),
                elem_name: Some("Campbell Synth Elementary".to_string()),
                elem_rating: Some(9.0),
                mid_name: Some("Campbell Synth Middle".to_string()),
                mid_rating: Some(8.4),
                high_name: Some("Campbell Synth High".to_string()),
                high_rating: Some(8.5),
                ..Default::default()
            }),
            utilities: None,
            hazards: Some(ListingHazardsProto {
                listing_sk: 4242,
                flood_zone: Some("X".to_string()),
                wildfire_label: Some("Low".to_string()),
                safety_score: Some(81.5),
                ..Default::default()
            }),
            features: Some(ListingFeaturesProto {
                listing_sk: 4242,
                architectural_style: Some("Craftsman".to_string()),
                property_type_label: Some("Single Family Residential".to_string()),
                garage_spaces: Some(2.0),
                adu_feasible: Some(true),
                has_pool: Some(true),
                has_ev_charger: Some(true),
                has_hardwood_floors: Some(true),
                ..Default::default()
            }),
            engagement: None,
            descriptions: Some(ListingDescriptionsProto {
                listing_sk: 4242,
                features_text: Some(
                    "Permitted 540 sqft 1BD/1BA ADU, vaulted ceilings, home office".to_string(),
                ),
                mls_remarks: None,
            }),
            tags: vec!["pool".to_string(), "ev_charger".to_string()],
        };

        let mut row = view.to_listing_row();
        // Ensure raw_json is ignored even if populated with conflicting legacy data.
        row.raw_json = Some(r#"{"architecturalStyle":"Victorian"}"#.to_string());

        let hf = HomeFactors::from_listing_row(&row);
        assert_eq!(hf.style.top_family(), Some(StyleFamily::Craftsman));
        assert!(hf.cost.monthly_total.as_ref().unwrap().estimated);
        assert_eq!(hf.home.parking_spaces.as_ref().unwrap().value, 2.0);
        assert_eq!(hf.home.adu.as_ref().unwrap().value.sqft, Some(540.0));
        assert_eq!(hf.gates.house_number.as_deref(), Some("200"));
        assert_eq!(hf.gates.sfha.as_ref().unwrap().value, false);
        assert_eq!(hf.crime.as_ref().unwrap().value, 81.5);
        let feats = &hf.features.as_ref().unwrap().value;
        assert!(feats.contains(&Feature::Pool));
        assert!(feats.contains(&Feature::EvReady));
        assert!(feats.contains(&Feature::Hardwood));
        assert!(feats.contains(&Feature::HighCeilings));
        assert!(feats.contains(&Feature::MainLevelPrimary));

        let (row_proto, prov_proto) = hf.to_proto(
            4242,
            &row.id,
            "light_scan",
            88.2,
            row.walk_score,
            row.school_score,
            None,
            Some("Low"),
            Some(&row.zip),
            row.lat,
            row.lon,
            1_700_000_000,
            1_700_000_000,
        );
        let fast_hf = HomeFactors::from_proto(&row_proto, None);
        assert_eq!(fast_hf.style.top_family(), Some(StyleFamily::Craftsman));
        assert_eq!(fast_hf.features.as_ref().unwrap().value, *feats);
        assert_eq!(
            fast_hf.cost.monthly_total.as_ref().unwrap().estimated,
            true
        );

        let full_hf = HomeFactors::from_proto(&row_proto, Some(&prov_proto));
        assert_eq!(full_hf, hf);

        let core_hf = HomeFactors::from_listing_core(&core);
        assert_eq!(core_hf.id, "synth-95008-proto");
        assert!(core_hf.features.as_ref().unwrap().value.contains(&Feature::Pool));
        assert!(core_hf.features.as_ref().unwrap().value.contains(&Feature::EvReady));
    }
}
