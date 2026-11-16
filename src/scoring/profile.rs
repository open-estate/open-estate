//! The buyer's "Your Match" profile: one versioned JSON document holding weights, budget, needs,
//! places, style picks, must-haves and deal-breakers (plan v3, sections 5.3 and 5.10). It lives in
//! the browser today and is sent with every request; the server keeps nothing.
//!
//! The profile asks about needs, features and places, never about who the buyer is (plan section 6).

use super::catalog::{
    Dimension, Feature, LoanType, PropertyKind, SchoolLevel, Screen, StyleFamily, StylePreference,
};
use super::presets::PRESETS;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Current profile document version.
pub const PROFILE_VERSION: u32 = 1;
pub const DEFAULT_TRAFFIC_BUFFER: f64 = 1.3;
pub const MIN_TRAFFIC_BUFFER: f64 = 1.0;
/// Buyer-facing UI profile traffic-buffer cap (`1.0..=2.0`); note `modules::commute::MAX_TRAFFIC_BUFFER`
/// is intentionally `3.0` to allow operator `OSRM_TRAFFIC_BUFFER` overrides during congestion testing.
pub const MAX_TRAFFIC_BUFFER: f64 = 2.0;
/// Down payment assumed by the report's headline cost model.
pub const DEFAULT_DOWN_PAYMENT_PCT: f64 = 20.0;
pub const DEFAULT_TRIPS_PER_WEEK: f64 = 5.0;
/// The router takes up to five places as sources per table call (plan 5.9).
pub const MAX_PLACES: usize = 5;
pub const MAX_WEIGHT: f64 = 100.0;
pub const MAX_PLACE_MINUTES: f64 = 240.0;
pub const MAX_TRIPS_PER_WEEK: f64 = 50.0;
pub const MAX_LABEL_CHARS: usize = 80;

/// Canonical JSON body keys accepted on `Profile` documents across `/api/match/*` and `/api/me/profile`.
pub const ALLOWED_PROFILE_KEYS: &[&str] = &[
    "version",
    "weights",
    "preset",
    "budget",
    "downPaymentPct",
    "loanType",
    "minBeds",
    "minBaths",
    "minSqft",
    "minLotSqft",
    "propertyTypes",
    "multigenerational",
    "schoolLevels",
    "places",
    "trafficBuffer",
    "stylePreferences",
    "mustHaves",
    "dealBreakers",
    "hoaMaxMonthly",
    "avoidDigits",
    "featureWishes",
    "critical",
];

fn default_down_payment_pct() -> f64 {
    DEFAULT_DOWN_PAYMENT_PCT
}

fn default_traffic_buffer() -> f64 {
    DEFAULT_TRAFFIC_BUFFER
}

fn default_trips_per_week() -> f64 {
    DEFAULT_TRIPS_PER_WEEK
}

/// The profile document, version 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// Must be [`PROFILE_VERSION`].
    pub version: u32,
    /// Weight per dimension, 0–100. Missing dimensions weigh 0. Weights are normalized at use, so
    /// they need not sum to 100; at least one must be above 0.
    pub weights: BTreeMap<Dimension, f64>,
    /// Key of the preset the weights started from (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default)]
    pub budget: Budget,
    /// Down payment in percent of the price (0–100). Ignored for cash.
    #[serde(default = "default_down_payment_pct")]
    pub down_payment_pct: f64,
    #[serde(default)]
    pub loan_type: LoanType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_beds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_baths: Option<f64>,
    /// Living-area target in square feet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_sqft: Option<f64>,
    /// Lot-size target in square feet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_lot_sqft: Option<f64>,
    /// Wanted property types; a home of another known type becomes a near miss. Empty = any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub property_types: Vec<PropertyKind>,
    /// Household plans for extended family: an existing ADU counts toward size and baths and layout
    /// weighs more (plan 5.12).
    #[serde(default)]
    pub multigenerational: bool,
    /// School levels that matter. Empty = all three.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub school_levels: Vec<SchoolLevel>,
    /// Places the buyer goes (work, family, ...), up to [`MAX_PLACES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<Place>,
    /// Multiplier on free-flow drive times for peak traffic, 1.0–2.0.
    #[serde(default = "default_traffic_buffer")]
    pub traffic_buffer: f64,
    /// Love / fine / avoid per style family; families not listed count as "fine".
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub style_preferences: BTreeMap<StyleFamily, StylePreference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_haves: Vec<Screen>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deal_breakers: Vec<Screen>,
    /// Monthly HOA limit used by the `hoa_over` screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hoa_max_monthly: Option<f64>,
    /// Digits (0–9) used by the `address_digits` screen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub avoid_digits: Vec<u8>,
    /// Character features the buyer would like (part of the style dimension).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feature_wishes: Vec<Feature>,
    /// Dimensions where a real score under 40 caps the total below "Strong match" (plan 5.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub critical: Vec<Dimension>,
}

