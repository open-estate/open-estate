//! The "Your Match" ranking engine (plan v3, section 5.4): deterministic, explainable, and the same for
//! every language.
//!
//! 1. Per-dimension utilities (0–100) from [`utility`], plus commute from the caller's
//!    [`CommuteMatrix`] with the profile's traffic buffer.
//! 2. A missing value takes the median of the candidate set and is flagged as estimated; a weighted
//!    dimension with no data anywhere in the set is dropped and reported.
//! 3. Score = Σ weight × utility ÷ Σ weight over the dimensions in use; coverage = share of that
//!    weight backed by real data.
//! 4. Gates sit outside the sum: an unmet must-have moves a home to the near-miss list, a
//!    deal-breaker flags it and hides it by default; a critical dimension scoring under 40 caps the
//!    total below "Strong match".
//! 5. Order: score descending, then id. Scores less than one point from the leader of their group
//!    are ties and share a rank.

use super::catalog::{Dimension, Lang, MatchBand, PropertyKind, Screen, StyleFamily, StylePreference};
use super::explain::{self, Explanation};
use super::factors::{Factor, HomeFactors};
use super::profile::{
    Profile, DEFAULT_TRAFFIC_BUFFER, DEFAULT_TRIPS_PER_WEEK, MAX_TRAFFIC_BUFFER, MIN_TRAFFIC_BUFFER,
    PROFILE_VERSION,
};
use super::utility::{self, Cell, EstimateReason};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Scores closer than this to the leader of their group are ties.
pub const TIE_MARGIN: f64 = 1.0;
/// A critical dimension with a real utility under this caps the total.
pub const CRITICAL_THRESHOLD: f64 = 40.0;
/// Cap applied by a failing critical dimension: one tie margin below "Strong match", so a capped
/// home never ties with a strong one.
pub const CRITICAL_CAP: f64 = MatchBand::STRONG_MIN - TIE_MARGIN;
/// Probability of avoided style families at which the `style_avoid` screen fires. An era prior
/// alone (0.30) never reaches it.
pub const STYLE_AVOID_MIN_PROBABILITY: f64 = 0.5;

const FEET_PER_METRE: f64 = 3.280_84;

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// Free-flow drive time from a home to one of the profile's places.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommuteTime {
    pub minutes: f64,
    /// A road-distance estimate (router down or outside coverage) rather than a routed time.
    #[serde(default, skip_serializing_if = "is_false")]
    pub estimated: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Free-flow drive minutes per home (by [`HomeFactors::id`]) and per place (in `Profile::places`
/// order). Supplied by the caller (Phase 4 routing); the engine applies the traffic buffer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommuteMatrix {
    pub homes: BTreeMap<String, Vec<Option<CommuteTime>>>,
}

impl CommuteMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a routed free-flow time.
    pub fn set(&mut self, home_id: &str, place_index: usize, minutes: f64) {
        self.put(home_id, place_index, CommuteTime { minutes, estimated: false });
    }

    /// Sets a road-distance estimate.
    pub fn set_estimated(&mut self, home_id: &str, place_index: usize, minutes: f64) {
        self.put(home_id, place_index, CommuteTime { minutes, estimated: true });
    }

    fn put(&mut self, home_id: &str, place_index: usize, t: CommuteTime) {
        let row = self.homes.entry(home_id.to_string()).or_default();
        if row.len() <= place_index {
            row.resize(place_index + 1, None);
        }
        row[place_index] = Some(t);
    }

    pub fn get(&self, home_id: &str, place_index: usize) -> Option<CommuteTime> {
        self.homes.get(home_id)?.get(place_index).copied().flatten()
    }
}

/// Options for [`rank_with`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankOptions {
    /// Keep homes that hit a deal-breaker in the main list (flagged) instead of hiding them.
    #[serde(default)]
    pub include_flagged: bool,
    /// Year that home age is measured against; defaults to the latest `as_of_year` in the set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_year: Option<i32>,
}

// ---------------------------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------------------------

