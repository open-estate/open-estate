//! Objective house profile, architectural character clues, separate `Home quality` sub-score,
//! and strictly in-memory buyer tour photo analysis (`Phase 6`).
//!
//! Guardrails:
//! - `Home quality` is an objective 0–100 sub-score shown on its own card; it never alters `Open Estate Score`.
//! - Buyer tour photos are analyzed in memory ONLY (`stored_bytes == 0`) — never saved to disk, DB, or caches.
//! - Listing photos remain disabled per connector in v1 (`ALLOW_LISTING_PHOTOS_*` default false), and
//!   Google Street View and RapidAPI images are hard-blocked (`false` even if env var is set).

use crate::ai::llm::{parse_json_loose, resolve_models};
use crate::guardrails::{inspect_output_guardrail, scrub_sensitive_text, IMMUTABLE_SYSTEM_PREAMBLE};
use crate::scoring::catalog::{style_families_for_value, Feature, StyleFamily};
use crate::scoring::factors::{HomeFactors, StyleSource};
use crate::scoring::utility::{age_utility, lerp};
use crate::shared::{EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub const MAX_BUYER_PHOTOS: usize = 12;
pub const MAX_BUYER_PHOTO_BYTES: usize = 10 * 1024 * 1024;
pub const BUYER_PHOTO_NOTICE: &str =
    "In-memory only; 0 bytes stored; AI guess (does not affect Match Score)";

const ALLOWED_PHOTO_MIMES: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/gif"];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuyerPhotoInput {
    pub filename: String,
    pub mime_type: String,
    /// Base64 payload (or data URI); inspected in memory only and dropped immediately.
    pub data_base64: String,
    #[serde(default)]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuyerPhotoDetail {
    pub filename: String,
    pub mime_type: String,
    pub byte_len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default)]
    pub has_exif: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuyerPhotoAnalysis {
    pub inferred_styles: Vec<String>,
    pub character_features: Vec<String>,
    pub condition_notes: Vec<String>,
    pub model_used: String,
    /// Always 0 — guarantees buyer photos are never stored on disk or in any database.
    pub stored_bytes: usize,
    pub analyzed_count: usize,
    pub notice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_level: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub photo_details: Vec<BuyerPhotoDetail>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MultigenerationalSignals {
    pub single_story: bool,
    pub has_main_level_primary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_level_bedrooms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_level_bathrooms: Option<f64>,
    pub has_existing_adu: bool,
    pub strong_adu_potential: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parking_spaces: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HomeQualityScore {
    pub score: Option<f64>,
    pub band: String,
    pub condition_score: Option<f64>,
    pub systems_score: Option<f64>,
    pub functionality_score: Option<f64>,
    pub style_family: Option<String>,
    pub style_label: Option<String>,
    pub character_features: Vec<String>,
    pub source_notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_energy_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lot_utility_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style_basis: Option<String>,
    #[serde(default)]
    pub style_evidence_backed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub style_evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub style_probabilities: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multigenerational_signals: Option<MultigenerationalSignals>,
}

/// Checks whether automated listing-photo ingestion is explicitly permitted for `connector`.
/// Defaults to `false` across all connectors in v1 pending per-feed MLS/license checks.
/// Hard-blocks Google Street View (`google_street_view`, `streetview`) and `rapidapi` even when
/// an environment override is set.
pub fn listing_photos_allowed(connector: &str, env: &EnvConfig) -> bool {
    let norm: String = connector
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if norm.is_empty()
        || norm.contains("streetview")
        || norm.contains("rapidapi")
    {
        return false;
    }

    let key = format!(
        "ALLOW_LISTING_PHOTOS_{}",
        connector
            .trim()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
            .collect::<String>()
    );
    env.get(&key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// Objective Home Quality computation (strictly separate from Open Estate Score)
// ---------------------------------------------------------------------------------------------

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn num_val(v: Option<&Value>) -> Option<f64> {
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

fn collect_strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => {
            let t = s.trim();
            if !t.is_empty() {
                out.push(t);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

fn facts_corpus(facts: &Value) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for key in [
        "renovations",
        "roof",
        "heating",
        "cooling",
        "foundation",
        "garage",
        "parking",
        "parkingFeatures",
        "interiorFeatures",
        "exteriorFeatures",
        "features",
        "details",
        "permitNotes",
        "storiesNote",
        "sqftNote",
        "lotNote",
        "remarks",
        "publicRemarks",
        "description",
    ] {
        if let Some(v) = facts.get(key) {
            collect_strings(v, &mut parts);
        }
    }
    if let Some(s) = facts.pointer("/mlsListing/remarks").and_then(Value::as_str) {
        parts.push(s);
    }
    parts.join(" | ")
}

fn scan_permit_categories(facts: &Value) -> BTreeSet<&'static str> {
    let mut cats = BTreeSet::new();
    let Some(permits) = facts.get("permits").and_then(Value::as_array) else {
        return cats;
    };
    for p in permits {
        let status = p
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if status.contains("cancel")
            || status.contains("void")
            || status.contains("denied")
            || status.contains("withdrawn")
        {
            continue;
        }
        let mut blob = String::new();
        for k in ["type", "category", "description", "workType", "title"] {
            if let Some(s) = p.get(k).and_then(Value::as_str) {
                blob.push(' ');
                blob.push_str(s);
            }
        }
        let low = blob.to_ascii_lowercase();
        if low.trim().is_empty() {
            continue;
        }
        if low.contains("roof") || low.contains("reroof") || low.contains("shingle") {
            cats.insert("roof");
        }
        if low.contains("hvac")
            || low.contains("furnace")
            || low.contains("air condition")
            || low.contains("a/c")
            || low.contains("heat pump")
            || low.contains("mechanical")
        {
            cats.insert("hvac");
        }
        if low.contains("electr")
            || low.contains("panel")
            || low.contains("rewire")
            || low.contains("amp")
            || low.contains("ev ")
            || low.contains("charger")
        {
            cats.insert("electrical");
        }
        if low.contains("plumb")
            || low.contains("repipe")
            || low.contains("water heater")
            || low.contains("tankless")
            || low.contains("sewer")
        {
            cats.insert("plumbing");
        }
        if low.contains("kitchen") {
            cats.insert("kitchen");
        }
        if low.contains("bath") || low.contains("shower") {
            cats.insert("bath");
        }
        if low.contains("remodel")
            || low.contains("renovat")
            || low.contains("addition")
            || low.contains("adu")
            || low.contains("solar")
            || low.contains("photovoltaic")
            || low.contains("seismic")
            || low.contains("foundation")
        {
            cats.insert("remodel_or_major");
        }
    }
    cats
}

fn infer_parking_spaces(facts: &Value, factor_parking: Option<f64>) -> Option<f64> {
    if let Some(p) = factor_parking {
        return Some(p);
    }
    static GARAGE_NUM_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*[- ]?car").expect("valid regex"));
    let mut best: Option<f64> = None;
    for key in ["garage", "parking", "parkingFeatures"] {
        if let Some(v) = facts.get(key) {
            let mut strs = Vec::new();
            collect_strings(v, &mut strs);
            for s in strs {
                if let Some(c) = GARAGE_NUM_RE.captures(s).and_then(|c| c.get(1)) {
                    if let Ok(n) = c.as_str().parse::<f64>() {
                        best = Some(best.map_or(n, |b| b.max(n)));
                    }
                }
                let low = s.to_ascii_lowercase();
                if (low.contains("tandem") || low.contains("rv") || low.contains("driveway"))
                    && best.is_some()
                {
                    best = best.map(|b| (b + 1.0).min(6.0));
                }
            }
        }
    }
    best
}

fn mean_opt(vals: &[f64]) -> Option<f64> {
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }
}

/// Computes the objective `Home quality` sub-score & character metadata for a property report.
/// Built from (a) condition & systems, (b) layout efficiency, (c) light & energy, (d) lot utility,
/// and (e) all 11 character features + consolidated architectural style family/evidence.
pub fn compute_home_quality(report: &Value) -> HomeQualityScore {
    let (brand_name, _) = crate::shared::report_brand(report);
    let facts = report.get("facts").filter(|v| v.is_object());
    let factors = HomeFactors::from_report(report);
    let mut source_notes: Vec<String> = Vec::new();

    let corpus_raw = facts.map(facts_corpus).unwrap_or_default();
    let corpus = corpus_raw.to_ascii_lowercase();
    let permit_cats = facts.map(scan_permit_categories).unwrap_or_default();

    // ---- (a) Condition & systems ----
    let effective_year = factors
        .home
        .effective_year
        .as_ref()
        .or(factors.home.year_built.as_ref())
        .map(|f| f.value);
    let age_base = effective_year.map(|y| age_utility(f64::from((factors.as_of_year - y).max(0))));

    let remodeled = corpus.contains("remodeled")
        || corpus.contains("renovated")
        || corpus.contains("newly updated")
        || corpus.contains("rebuilt")
        || corpus.contains("updated kitchen")
        || corpus.contains("remodeled bath")
        || corpus.contains("upgraded bath");
    let has_new_roof = corpus.contains("new roof")
        || corpus.contains("newer roof")
        || permit_cats.contains("roof");
    let has_central_hvac = corpus.contains("central a/c")
        || corpus.contains("central air")
        || corpus.contains("heat pump")
        || corpus.contains("updated hvac")
        || corpus.contains("mini-split")
        || permit_cats.contains("hvac");
    let has_updated_elec_plumb = corpus.contains("updated electrical")
        || corpus.contains("updated plumbing")
        || corpus.contains("repipe")
        || corpus.contains("copper plumbing")
        || corpus.contains("pex")
        || corpus.contains("200 amp")
        || corpus.contains("200-amp")
        || permit_cats.contains("electrical")
        || permit_cats.contains("plumbing");
    let has_solar_panels = corpus.contains("solar")
        || corpus.contains("photovoltaic")
        || facts
            .and_then(|f| f.get("solar"))
            .map_or(false, |v| !v.is_null());
    let has_battery_backup = corpus.contains("powerwall")
        || corpus.contains("enphase iq")
        || corpus.contains("franklinwh")
        || corpus.contains("pwrcell")
        || corpus.contains("battery backup")
        || corpus.contains("energy storage");

    let permit_bonus = (permit_cats.len() as f64 * 3.5).min(16.0);
    let remodel_bonus = if remodeled { 9.0 } else { 0.0 };

    let condition_score = if age_base.is_some() || remodeled || !permit_cats.is_empty() {
        let base = age_base.unwrap_or(62.0);
        let mut s = base + remodel_bonus + permit_bonus;
        if corpus.contains("fixer") || corpus.contains("needs work") || corpus.contains("as-is") {
            s -= 14.0;
        }
        Some(round1(s.clamp(15.0, 100.0)))
    } else {
        None
    };

    let has_any_system_info = facts.map_or(false, |f| {
        f.get("roof").is_some()
            || f.get("heating").is_some()
            || f.get("cooling").is_some()
            || f.get("foundation").is_some()
    }) || has_new_roof
        || has_central_hvac
        || has_updated_elec_plumb
        || has_solar_panels;

    let mech_systems_score = if has_any_system_info || age_base.is_some() {
        let mut s = age_base.map(|b| (0.5 * b + 35.0).max(52.0)).unwrap_or(64.0);
        if has_new_roof {
            s += 7.0;
        }
        if has_central_hvac {
            s += 8.0;
        }
        if has_updated_elec_plumb {
            s += 8.0;
        }
        if has_solar_panels {
            s += 7.0;
        }
        if has_battery_backup {
            s += 4.0;
        }
        if corpus.contains("seismic") || corpus.contains("bolted") {
            s += 4.0;
        }
        Some(round1(s.clamp(20.0, 100.0)))
    } else {
        None
    };

    let condition_and_systems = match (condition_score, mech_systems_score) {
        (Some(c), Some(m)) => Some(round1(0.55 * c + 0.45 * m)),
        (Some(c), None) => Some(c),
        (None, Some(m)) => Some(m),
        (None, None) => None,
    };

    // ---- (b) Layout efficiency ----
    let beds = factors.home.beds.as_ref().map(|f| f.value);
    let baths = factors.home.baths.as_ref().map(|f| f.value);
    let sqft = factors.home.sqft.as_ref().map(|f| f.value);
    let stories = factors.home.stories.as_ref().map(|f| f.value);
    let single_story = stories.map_or(false, |s| s < 1.5);
    let parking_spaces = infer_parking_spaces(
        facts.unwrap_or(&Value::Null),
        factors.home.parking_spaces.as_ref().map(|f| f.value),
    );
    let has_main_level_primary = factors
        .features
        .as_ref()
        .map_or(false, |f| f.value.contains(&Feature::MainLevelPrimary));

    let mut layout_parts: Vec<f64> = Vec::new();
    if let (Some(sq), Some(bd)) = (sqft, beds) {
        let per_bed = sq / bd.max(1.0);
        layout_parts.push(lerp(
            per_bed,
            &[(220.0, 45.0), (350.0, 70.0), (475.0, 88.0), (620.0, 100.0)],
        ));
    }
    if let (Some(bd), Some(ba)) = (beds, baths) {
        let bath_per_bed = ba / bd.max(1.0);
        layout_parts.push(lerp(
            bath_per_bed,
            &[(0.30, 40.0), (0.50, 76.0), (0.67, 92.0), (0.85, 100.0)],
        ));
    }
    let layout_score = if !layout_parts.is_empty() || stories.is_some() {
        let mut s = mean_opt(&layout_parts).unwrap_or(68.0);
        if single_story {
            s += 9.0;
        } else if has_main_level_primary
            || (factors.home.main_level_bedrooms.unwrap_or(0.0) >= 1.0
                && factors.home.main_level_bathrooms.unwrap_or(0.0) >= 1.0)
        {
            s += 6.0;
        }
        if let Some(pk) = parking_spaces {
            if pk >= 2.0 {
                s += 6.0;
            } else if pk >= 1.0 {
                s += 3.0;
            }
        }
        Some(round1(s.clamp(20.0, 100.0)))
    } else {
        None
    };

    // ---- (c) Light & energy ----
    let fs_data = report.pointer("/modules/fengshui/data");
    let facing_compass = fs_data
        .and_then(|d| {
            d.pointer("/orientation/facingCompass")
                .or_else(|| d.pointer("/facing/facingCompass"))
        })
        .and_then(Value::as_str);
    let facing_light = facing_compass.map(|dir| match dir {
        "S" | "SE" => 88.0,
        "SW" | "E" => 80.0,
        "W" | "NE" => 70.0,
        _ => 62.0,
    });
    let sun_hours = report
        .pointer("/decisionFactors/solar/sunHoursPerDay")
        .and_then(|v| num_val(Some(v)));
    let solar_score = sun_hours.map(|sh| {
        lerp(sh, &[(3.0, 58.0), (4.5, 78.0), (5.5, 92.0), (6.5, 98.0)])
    });
    let has_natural_light = factors
        .features
        .as_ref()
        .map_or(false, |f| f.value.contains(&Feature::NaturalLight));
    let has_ev = factors
        .features
        .as_ref()
        .map_or(false, |f| f.value.contains(&Feature::EvReady));

    let mut light_parts: Vec<f64> = Vec::new();
    if let Some(fl) = facing_light {
        light_parts.push(fl);
    }
    if let Some(ss) = solar_score {
        light_parts.push(ss);
    }
    let light_energy_score = if !light_parts.is_empty()
        || has_solar_panels
        || has_natural_light
        || has_central_hvac
    {
        let mut s = mean_opt(&light_parts).unwrap_or(66.0);
        if has_solar_panels {
            s += 10.0;
        }
        if has_natural_light {
            s += 6.0;
        }
        if has_central_hvac {
            s += 4.0;
        }
        if has_ev {
            s += 3.0;
        }
        Some(round1(s.clamp(20.0, 100.0)))
    } else {
        None
    };

    let systems_score = match (mech_systems_score, light_energy_score) {
        (Some(m), Some(le)) => Some(round1(0.55 * m + 0.45 * le)),
        (Some(m), None) => Some(m),
        (None, Some(le)) => Some(le),
        (None, None) => None,
    };

    // ---- (d) Lot utility ----
    let open_lot_sqft = report
        .pointer("/decisionFactors/lotAndAdu/openLotSqft")
        .and_then(|v| num_val(Some(v)))
        .or_else(|| {
            let lot = factors.home.lot_sqft.as_ref().map(|f| f.value)?;
            let sq = sqft?;
            let st = stories.unwrap_or(1.0).max(1.0);
            Some((lot - sq / st).max(0.0))
        });
    let lot_sqft = factors.home.lot_sqft.as_ref().map(|f| f.value);
    let lot_base = open_lot_sqft
        .map(|open| {
            lerp(
                open,
                &[(500.0, 42.0), (1800.0, 66.0), (3500.0, 82.0), (6000.0, 93.0), (9000.0, 99.0)],
            )
        })
        .or_else(|| {
            lot_sqft.map(|lt| {
                lerp(lt, &[(1500.0, 45.0), (5000.0, 72.0), (8000.0, 86.0), (12000.0, 95.0)])
            })
        });
    let has_adu = factors.home.has_adu();
    let strong_adu = factors.home.strong_adu_potential();
    let adu_feas = factors
        .home
        .adu_feasibility
        .as_ref()
        .map(|f| f.value.as_str())
        .unwrap_or("");
    let lot_utility_score = lot_base.map(|mut s| {
        if has_adu {
            s += 12.0;
        } else if strong_adu {
            s += 8.0;
        } else if adu_feas.starts_with("Promising") {
            s += 4.0;
        }
        round1(s.clamp(20.0, 100.0))
    });

    let functionality_score = match (layout_score, lot_utility_score) {
        (Some(l), Some(lu)) => Some(round1(0.55 * l + 0.45 * lu)),
        (Some(l), None) => Some(l),
        (None, Some(lu)) => Some(lu),
        (None, None) => None,
    };

    // ---- Overall objective Home Quality (0-100) ----
    let pillars = [
        (0.35, condition_and_systems),
        (0.25, layout_score),
        (0.20, light_energy_score),
        (0.20, lot_utility_score),
    ];
    let (mut w_sum, mut v_sum) = (0.0, 0.0);
    for (w, val) in pillars {
        if let Some(v) = val {
            w_sum += w;
            v_sum += w * v;
        }
    }
    let score = if w_sum > 0.0 {
        Some(round1((v_sum / w_sum).clamp(0.0, 100.0)))
    } else {
        None
    };
    let band = match score {
        Some(s) if s >= 82.0 => "Excellent",
        Some(s) if s >= 70.0 => "Good",
        Some(s) if s >= 56.0 => "Fair",
        Some(_) => "Modest",
        None => "Insufficient Data",
    }
    .to_string();

    // ---- (e) Character features & consolidated architectural style ----
    let character_features: Vec<String> = factors
        .features
        .as_ref()
        .map(|f| f.value.iter().map(|feat| feat.key().to_string()).collect())
        .unwrap_or_default();

    let style_evidence_backed = factors.style.is_evidence_backed();
    let (style_family, style_label) = if let Some(fam) = factors.style.top_family() {
        (Some(fam.key().to_string()), Some(fam.label().en.to_string()))
    } else if let Some(fam) = factors.style.likely_family() {
        (
            Some(fam.key().to_string()),
            Some(format!("Likely {} (era/form estimate)", fam.label().en)),
        )
    } else {
        (None, None)
    };
    let style_basis = factors.style.basis.map(|b| match b {
        StyleSource::Mls => "mls".to_string(),
        StyleSource::CountyRecord => "county_record".to_string(),
        StyleSource::ListingText => "listing_text".to_string(),
        StyleSource::EraPrior => "era_prior".to_string(),
    });
    let style_evidence: Vec<String> = factors
        .style
        .clues
        .iter()
        .map(|c| {
            let src = match c.source {
                StyleSource::Mls => "MLS ArchitecturalStyle",
                StyleSource::CountyRecord => "County record",
                StyleSource::ListingText => "Listing remarks/features",
                StyleSource::EraPrior => "Build era & form",
            };
            format!("{src}: {}", c.raw)
        })
        .collect();
    let style_probabilities: BTreeMap<String, f64> = factors
        .style
        .probabilities
        .iter()
        .map(|(f, p)| (f.key().to_string(), (*p * 1000.0).round() / 1000.0))
        .collect();

    if let Some(y) = effective_year {
        if factors.home.effective_year.is_some() {
            source_notes.push(format!("Condition uses assessor effective year {y}."));
        } else {
            source_notes.push(format!("Condition uses year built {y}."));
        }
    }
    if !permit_cats.is_empty() {
        let list: Vec<&str> = permit_cats.iter().copied().collect();
        source_notes.push(format!("Building permits noted: {}.", list.join(", ")));
    }
    if let Some(sh) = sun_hours {
        source_notes.push(format!("Solar irradiance: {sh:.2} kWh/m2/day (NASA POWER)."));
    }
    if let Some(dir) = facing_compass {
        source_notes.push(format!("Building orientation facing {dir} (OSM footprint & street)."));
    }
    if has_battery_backup {
        source_notes.push("Home battery storage hardware detected in property/permit records.".to_string());
    }
    source_notes.push(format!(
        "Objective Home Quality sub-score; strictly separate from {brand_name} Score."
    ));

    let multigenerational_signals = if factors.home.stories.is_some()
        || has_main_level_primary
        || factors.home.main_level_bedrooms.is_some()
        || factors.home.main_level_bathrooms.is_some()
        || has_adu
        || strong_adu
        || parking_spaces.is_some()
    {
        Some(MultigenerationalSignals {
            single_story,
            has_main_level_primary,
            main_level_bedrooms: factors.home.main_level_bedrooms,
            main_level_bathrooms: factors.home.main_level_bathrooms,
            has_existing_adu: has_adu,
            strong_adu_potential: strong_adu,
            parking_spaces,
        })
    } else {
        None
    };

    HomeQualityScore {
        score,
        band,
        condition_score,
        systems_score,
        functionality_score,
        style_family,
        style_label,
        character_features,
        source_notes,
        layout_score,
        light_energy_score,
        lot_utility_score,
        style_basis,
        style_evidence_backed,
        style_evidence,
        style_probabilities,
        multigenerational_signals,
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory Buyer Tour Photo Analysis (0 bytes stored anywhere)
// ---------------------------------------------------------------------------------------------

struct InspectedPhoto {
    filename: String,
    mime_type: String,
    caption: Option<String>,
    detail: BuyerPhotoDetail,
    clean_base64: Option<String>,
}

fn normalize_mime(raw: &str) -> Option<&'static str> {
    let base = raw.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    ALLOWED_PHOTO_MIMES.into_iter().find(|&m| m == base)
}

fn parse_data_uri<'a>(raw: &'a str) -> Result<(Option<&'static str>, &'a str)> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("data:") {
        let Some((header, payload)) = rest.split_once(',') else {
            return Err(anyhow!("Invalid data URI in buyer photo payload"));
        };
        let header_lower = header.to_ascii_lowercase();
        if !header_lower.contains(";base64") {
            return Err(anyhow!("Buyer photo data URI must be base64-encoded"));
        }
        let mime_part = header.split(';').next().unwrap_or("").trim();
        let uri_mime = if mime_part.is_empty() {
            None
        } else {
            Some(
                normalize_mime(mime_part)
                    .ok_or_else(|| anyhow!("Unsupported image MIME type in data URI: {mime_part}"))?,
            )
        };
        Ok((uri_mime, payload.trim()))
    } else {
        Ok((None, trimmed))
    }
}

fn inspect_image_headers(bytes: &[u8]) -> (Option<u32>, Option<u32>, bool) {
    // PNG: 8-byte signature + IHDR chunk
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return (Some(w), Some(h), false);
    }

    // GIF: GIF87a / GIF89a
    if bytes.len() >= 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        let w = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
        let h = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
        return (Some(w), Some(h), false);
    }

    // JPEG: SOI (0xFF 0xD8), scan APP1 (Exif) and SOFn markers
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2usize;
        let mut has_exif = false;
        let mut dims: Option<(u32, u32)> = None;
        while i + 1 < bytes.len() {
            while i < bytes.len() && bytes[i] != 0xFF {
                i += 1;
            }
            while i < bytes.len() && bytes[i] == 0xFF {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let marker = bytes[i];
            i += 1;
            if marker == 0xD9 || marker == 0xDA {
                break;
            }
            if marker == 0x00 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                continue;
            }
            if i + 2 > bytes.len() {
                break;
            }
            let seg_len = usize::from(u16::from_be_bytes([bytes[i], bytes[i + 1]]));
            if seg_len < 2 || i + seg_len > bytes.len() {
                break;
            }
            if marker == 0xE1 && seg_len >= 8 && &bytes[i + 2..i + 8] == b"Exif\0\0" {
                has_exif = true;
            }
            if matches!(
                marker,
                0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
            ) && seg_len >= 7
                && dims.is_none()
            {
                let h = u32::from(u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]));
                let w = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
                dims = Some((w, h));
            }
            i += seg_len;
        }
        return (dims.map(|d| d.0), dims.map(|d| d.1), has_exif);
    }

    // WebP: RIFF....WEBP
    if bytes.len() >= 30 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let chunk = &bytes[12..16];
        if chunk == b"VP8X" && bytes.len() >= 30 {
            let w = 1 + (u32::from(bytes[24]) | (u32::from(bytes[25]) << 8) | (u32::from(bytes[26]) << 16));
            let h = 1 + (u32::from(bytes[27]) | (u32::from(bytes[28]) << 8) | (u32::from(bytes[29]) << 16));
            return (Some(w), Some(h), false);
        }
        if chunk == b"VP8L" && bytes.len() >= 25 && bytes[20] == 0x2F {
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            return (Some(w), Some(h), false);
        }
    }

    (None, None, false)
}