/// Budget: a maximum price, a monthly budget, or both.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_price: Option<f64>,
    /// All-in monthly cost budget (payment, tax, insurance, maintenance, HOA).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<f64>,
}

/// Travel mode. Driving only for now (plan 5.9).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TravelMode {
    #[default]
    Drive,
}

/// A place the buyer goes. Any label and any address are allowed (plan section 6, rule 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    pub label: String,
    pub lat: f64,
    pub lon: f64,
    /// Longest acceptable trip, in minutes, after the traffic buffer.
    pub max_minutes: f64,
    #[serde(default = "default_trips_per_week")]
    pub trips_per_week: f64,
    #[serde(default)]
    pub mode: TravelMode,
}

/// One validation problem, with the JSON field it concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileError {
    /// Path such as `weights.cost` or `places[0].lat`; empty for document-level problems.
    pub field: String,
    /// Stable machine-readable code (`out_of_range`, `missing_parameter`, ...).
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.field, self.message)
        }
    }
}

impl std::error::Error for ProfileError {}

impl Default for Profile {
    /// Version 1 with the Balanced preset's weights and every other field at its default.
    fn default() -> Self {
        Profile {
            version: PROFILE_VERSION,
            weights: PRESETS[0].weights_map(),
            preset: None,
            budget: Budget::default(),
            down_payment_pct: DEFAULT_DOWN_PAYMENT_PCT,
            loan_type: LoanType::Conventional,
            min_beds: None,
            min_baths: None,
            min_sqft: None,
            min_lot_sqft: None,
            property_types: Vec::new(),
            multigenerational: false,
            school_levels: Vec::new(),
            places: Vec::new(),
            traffic_buffer: DEFAULT_TRAFFIC_BUFFER,
            style_preferences: BTreeMap::new(),
            must_haves: Vec::new(),
            deal_breakers: Vec::new(),
            hoa_max_monthly: None,
            avoid_digits: Vec::new(),
            feature_wishes: Vec::new(),
            critical: Vec::new(),
        }
    }
}

struct Errors(Vec<ProfileError>);

impl Errors {
    fn push(&mut self, field: impl Into<String>, code: &'static str, message: impl Into<String>) {
        self.0.push(ProfileError { field: field.into(), code, message: message.into() });
    }

    /// Checks `lo <= v <= hi` (finite); `lo_open` makes the lower bound exclusive.
    fn range(&mut self, field: impl Into<String>, v: f64, lo: f64, hi: f64, lo_open: bool) {
        let field = field.into();
        if !v.is_finite() {
            self.push(field, "not_finite", "must be a finite number");
        } else if (lo_open && v <= lo) || (!lo_open && v < lo) || v > hi {
            let lo_txt = if lo_open { format!("above {lo}") } else { format!("at least {lo}") };
            self.push(field, "out_of_range", format!("must be {lo_txt} and at most {hi} (got {v})"));
        }
    }
}