/// Why a weighted dimension was left out of every score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// No home in the set has data for it.
    NoData,
    /// The buyer picked no style and wished for no feature.
    NoStylePreference,
    /// The profile has no places.
    NoPlaces,
    /// The profile has neither a monthly budget nor a maximum price.
    NoBudget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedDimension {
    pub dimension: Dimension,
    pub reason: DropReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Weight actually used for a dimension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeightShare {
    pub dimension: Dimension,
    /// Weight from the profile.
    pub weight: f64,
    /// Normalized share over the dimensions in use, 0–1.
    pub share: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeCode {
    /// None of the weighted dimensions has data; scores are 0.
    NoScorableDimensions,
    /// No home has a drive time to this place; it was left out of the commute score.
    PlaceWithoutDriveTimes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub code: NoticeCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One dimension of one home's score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DimensionScore {
    pub dimension: Dimension,
    /// Utility, 0–100.
    pub utility: f64,
    /// Normalized weight, 0–1.
    pub share: f64,
    /// Contribution to the score: share × utility.
    pub points: f64,
    pub estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimatedDimension {
    pub dimension: Dimension,
    pub reason: EstimateReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    MustHave,
    DealBreaker,
}

/// What a gate hit is about: one of the profile's screens, or a property type the buyer did not
/// select.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    SfhaFlood,
    VeryHighFire,
    #[serde(rename = "freeway_within_500ft")]
    FreewayWithin500ft,
    TJunction,
    HoaOver,
    StyleAvoid,
    AddressDigits,
    PropertyType,
}

impl From<Screen> for GateReason {
    fn from(s: Screen) -> Self {
        match s {
            Screen::SfhaFlood => GateReason::SfhaFlood,
            Screen::VeryHighFire => GateReason::VeryHighFire,
            Screen::FreewayWithin500ft => GateReason::FreewayWithin500ft,
            Screen::TJunction => GateReason::TJunction,
            Screen::HoaOver => GateReason::HoaOver,
            Screen::StyleAvoid => GateReason::StyleAvoid,
            Screen::AddressDigits => GateReason::AddressDigits,
        }
    }
}

/// A failed must-have or deal-breaker, with the values behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateHit {
    pub reason: GateReason,
    pub kind: GateKind,
    /// Freeway distance in feet, HOA per month, or avoided-style probability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The buyer's limit (500 ft, HOA cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// Flood zone, avoided style family key, matched digits or property type key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One home's Your Match result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoredHome {
    pub id: String,
    /// 1-based rank within its list; tied homes share the rank of their group's leader.
    pub rank: usize,
    pub tied: bool,
    /// Match score, 0–100, after any critical cap.
    pub score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncapped_score: Option<f64>,
    pub band: MatchBand,
    /// Share of the weight backed by real data, 0–1.
    pub coverage: f64,
    pub dimensions: Vec<DimensionScore>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub estimated: Vec<EstimatedDimension>,
    /// Critical dimensions that scored under 40.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capped_by: Vec<Dimension>,
    pub flagged: bool,
    pub near_miss: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<GateHit>,
    /// Screens the buyer uses that could not be checked for lack of data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unverified: Vec<Screen>,
    pub reasons: Vec<Explanation>,
    pub trade_offs: Vec<Explanation>,
}

impl ScoredHome {
    pub fn utility(&self, d: Dimension) -> Option<f64> {
        self.dimensions.iter().find(|x| x.dimension == d).map(|x| x.utility)
    }

    pub fn is_estimated(&self, d: Dimension) -> bool {
        self.dimensions.iter().any(|x| x.dimension == d && x.estimated)
    }
}

/// Result of [`rank`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankResult {
    pub profile_version: u32,
    pub reference_year: i32,
    pub home_count: usize,
    /// Dimensions in use and their normalized weights.
    pub weights: Vec<WeightShare>,
    pub dropped: Vec<DroppedDimension>,
    /// Median utility of each dimension in use across the set (after imputation).
    pub medians: BTreeMap<Dimension, f64>,
    /// Homes that pass every gate (plus flagged homes when `include_flagged`), best first.
    pub ranked: Vec<ScoredHome>,
    /// Homes that miss a must-have.
    pub near_misses: Vec<ScoredHome>,
    /// Homes that hit a deal-breaker, hidden from `ranked` by default.
    pub flagged: Vec<ScoredHome>,
    pub notices: Vec<Notice>,
}

impl RankResult {
    /// Every scored home: ranked, then near misses, then flagged.
    pub fn all(&self) -> impl Iterator<Item = &ScoredHome> {
        self.ranked.iter().chain(self.near_misses.iter()).chain(self.flagged.iter())
    }