fn validate_and_inspect_photos(
    photos: &[BuyerPhotoInput],
    retain_b64_for_vision: bool,
) -> Result<Vec<InspectedPhoto>> {
    if photos.len() > MAX_BUYER_PHOTOS {
        return Err(anyhow!(
            "Too many buyer photos: maximum {MAX_BUYER_PHOTOS} allowed, got {}",
            photos.len()
        ));
    }

    let mut out = Vec::with_capacity(photos.len());
    for (idx, p) in photos.iter().enumerate() {
        let declared_mime = normalize_mime(&p.mime_type)
            .ok_or_else(|| anyhow!("Unsupported MIME type '{}'", p.mime_type))?;
        let (uri_mime, payload) = parse_data_uri(&p.data_base64)?;
        if let Some(um) = uri_mime {
            if um != declared_mime {
                return Err(anyhow!(
                    "Photo '{}' data URI MIME ({um}) does not match declared mimeType ({declared_mime})",
                    p.filename
                ));
            }
        }
        let clean_b64: String = payload.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        if clean_b64.is_empty() {
            return Err(anyhow!("Photo '{}' payload is empty", p.filename));
        }
        let approx_bytes = (clean_b64.len() / 4) * 3;
        if approx_bytes > MAX_BUYER_PHOTO_BYTES + 3 {
            return Err(anyhow!(
                "Photo '{}' exceeds maximum 10 MB decoded size",
                p.filename
            ));
        }

        let decoded = STANDARD
            .decode(&clean_b64)
            .map_err(|e| anyhow!("Invalid base64 in buyer photo '{}': {e}", p.filename))?;
        let byte_len = decoded.len();
        if byte_len == 0 {
            return Err(anyhow!("Photo '{}' decoded to 0 bytes", p.filename));
        }
        if byte_len > MAX_BUYER_PHOTO_BYTES {
            drop(decoded);
            return Err(anyhow!(
                "Photo '{}' exceeds maximum 10 MB decoded size ({byte_len} bytes)",
                p.filename
            ));
        }

        let (width, height, has_exif) = inspect_image_headers(&decoded);
        // Drop raw image bytes immediately after in-memory header inspection.
        drop(decoded);

        let safe_name = if p.filename.trim().is_empty() {
            format!("photo-{}.jpg", idx + 1)
        } else {
            p.filename.trim().chars().take(120).collect()
        };

        out.push(InspectedPhoto {
            filename: safe_name.clone(),
            mime_type: declared_mime.to_string(),
            caption: p.caption.clone(),
            detail: BuyerPhotoDetail {
                filename: safe_name,
                mime_type: declared_mime.to_string(),
                byte_len,
                width,
                height,
                has_exif,
            },
            clean_base64: if retain_b64_for_vision {
                Some(clean_b64)
            } else {
                None
            },
        });
    }
    Ok(out)
}