impl Profile {
    /// Parses and validates a profile document.
    pub fn from_json(text: &str) -> Result<Profile, Vec<ProfileError>> {
        let p: Profile = serde_json::from_str(text).map_err(|e| {
            vec![ProfileError { field: String::new(), code: "invalid_document", message: e.to_string() }]
        })?;
        p.validate()?;
        Ok(p)
    }

    /// Checks every field and returns all problems at once.
    pub fn validate(&self) -> Result<(), Vec<ProfileError>> {
        let mut e = Errors(Vec::new());
        if self.version != PROFILE_VERSION {
            e.push(
                "version",
                "unsupported_version",
                format!("profile version {} is not supported (expected {PROFILE_VERSION})", self.version),
            );
        }

        let mut any_positive = false;
        for (d, w) in &self.weights {
            let field = format!("weights.{}", d.key());
            if !w.is_finite() || *w < 0.0 || *w > MAX_WEIGHT {
                e.range(field, *w, 0.0, MAX_WEIGHT, false);
            } else if *w > 0.0 {
                any_positive = true;
            }
        }
        if !any_positive {
            e.push("weights", "no_positive_weight", "at least one dimension needs a weight above 0");
        }

        if let Some(p) = self.budget.max_price {
            e.range("budget.maxPrice", p, 0.0, 1e9, true);
        }
        if let Some(m) = self.budget.monthly {
            e.range("budget.monthly", m, 0.0, 1e7, true);
        }
        e.range("downPaymentPct", self.down_payment_pct, 0.0, 100.0, false);

        if let Some(v) = self.min_beds {
            e.range("minBeds", v, 0.0, 20.0, false);
        }
        if let Some(v) = self.min_baths {
            e.range("minBaths", v, 0.0, 20.0, false);
        }
        if let Some(v) = self.min_sqft {
            e.range("minSqft", v, 0.0, 100_000.0, true);
        }
        if let Some(v) = self.min_lot_sqft {
            e.range("minLotSqft", v, 0.0, 50_000_000.0, true);
        }

        if self.places.len() > MAX_PLACES {
            e.push(
                "places",
                "too_many",
                format!("at most {MAX_PLACES} places are allowed (got {})", self.places.len()),
            );
        }
        for (i, pl) in self.places.iter().enumerate() {
            let label = pl.label.trim();
            if label.is_empty() {
                e.push(format!("places[{i}].label"), "empty", "a place needs a label");
            } else if label.chars().count() > MAX_LABEL_CHARS {
                e.push(
                    format!("places[{i}].label"),
                    "too_long",
                    format!("labels are limited to {MAX_LABEL_CHARS} characters"),
                );
            }
            e.range(format!("places[{i}].lat"), pl.lat, -90.0, 90.0, false);
            e.range(format!("places[{i}].lon"), pl.lon, -180.0, 180.0, false);
            e.range(format!("places[{i}].maxMinutes"), pl.max_minutes, 0.0, MAX_PLACE_MINUTES, true);
            e.range(format!("places[{i}].tripsPerWeek"), pl.trips_per_week, 0.0, MAX_TRIPS_PER_WEEK, true);
        }

        e.range("trafficBuffer", self.traffic_buffer, MIN_TRAFFIC_BUFFER, MAX_TRAFFIC_BUFFER, false);

        for (name, list) in [("mustHaves", &self.must_haves), ("dealBreakers", &self.deal_breakers)] {
            for (i, s) in list.iter().enumerate() {
                if list[..i].contains(s) {
                    e.push(format!("{name}[{i}]"), "duplicate", format!("{} is listed twice", s.key()));
                }
            }
        }
        for s in &self.must_haves {
            if self.deal_breakers.contains(s) {
                e.push(
                    "mustHaves",
                    "conflict",
                    format!("{} cannot be both a must-have and a deal-breaker", s.key()),
                );
            }
        }
        if let Some(h) = self.hoa_max_monthly {
            e.range("hoaMaxMonthly", h, 0.0, 1e6, false);
        } else if self.uses_screen(Screen::HoaOver) {
            e.push("hoaMaxMonthly", "missing_parameter", "hoa_over needs a monthly HOA limit");
        }
        for (i, d) in self.avoid_digits.iter().enumerate() {
            if *d > 9 {
                e.push(format!("avoidDigits[{i}]"), "out_of_range", format!("digits are 0–9 (got {d})"));
            }
        }
        if self.uses_screen(Screen::AddressDigits) && self.avoid_digits.is_empty() {
            e.push("avoidDigits", "missing_parameter", "address_digits needs at least one digit to avoid");
        }
        if self.uses_screen(Screen::StyleAvoid)
            && !self.style_preferences.values().any(|p| *p == StylePreference::Avoid)
        {
            e.push(
                "stylePreferences",
                "missing_parameter",
                "style_avoid needs at least one style family marked avoid",
            );
        }
        for (i, d) in self.critical.iter().enumerate() {
            if self.weight(*d) <= 0.0 {
                e.push(
                    format!("critical[{i}]"),
                    "no_weight",
                    format!("critical dimension {} needs a weight above 0", d.key()),
                );
            }
        }

        if e.0.is_empty() {
            Ok(())
        } else {
            Err(e.0)
        }
    }