    pub fn find(&self, id: &str) -> Option<&ScoredHome> {
        self.all().find(|h| h.id == id)
    }

    /// Fills every `text` field in `lang` (see [`explain::localize`]).
    pub fn localize(&mut self, lang: Lang) {
        explain::localize(self, lang);
    }
}

// ---------------------------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------------------------

/// Ranks `homes` for `profile` with default options: deal-breaker homes are hidden in `flagged`.
pub fn rank(profile: &Profile, homes: &[HomeFactors], commute: &CommuteMatrix) -> RankResult {
    rank_with(profile, homes, commute, &RankOptions::default())
}

/// Median of `v` (mean of the two middle values for an even count). `v` must not be empty.
fn median(v: &mut [f64]) -> f64 {
    v.sort_unstable_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn default_reference_year(homes: &[HomeFactors]) -> i32 {
    homes
        .iter()
        .map(|h| h.as_of_year)
        .filter(|y| *y > 0)
        .max()
        .unwrap_or_else(|| {
            use chrono::Datelike;
            chrono::Utc::now().year()
        })
}

/// [`rank`] with explicit options.
pub fn rank_with(
    profile: &Profile,
    homes: &[HomeFactors],
    commute: &CommuteMatrix,
    opts: &RankOptions,
) -> RankResult {
    const N: usize = Dimension::COUNT;
    let reference_year = opts.reference_year.unwrap_or_else(|| default_reference_year(homes));
    let weights = profile.weight_vector();
    let mut notices = Vec::new();

    // Profile-level reasons a weighted dimension cannot be used.
    let mut inactive: [Option<DropReason>; N] = [None; N];
    if profile.style_preferences.is_empty() && profile.feature_wishes.is_empty() {
        inactive[Dimension::Style.index()] = Some(DropReason::NoStylePreference);
    }
    if profile.places.is_empty() {
        inactive[Dimension::Commute.index()] = Some(DropReason::NoPlaces);
    }
    if profile.budget.monthly.is_none() && profile.budget.max_price.is_none() {
        inactive[Dimension::Cost.index()] = Some(DropReason::NoBudget);
    }

    let mut cells: Vec<[Cell; N]> = homes
        .iter()
        .map(|h| utility::home_utilities(profile, h, reference_year))
        .collect();
    if inactive[Dimension::Commute.index()].is_none() && weights[Dimension::Commute.index()] > 0.0 {
        fill_commute(profile, homes, commute, &mut cells, &mut notices);
    }

    // Impute gaps with the set median; drop dimensions with no data anywhere.
    let mut active = [false; N];
    let mut dropped = Vec::new();
    for d in Dimension::ALL {
        let i = d.index();
        if weights[i] <= 0.0 {
            continue;
        }
        if let Some(reason) = inactive[i] {
            dropped.push(DroppedDimension { dimension: d, reason, text: None });
            continue;
        }
        let mut present: Vec<f64> = cells.iter().filter_map(|c| c[i].value).collect();
        if present.is_empty() {
            if !homes.is_empty() {
                dropped.push(DroppedDimension { dimension: d, reason: DropReason::NoData, text: None });
            }
            continue;
        }
        let med = median(&mut present);
        for c in cells.iter_mut() {
            if c[i].value.is_none() {
                c[i] = Cell { value: Some(med), estimated: Some(EstimateReason::ImputedMedian) };
            }
        }
        active[i] = true;
    }

    let total: f64 = (0..N).filter(|i| active[*i]).map(|i| weights[i]).sum();
    let mut share = [0.0; N];
    if total > 0.0 {
        for i in 0..N {
            if active[i] {
                share[i] = weights[i] / total;
            }
        }
    } else if !homes.is_empty() {
        notices.push(Notice {
            code: NoticeCode::NoScorableDimensions,
            place_index: None,
            place_label: None,
            text: None,
        });
    }

    // Medians of the final utilities, for explanations.
    let mut medians: [Option<f64>; N] = [None; N];
    for i in 0..N {
        if active[i] {
            let mut v: Vec<f64> = cells.iter().filter_map(|c| c[i].value).collect();
            medians[i] = Some(median(&mut v));
        }
    }

    let mut ranked = Vec::new();
    let mut near_misses = Vec::new();
    let mut flagged = Vec::new();
    for (home, c) in homes.iter().zip(cells.iter()) {
        let scored = score_home(profile, home, c, &active, &share, &medians);
        if scored.flagged && !opts.include_flagged {
            flagged.push(scored);
        } else if scored.near_miss {
            near_misses.push(scored);
        } else {
            ranked.push(scored);
        }
    }
    sort_and_rank(&mut ranked);
    sort_and_rank(&mut near_misses);
    sort_and_rank(&mut flagged);

    RankResult {
        profile_version: PROFILE_VERSION,
        reference_year,
        home_count: homes.len(),
        weights: Dimension::ALL
            .iter()
            .filter(|d| active[d.index()])
            .map(|d| WeightShare { dimension: *d, weight: weights[d.index()], share: share[d.index()] })
            .collect(),
        dropped,
        medians: Dimension::ALL
            .iter()
            .filter_map(|d| medians[d.index()].map(|m| (*d, m)))
            .collect(),
        ranked,
        near_misses,
        flagged,
        notices,
    }
}

/// Commute utility per home: per-place utilities (free-flow × traffic buffer against each place's
/// maximum), a missing place filled with that place's median over the set, combined as a
/// trips-per-week weighted mean.
fn fill_commute(
    profile: &Profile,
    homes: &[HomeFactors],
    commute: &CommuteMatrix,
    cells: &mut [[Cell; Dimension::COUNT]],
    notices: &mut Vec<Notice>,
) {
    let ci = Dimension::Commute.index();
    let places = &profile.places;
    let buffer = if profile.traffic_buffer.is_finite() {
        profile.traffic_buffer.clamp(MIN_TRAFFIC_BUFFER, MAX_TRAFFIC_BUFFER)
    } else {
        DEFAULT_TRAFFIC_BUFFER
    };
    let per_place: Vec<Vec<Option<(f64, bool)>>> = homes
        .iter()
        .map(|h| {
            let row = commute.homes.get(&h.id);
            places
                .iter()
                .enumerate()
                .map(|(p, place)| {
                    row.and_then(|r| r.get(p))
                        .copied()
                        .flatten()
                        .filter(|t| t.minutes.is_finite() && t.minutes >= 0.0 && place.max_minutes > 0.0)
                        .map(|t| (utility::commute_utility(t.minutes * buffer, place.max_minutes), t.estimated))
                })
                .collect()
        })
        .collect();
    let mut place_median: Vec<Option<f64>> = Vec::with_capacity(places.len());
    for (p, place) in places.iter().enumerate() {
        let mut v: Vec<f64> = per_place.iter().filter_map(|row| row[p].map(|(u, _)| u)).collect();
        if v.is_empty() {
            if !homes.is_empty() {
                notices.push(Notice {
                    code: NoticeCode::PlaceWithoutDriveTimes,
                    place_index: Some(p),
                    place_label: Some(place.label.clone()),
                    text: None,
                });
            }
            place_median.push(None);
        } else {
            place_median.push(Some(median(&mut v)));
        }
    }
    for (row, c) in per_place.iter().zip(cells.iter_mut()) {
        let (mut sum, mut wsum) = (0.0, 0.0);
        let (mut imputed, mut fallback) = (false, false);
        for (p, place) in places.iter().enumerate() {
            let Some(med) = place_median[p] else {
                continue;
            };
            let trips = if place.trips_per_week.is_finite() && place.trips_per_week > 0.0 {
                place.trips_per_week
            } else {
                DEFAULT_TRIPS_PER_WEEK
            };
            let u = match row[p] {
                Some((u, est)) => {
                    fallback |= est;
                    u
                }
                None => {
                    imputed = true;
                    med
                }
            };
            sum += trips * u;
            wsum += trips;
        }
        if wsum > 0.0 {
            c[ci] = Cell {
                value: Some(sum / wsum),
                estimated: if imputed {
                    Some(EstimateReason::ImputedMedian)
                } else if fallback {
                    Some(EstimateReason::RoadDistanceEstimate)
                } else {
                    None
                },
            };
        }
    }
}

fn score_home(
    profile: &Profile,
    home: &HomeFactors,
    cells: &[Cell; Dimension::COUNT],
    active: &[bool; Dimension::COUNT],
    share: &[f64; Dimension::COUNT],
    medians: &[Option<f64>; Dimension::COUNT],
) -> ScoredHome {
    let mut score = 0.0;
    let mut estimated_share = 0.0;
    let mut used_share = 0.0;
    let mut dimensions = Vec::with_capacity(Dimension::COUNT);
    let mut estimated = Vec::new();
    for d in Dimension::ALL {
        let i = d.index();
        if !active[i] {
            continue;
        }
        let cell = cells[i];
        let utility = cell.value.unwrap_or(0.0);
        let points = share[i] * utility;
        score += points;
        used_share += share[i];
        if let Some(reason) = cell.estimated {
            estimated_share += share[i];
            estimated.push(EstimatedDimension { dimension: d, reason, text: None });
        }
        dimensions.push(DimensionScore {
            dimension: d,
            utility,
            share: share[i],
            points,
            estimated: cell.estimated.is_some(),
        });
    }
    let coverage = if used_share > 0.0 { (1.0 - estimated_share / used_share).clamp(0.0, 1.0) } else { 0.0 };

    let mut capped_by: Vec<Dimension> = Vec::new();
    for d in &profile.critical {
        let c = cells[d.index()];
        let failing = active[d.index()]
            && c.estimated.is_none()
            && c.value.map_or(false, |u| u < CRITICAL_THRESHOLD);
        if failing && !capped_by.contains(d) {
            capped_by.push(*d);
        }
    }
    let (score, uncapped_score) = if !capped_by.is_empty() && score > CRITICAL_CAP {
        (CRITICAL_CAP, Some(score))
    } else {
        (score, None)
    };

    let (gates, unverified) = evaluate_gates(profile, home);
    let flagged = gates.iter().any(|g| g.kind == GateKind::DealBreaker);
    let near_miss = gates.iter().any(|g| g.kind == GateKind::MustHave);
    let (reasons, trade_offs) = explain::explain_dimensions(&dimensions, medians);

    ScoredHome {
        id: home.id.clone(),
        rank: 0,
        tied: false,
        score,
        uncapped_score,
        band: MatchBand::from_score(score),
        coverage,
        dimensions,
        estimated,
        capped_by,
        flagged,
        near_miss,
        gates,
        unverified,
        reasons,
        trade_offs,
    }
}

/// Sorts by score descending, then id, and assigns ranks: a home within [`TIE_MARGIN`] of its
/// group's leader shares the leader's rank.
fn sort_and_rank(list: &mut [ScoredHome]) {
    list.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    let mut i = 0;
    while i < list.len() {
        let lead = list[i].score;
        let mut j = i + 1;
        while j < list.len() && lead - list[j].score < TIE_MARGIN {
            j += 1;
        }
        let tied = j - i > 1;
        for h in &mut list[i..j] {
            h.rank = i + 1;
            h.tied = tied;
        }
        i = j;
    }
}

enum Outcome {
    Hit { value: Option<f64>, limit: Option<f64>, label: Option<String> },
    Pass,
    Unknown,
}

fn flag_outcome(f: &Option<Factor<bool>>, label: Option<String>) -> Outcome {
    match f {
        Some(f) if f.value => Outcome::Hit { value: None, limit: None, label },
        Some(_) => Outcome::Pass,
        None => Outcome::Unknown,
    }
}

fn screen_outcome(screen: Screen, profile: &Profile, home: &HomeFactors) -> Outcome {
    let g = &home.gates;
    match screen {
        Screen::SfhaFlood => flag_outcome(&g.sfha, g.flood_zone.clone()),
        Screen::VeryHighFire => flag_outcome(&g.very_high_fire, None),
        Screen::TJunction => flag_outcome(&g.t_junction, None),
        Screen::FreewayWithin500ft => match &g.freeway_within_500ft {
            Some(f) if f.value => Outcome::Hit {
                value: g.freeway_distance_m.map(|m| (m * FEET_PER_METRE).round()),
                limit: Some(500.0),
                label: None,
            },
            Some(_) => Outcome::Pass,
            None => Outcome::Unknown,
        },
        Screen::HoaOver => match (profile.hoa_max_monthly, home.cost.hoa_monthly.as_ref()) {
            (None, _) => Outcome::Pass,
            (Some(max), Some(h)) if h.value > max => {
                Outcome::Hit { value: Some(h.value), limit: Some(max), label: None }
            }
            (Some(_), Some(_)) => Outcome::Pass,
            (Some(_), None) => Outcome::Unknown,
        },
        Screen::StyleAvoid => {
            let avoided: Vec<StyleFamily> = profile
                .style_preferences
                .iter()
                .filter(|(_, p)| **p == StylePreference::Avoid)
                .map(|(f, _)| *f)
                .collect();
            if avoided.is_empty() {
                return Outcome::Pass;
            }
            let mass: f64 = avoided.iter().map(|f| home.style.probability(*f)).sum();
            if mass >= STYLE_AVOID_MIN_PROBABILITY {
                let mut top = avoided[0];
                for f in &avoided[1..] {
                    if home.style.probability(*f) > home.style.probability(top) {
                        top = *f;
                    }
                }
                Outcome::Hit { value: Some(mass), limit: None, label: Some(top.key().to_string()) }
            } else if home.style.is_evidence_backed() {
                Outcome::Pass
            } else {
                Outcome::Unknown
            }
        }
        Screen::AddressDigits => match &g.house_number {
            None => Outcome::Unknown,
            Some(n) => {
                let mut found = String::new();
                for d in &profile.avoid_digits {
                    if *d <= 9 {
                        let c = char::from(b'0' + *d);
                        if n.contains(c) && !found.contains(c) {
                            found.push(c);
                        }
                    }
                }
                if found.is_empty() {
                    Outcome::Pass
                } else {
                    Outcome::Hit { value: None, limit: None, label: Some(found) }
                }
            }
        },
    }
}

fn evaluate_gates(profile: &Profile, home: &HomeFactors) -> (Vec<GateHit>, Vec<Screen>) {
    let mut hits = Vec::new();
    let mut unverified = Vec::new();
    for (kind, list) in [(GateKind::DealBreaker, &profile.deal_breakers), (GateKind::MustHave, &profile.must_haves)] {
        for s in list {
            match screen_outcome(*s, profile, home) {
                Outcome::Hit { value, limit, label } => hits.push(GateHit {
                    reason: GateReason::from(*s),
                    kind,
                    value,
                    limit,
                    label,
                    text: None,
                }),
                Outcome::Pass => {}
                Outcome::Unknown => {
                    if !unverified.contains(s) {
                        unverified.push(*s);
                    }
                }
            }
        }
    }
    if !profile.property_types.is_empty() {
        if let Some(kind) = home.home.property_type.as_ref().map(|f| f.value) {
            if kind != PropertyKind::Other && !profile.property_types.contains(&kind) {
                hits.push(GateHit {
                    reason: GateReason::PropertyType,
                    kind: GateKind::MustHave,
                    value: None,
                    limit: None,
                    label: Some(kind.key().to_string()),
                    text: None,
                });
            }
        }
    }
    (hits, unverified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_handles_odd_and_even_counts() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&mut [48.0]), 48.0);
    }

    #[test]
    fn ties_group_from_the_leader() {
        let mk = |id: &str, score: f64| ScoredHome {
            id: id.into(),
            rank: 0,
            tied: false,
            score,
            uncapped_score: None,
            band: MatchBand::from_score(score),
            coverage: 1.0,
            dimensions: vec![],
            estimated: vec![],
            capped_by: vec![],
            flagged: false,
            near_miss: false,
            gates: vec![],
            unverified: vec![],
            reasons: vec![],
            trade_offs: vec![],
        };
        let mut list = vec![mk("c", 73.8), mk("a", 75.0), mk("b", 74.4), mk("d", 70.0), mk("e", 70.0)];
        sort_and_rank(&mut list);
        let got: Vec<(&str, usize, bool)> = list.iter().map(|h| (h.id.as_str(), h.rank, h.tied)).collect();
        assert_eq!(got, vec![("a", 1, true), ("b", 1, true), ("c", 3, false), ("d", 4, true), ("e", 4, true)]);
    }

    #[test]
    fn commute_matrix_set_and_get() {
        let mut m = CommuteMatrix::new();
        m.set("h1", 1, 12.5);
        m.set_estimated("h1", 0, 20.0);
        assert_eq!(m.get("h1", 1), Some(CommuteTime { minutes: 12.5, estimated: false }));
        assert_eq!(m.get("h1", 0), Some(CommuteTime { minutes: 20.0, estimated: true }));
        assert_eq!(m.get("h1", 2), None);
        assert_eq!(m.get("h2", 0), None);
    }
}