fn feature_keywords() -> &'static [(Feature, Regex)] {
    static PATTERNS: Lazy<Vec<(Feature, Regex)>> = Lazy::new(|| {
        let f = |feature, pat: &str| (feature, Regex::new(&format!("(?i){pat}")).expect("valid regex"));
        vec![
            f(Feature::OpenPlan, r"\bopen[- ](?:floor[- ]?)?plan\b|\bopen[- ]?concept\b|\bgreat room\b"),
            f(Feature::HighCeilings, r"\bhigh ceilings?\b|\bvaulted\b|\bcathedral ceilings?\b|\bbeamed ceilings?\b"),
            f(Feature::NaturalLight, r"\bskylights?\b|\bnatural light\b|\blight[- ]filled\b|\bsunroom\b|\bclerestory\b"),
            f(Feature::Hardwood, r"\bhardwood\b|\boak floors?\b|\bwood flooring\b"),
            f(Feature::Fireplace, r"\bfireplaces?\b|\bhearth\b"),
            f(Feature::Pool, r"\b(?:swimming )?pool\b"),
            f(Feature::View, r"\b(?:panoramic|mountain|ocean|bay|city|hill|canyon|valley|lake) views?\b"),
            f(Feature::MainLevelPrimary, r"\bmain[- ]level primary\b|\bground[- ]floor (?:primary|master|bedroom)\b|\bdownstairs (?:primary|master)\b"),
            f(Feature::Office, r"\b(?:home )?office\b|\bstudy\b|\bden\b"),
            f(Feature::WorkshopGarage, r"\bworkshop\b|\bworkbench\b"),
            f(Feature::EvReady, r"\bev[- ]?charg(?:er|ing)\b|\bnema 14-50\b|\blevel[- ]2\b"),
        ]
    });
    &PATTERNS
}