    /// Weight of `d` as used by the engine: 0 when missing, negative or not finite; at most 100.
    pub fn weight(&self, d: Dimension) -> f64 {
        match self.weights.get(&d) {
            Some(w) if w.is_finite() && *w > 0.0 => w.min(MAX_WEIGHT),
            _ => 0.0,
        }
    }

    /// [`Profile::weight`] for every dimension, in [`Dimension::ALL`] order.
    pub fn weight_vector(&self) -> [f64; Dimension::COUNT] {
        let mut w = [0.0; Dimension::COUNT];
        for d in Dimension::ALL {
            w[d.index()] = self.weight(d);
        }
        w
    }

    /// Preference value for a family: love 1.0, fine 0.6, avoid 0; unlisted families count as fine.
    pub fn style_value(&self, family: StyleFamily) -> f64 {
        self.style_preferences
            .get(&family)
            .copied()
            .unwrap_or(StylePreference::Fine)
            .value()
    }

    /// The selected school levels, or all three when none are selected.
    pub fn effective_school_levels(&self) -> Vec<SchoolLevel> {
        if self.school_levels.is_empty() {
            return SchoolLevel::ALL.to_vec();
        }
        let mut out = Vec::new();
        for l in SchoolLevel::ALL {
            if self.school_levels.contains(&l) {
                out.push(l);
            }
        }
        out
    }

    /// True when the screen is a must-have or a deal-breaker.
    pub fn uses_screen(&self, s: Screen) -> bool {
        self.must_haves.contains(&s) || self.deal_breakers.contains(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_document_parses_with_defaults() {
        let p = Profile::from_json(r#"{"version":1,"weights":{"cost":50,"schools":50}}"#).unwrap();
        assert_eq!(p.traffic_buffer, DEFAULT_TRAFFIC_BUFFER);
        assert_eq!(p.down_payment_pct, DEFAULT_DOWN_PAYMENT_PCT);
        assert_eq!(p.loan_type, LoanType::Conventional);
        assert_eq!(p.weight(Dimension::Commute), 0.0);
        assert_eq!(p.effective_school_levels().len(), 3);
        let round_trip: Profile = serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(round_trip, p);
    }

    #[test]
    fn unknown_dimension_is_rejected() {
        let err = Profile::from_json(r#"{"version":1,"weights":{"vibes":50}}"#).unwrap_err();
        assert_eq!(err[0].code, "invalid_document");
    }
}