fn normalize_style_key(raw: &str) -> Option<String> {
    if let Some(fam) = StyleFamily::from_key(&raw.trim().to_ascii_lowercase()) {
        return Some(fam.key().to_string());
    }
    style_families_for_value(raw)
        .into_iter()
        .next()
        .map(|f| f.key().to_string())
}

fn normalize_feature_key(raw: &str) -> Option<String> {
    let k = raw.trim().to_ascii_lowercase();
    if let Some(feat) = Feature::ALL.into_iter().find(|f| f.key() == k) {
        return Some(feat.key().to_string());
    }
    for (feat, re) in feature_keywords() {
        if re.is_match(raw) {
            return Some(feat.key().to_string());
        }
    }
    None
}

fn deterministic_offline_analysis(
    inspected: &[InspectedPhoto],
    model_used: &str,
) -> BuyerPhotoAnalysis {
    let mut styles = BTreeSet::new();
    let mut features = BTreeSet::new();
    let mut condition_notes = Vec::new();
    let mut updated_hits = 0usize;
    let mut dated_hits = 0usize;

    for item in inspected {
        let name_words = item.filename.replace(['_', '-', '.'], " ");
        let mut text = name_words;
        if let Some(ref cap) = item.caption {
            text.push(' ');
            text.push_str(cap);
        }

        // Style clues from caption & filename
        for tok in text.split([',', ';', '/', '|']) {
            for fam in style_families_for_value(tok.trim()) {
                styles.insert(fam.key().to_string());
            }
        }
        let low = text.to_ascii_lowercase();
        for fam in StyleFamily::ALL {
            let key_spaced = fam.key().replace('_', " ");
            if low.contains(&key_spaced) || low.contains(fam.key()) {
                styles.insert(fam.key().to_string());
            }
        }
        if low.contains("craftsman") || low.contains("bungalow") {
            styles.insert(StyleFamily::Craftsman.key().to_string());
        }
        if low.contains("mid century") || low.contains("midcentury") || low.contains("eichler") {
            styles.insert(StyleFamily::MidCentury.key().to_string());
        }
        if low.contains("spanish") || low.contains("mediterranean") {
            styles.insert(StyleFamily::Mediterranean.key().to_string());
        }
        if low.contains("victorian") {
            styles.insert(StyleFamily::Victorian.key().to_string());
        }
        if low.contains("ranch") {
            styles.insert(StyleFamily::Ranch.key().to_string());
        }
        if low.contains("tudor") {
            styles.insert(StyleFamily::Tudor.key().to_string());
        }
        if low.contains("farmhouse") && !low.contains("farmhouse sink") {
            styles.insert(StyleFamily::Farmhouse.key().to_string());
        }
        if low.contains("colonial") {
            styles.insert(StyleFamily::Colonial.key().to_string());
        }
        if low.contains("contemporary")
            || (low.contains("modern")
                && !low.contains("mid century")
                && !low.contains("midcentury"))
        {
            styles.insert(StyleFamily::Modern.key().to_string());
        }

        // Character features from caption & filename
        for (feat, re) in feature_keywords() {
            if re.is_match(&text) {
                features.insert(feat.key().to_string());
            }
        }

        // Building finish/condition notes (strictly building-only; word-bounded so "updated" != "dated")
        static UPDATED_FINISH_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"(?i)\b(?:quartz|granite|stainless|remodeled|updated|renovated)\b")
                .expect("valid regex")
        });
        static DATED_FINISH_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r"(?i)\b(?:dated|outdated|original|fixer|wear|cracks?)\b")
                .expect("valid regex")
        });

        let mut obs = Vec::new();
        if UPDATED_FINISH_RE.is_match(&text) {
            updated_hits += 1;
            obs.push("updated kitchen/bath or interior finish noted");
        }
        if DATED_FINISH_RE.is_match(&text) {
            dated_hits += 1;
            obs.push("older or original building finish noted");
        }
        if low.contains("roof") {
            obs.push("roof exterior inspected");
        }
        if low.contains("panel") || low.contains("hvac") || low.contains("furnace") {
            obs.push("mechanical/electrical equipment inspected");
        }

        let dims_str = match (item.detail.width, item.detail.height) {
            (Some(w), Some(h)) => format!("{w}x{h}px"),
            _ => "unparsed dimensions".to_string(),
        };
        let exif_str = if item.detail.has_exif { "; EXIF ignored" } else { "" };
        let obs_str = if obs.is_empty() {
            "inspected in memory for building style & finish clues".to_string()
        } else {
            obs.join(", ")
        };
        condition_notes.push(format!(
            "{} ({}, {dims_str}{exif_str}): {obs_str}",
            item.filename, item.mime_type
        ));
    }

    let finish_level = if updated_hits > 0 && dated_hits == 0 {
        Some("updated_modern".to_string())
    } else if dated_hits > 0 && updated_hits == 0 {
        Some("dated_original".to_string())
    } else if updated_hits > 0 && dated_hits > 0 {
        Some("mixed_updates".to_string())
    } else {
        Some("unknown".to_string())
    };

    BuyerPhotoAnalysis {
        inferred_styles: styles.into_iter().collect(),
        character_features: features.into_iter().collect(),
        condition_notes,
        model_used: model_used.to_string(),
        stored_bytes: 0,
        analyzed_count: inspected.len(),
        notice: BUYER_PHOTO_NOTICE.to_string(),
        finish_level,
        photo_details: inspected.iter().map(|i| i.detail.clone()).collect(),
    }
}

fn extract_responses_text(resp_json: &Value) -> std::result::Result<String, String> {
    if let Some(ot) = resp_json.get("output_text").and_then(Value::as_str) {
        if !ot.trim().is_empty() {
            return Ok(ot.to_string());
        }
    }
    let output_arr = resp_json
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing output array in Responses JSON".to_string())?;
    let mut parts = Vec::new();
    for item in output_arr {
        if let Some(content_arr) = item.get("content").and_then(Value::as_array) {
            for c in content_arr {
                let ctype = c.get("type").and_then(Value::as_str).unwrap_or("");
                if ctype == "refusal" {
                    let msg = c.get("refusal").and_then(Value::as_str).unwrap_or("refused");
                    return Err(format!("model refused: {msg}"));
                }
                if (ctype == "output_text" || ctype == "text" || ctype.is_empty())
                    && c.get("text").and_then(Value::as_str).is_some()
                {
                    if let Some(txt) = c.get("text").and_then(Value::as_str) {
                        parts.push(txt);
                    }
                }
            }
        }
    }
    let joined = parts.join("");
    if joined.trim().is_empty() {
        Err("empty response text".to_string())
    } else {
        Ok(joined)
    }
}

async fn call_vision_photo_endpoint(
    http: &HttpClient,
    env: &EnvConfig,
    inspected: &[InspectedPhoto],
) -> Result<BuyerPhotoAnalysis> {
    let base_url = env
        .llm_base_url()
        .ok_or_else(|| anyhow!("missing OPENAI_BASE_URL"))?;
    let api_key = env
        .llm_api_key()
        .ok_or_else(|| anyhow!("missing OPENAI_API_KEY"))?;
    let endpoint = format!("{}/responses", base_url.trim_end_matches('/'));

    let instructions = format!(
        "{IMMUTABLE_SYSTEM_PREAMBLE}\n\n\
         You inspect buyer-uploaded residential tour photos strictly for building architecture, kitchen/bath finish quality, and physical home character features.\n\
         STRICT BUILDING-ONLY & FAIR HOUSING RULE:\n\
         - Describe ONLY the building, structure, exterior, rooms, fixtures, finishes, and yard.\n\
         - NEVER mention, infer, or describe any people, faces, occupants, demographics, race, ethnicity, age, gender, religion, or personal/family traits."
    );

    let mut content_items: Vec<Value> = Vec::with_capacity(inspected.len() + 1);
    let mut captions_summary = Vec::new();
    for (idx, p) in inspected.iter().enumerate() {
        let Some(ref b64) = p.clean_base64 else {
            return Err(anyhow!("missing transient photo payload for vision request"));
        };
        content_items.push(json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{b64}", p.mime_type),
        }));
        if let Some(ref cap) = p.caption {
            if !cap.trim().is_empty() {
                captions_summary.push(format!("Photo {} ({}): {}", idx + 1, p.filename, cap.trim()));
            }
        }
    }
    let prompt = format!(
        "Analyze these {} residential building tour photo(s). Identify:\n\
         1) inferredStyles: architectural style family keys from [modern, mid_century, ranch, split_level, craftsman, farmhouse, barn, colonial, cape_cod, victorian, tudor, mediterranean, southwestern, cabin, traditional, other_distinctive].\n\
         2) characterFeatures: keys from [open_plan, high_ceilings, natural_light, hardwood, fireplace, pool, view, main_level_primary, office, workshop_garage, ev_ready].\n\
         3) finishLevel: one of [updated_modern, well_maintained, mixed_updates, dated_original, needs_renovation, unknown].\n\
         4) conditionNotes: brief building-only notes on architecture, kitchen/bath finishes, or visible maintenance items (zero people descriptions).\n{}",
        inspected.len(),
        if captions_summary.is_empty() {
            String::new()
        } else {
            format!("Buyer captions:\n{}", captions_summary.join("\n"))
        }
    );
    content_items.push(json!({ "type": "input_text", "text": prompt }));

    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["inferredStyles", "characterFeatures", "finishLevel", "conditionNotes"],
        "properties": {
            "inferredStyles": { "type": "array", "items": { "type": "string" } },
            "characterFeatures": { "type": "array", "items": { "type": "string" } },
            "finishLevel": { "type": "string" },
            "conditionNotes": { "type": "array", "items": { "type": "string" } }
        }
    });

    let models = resolve_models(env, &[]);
    let mut errors = Vec::new();
    for spec in &models {
        let mut body = json!({
            "model": spec.model,
            "instructions": instructions,
            "input": [{ "role": "user", "content": content_items }],
            "stream": false,
            "store": false,
            "max_output_tokens": 1200,
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "buyer_photo_building_analysis",
                    "strict": true,
                    "schema": schema
                }
            }
        });
        if let Some(ref eff) = spec.effort {
            body["reasoning"] = json!({ "effort": eff });
        }

        let resp = match http
            .raw()
            .post(&endpoint)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(35_000))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                errors.push(scrub_sensitive_text(&format!("{}: network error {e}", spec.model)));
                continue;
            }
        };

        let status = resp.status();
        let raw_text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                errors.push(scrub_sensitive_text(&format!("{}: read error {e}", spec.model)));
                continue;
            }
        };
        if !status.is_success() {
            errors.push(scrub_sensitive_text(&format!("{}: HTTP {}", spec.model, status.as_u16())));
            continue;
        }
        let parsed_resp: Value = match serde_json::from_str(&raw_text) {
            Ok(v) => v,
            Err(_) => {
                errors.push(format!("{}: non-JSON upstream response", spec.model));
                continue;
            }
        };
        let text = match extract_responses_text(&parsed_resp) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{}: {e}", spec.model));
                continue;
            }
        };
        let data = match parse_json_loose(&text) {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("{}: {}", spec.model, e.message));
                continue;
            }
        };

        // Check guardrail on all strings
        let mut guardrail_hit = false;
        let mut strs = Vec::new();
        collect_strings(&data, &mut strs);
        for s in strs {
            if inspect_output_guardrail(s).is_some() {
                guardrail_hit = true;
                break;
            }
        }
        if guardrail_hit {
            errors.push(format!("{}: output guardrail triggered", spec.model));
            continue;
        }

        let styles: BTreeSet<String> = data
            .get("inferredStyles")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter_map(normalize_style_key)
                    .collect()
            })
            .unwrap_or_default();
        let features: BTreeSet<String> = data
            .get("characterFeatures")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter_map(normalize_feature_key)
                    .collect()
            })
            .unwrap_or_default();
        let notes: Vec<String> = data
            .get("conditionNotes")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let finish_level = data
            .get("finishLevel")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        return Ok(BuyerPhotoAnalysis {
            inferred_styles: styles.into_iter().collect(),
            character_features: features.into_iter().collect(),
            condition_notes: notes,
            model_used: spec.model.clone(),
            stored_bytes: 0,
            analyzed_count: inspected.len(),
            notice: BUYER_PHOTO_NOTICE.to_string(),
            finish_level,
            photo_details: inspected.iter().map(|i| i.detail.clone()).collect(),
        });
    }

    Err(anyhow!("Vision endpoints unavailable: {}", errors.join(" | ")))
}

/// Analyzes buyer-uploaded tour photos strictly in memory, returning style/character clues
/// without persisting any image bytes (`stored_bytes` is always 0).
pub async fn analyze_buyer_photos(
    http: &HttpClient,
    env: &EnvConfig,
    photos: &[BuyerPhotoInput],
) -> Result<BuyerPhotoAnalysis> {
    let use_vision = env.llm_configured();
    let inspected = validate_and_inspect_photos(photos, use_vision)?;
    if inspected.is_empty() {
        return Ok(BuyerPhotoAnalysis {
            analyzed_count: 0,
            stored_bytes: 0,
            model_used: "none".to_string(),
            notice: BUYER_PHOTO_NOTICE.to_string(),
            ..Default::default()
        });
    }

    if use_vision {
        match call_vision_photo_endpoint(http, env, &inspected).await {
            Ok(res) => return Ok(res),
            Err(_) => {
                // Graceful fallback to deterministic offline in-memory inspection
                return Ok(deterministic_offline_analysis(
                    &inspected,
                    "deterministic_offline_fallback",
                ));
            }
        }
    }

    Ok(deterministic_offline_analysis(
        &inspected,
        "deterministic_offline",
    ))
}
