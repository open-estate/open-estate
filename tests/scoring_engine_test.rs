//! "Your Match" engine: plan section 5.12 table on synthetic reports, and engine invariants
//! verified across the synthetic multi-state cohort (`cargo test --test scoring_engine_test`).

mod common;

use open_estate::scoring::catalog::{
    Dimension, Feature, Lang, MatchBand, PropertyKind, Screen, StyleFamily, StylePreference,
};
use open_estate::scoring::engine::{
    rank, rank_with, CommuteMatrix, DropReason, GateKind, GateReason, NoticeCode, RankOptions,
    RankResult, ScoredHome, CRITICAL_CAP,
};
use open_estate::scoring::factors::{
    estimate_listing_monthly_ownership, Factor, Grade, HomeFactors, SchoolMetric, SchoolScore,
    StyleSource,
};
use open_estate::scoring::presets::{preset, PRESETS};
use open_estate::scoring::profile::{Place, Profile, TravelMode};
use open_estate::scoring::utility::EstimateReason;
use open_estate::shared::slugify;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

const A: &str = common::SYNTH_SECONDARY_SLUG;
const B: &str = common::SYNTH_PRIMARY_SLUG;
/// Primary workplace destination coordinates for commute scoring.
const PRIMARY_HUB: (f64, f64) = (37.3349, -122.0090);
/// Secondary transit destination coordinates for multi-place commute invariants.
const TRANSIT_HUB: (f64, f64) = (37.3785, -122.0307);
/// Free-flow OSRM drive times from the prototype's saved table call (814.2 s and 1134.4 s), i.e.
/// 13.6 and 18.9 minutes.
const A_MINUTES: f64 = 814.2 / 60.0;
const B_MINUTES: f64 = 1134.4 / 60.0;

fn synth_scoring_report(slug: &str) -> Value {
    let mut r = common::synth_report_for_slug(slug);
    r["meta"]["generatedAt"] = json!("2026-11-10T00:00:00Z");
    if let Some(fobj) = r["facts"].as_object_mut() {
        fobj.remove("garageSpaces");
        fobj.remove("parkingSpaces");
        fobj.remove("architecturalStyle");
        fobj.remove("architectureType");
        fobj.remove("mlsListing");
        fobj.remove("effectiveYearBuilt");
        fobj.remove("effectiveYear");
    }

    if slug == A {
        r["facts"]["propertyType"] = json!("Single-Family Detached");
        r["facts"]["beds"] = json!(4);
        r["facts"]["baths"] = json!(2.0);
        r["facts"]["sqft"] = json!(1990);
        r["facts"]["lotSqft"] = json!(10018);
        r["facts"]["stories"] = json!(1);
        r["facts"]["yearBuilt"] = json!(1925);
        r["facts"]["hoaMonthly"] = json!(0);
        r["facts"]["renovations"] =
            json!("Remodeled 1925 Craftsman home; county assessor effective year built is 1960.");
        r["facts"]["features"] = json!([
            "Permitted 584 sq ft detached 1bd/1ba ADU",
            "Hardwood floors",
            "Wood-burning fireplace",
            "High ceilings",
            "Above-ground spa"
        ]);
        r["facts"]["schools"] = json!({
            "assigned": [
                { "name": "Synth Beta Elementary", "grades": "K-5", "distanceMi": 0.42 },
                { "name": "Synth Beta Middle", "grades": "6-8", "distanceMi": 0.85 },
                { "name": "Synth Beta High", "grades": "9-12", "distanceMi": 1.15 }
            ]
        });
        r["decisionFactors"]["lotAndAdu"] =
            json!({ "feasibility": "Strong (CA State ADU Eligible)" });
        r["decisionFactors"]["safety"] = Value::Null;
        r["summary"]["scores"]["investment"] = json!(54.0);
        r["modules"]["valuation"]["data"] = json!({
            "estimate": { "value": 2_378_000.0, "asOf": "2026-11-10" },
            "ownership": {
                "price": 2_378_000.0,
                "downPaymentPct": 20.0,
                "rate30": 6.5,
                "rateDate": "2026-11-10",
                "monthly": {
                    "total": 16_496.0,
                    "principalInterest": 12_028.0,
                    "hoa": 0.0
                }
            }
        });
        r["modules"]["hazards"]["data"] = json!({
            "scores": { "overall": 53.0 },
            "flood": { "zone": "X", "sfha": false, "firmEffective": "2009-05-18" },
            "wildfire": {
                "california": { "fhsz": "Not in a Fire Hazard Severity Zone", "fhszClass": 0 }
            }
        });
        r["modules"]["environment"]["data"] = json!({ "scores": { "overall": 60.0 } });
        r["modules"]["fengshui"]["data"] = json!({
            "scores": { "overall": 52.0 },
            "form": [
                { "key": "lu-chong", "en": "A neighborhood street points straight at the house" }
            ]
        });
        r["modules"]["amenities"]["data"] = json!({
            "accessScore": 64.0,
            "proximity": { "freeway": { "distM": 1430.0 } }
        });
        r["modules"]["civic"]["data"] = json!({
            "scores": { "walkability": 32.0 },
            "schools": {
                "public": {
                    "list": [
                        {
                            "name": "Synth Beta Elementary",
                            "level": "elementary",
                            "grades": "K-5",
                            "distanceMi": 0.42,
                            "inAttendanceZone": true,
                            "caDashboard": { "year": "2024-25", "elaTier": 4, "mathTier": 4 }
                        },
                        {
                            "name": "Synth Beta Middle",
                            "level": "middle",
                            "grades": "6-8",
                            "distanceMi": 0.85,
                            "inAttendanceZone": true,
                            "caDashboard": { "year": "2024-25", "elaTier": 5, "mathTier": 5 }
                        },
                        {
                            "name": "Synth Beta High",
                            "level": "high",
                            "grades": "9-12",
                            "distanceMi": 1.15,
                            "inAttendanceZone": true,
                            "caDashboard": {
                                "year": "2024-25",
                                "elaTier": 4,
                                "mathTier": 3,
                                "sciTier": 4,
                                "cciTier": 5,
                                "gradTier": 5
                            }
                        }
                    ]
                }
            }
        });
    } else {
        r["facts"]["propertyType"] = json!("Single-Family Detached");
        r["facts"]["beds"] = json!(4);
        r["facts"]["baths"] = json!(2.0);
        r["facts"]["sqft"] = json!(1956);
        r["facts"]["lotSqft"] = json!(6300);
        r["facts"]["stories"] = json!(2);
        r["facts"]["yearBuilt"] = json!(1975);
        r["facts"]["hoaMonthly"] = json!(0);
        r["facts"]["renovations"] = json!("");
        r["facts"]["features"] = json!([
            "Abundant natural light and skylights",
            "Primary bedroom on the main floor"
        ]);
        r["facts"]["schools"] = json!({
            "assigned": [
                { "name": "Synth Alpha Elementary", "grades": "K-5", "distanceMi": 0.35 },
                { "name": "Synth Alpha Middle", "grades": "6-8", "distanceMi": 0.78 },
                { "name": "Synth Alpha High", "grades": "9-12", "distanceMi": 1.20 }
            ]
        });
        r["decisionFactors"]["lotAndAdu"] =
            json!({ "feasibility": "Strong (CA State ADU Eligible)" });
        r["decisionFactors"]["safety"] = json!({
            "agencySafetyScore": 48.0,
            "agencyName": "Metro Area Police Department",
            "year": 2025
        });
        r["summary"]["scores"]["investment"] = json!(59.0);
        r["modules"]["valuation"]["data"] = json!({
            "estimate": { "value": 2_114_000.0, "asOf": "2026-11-10" },
            "ownership": {
                "price": 2_114_000.0,
                "downPaymentPct": 20.0,
                "rate30": 6.5,
                "rateDate": "2026-11-10",
                "monthly": {
                    "total": 14_847.0,
                    "principalInterest": 10_693.0,
                    "hoa": 0.0
                }
            }
        });
        r["modules"]["hazards"]["data"] = json!({
            "scores": { "overall": 59.0 },
            "flood": { "zone": "X", "sfha": false, "firmEffective": "2009-05-18" },
            "wildfire": {
                "california": { "fhsz": "Not in a Fire Hazard Severity Zone", "fhszClass": 0 }
            }
        });
        r["modules"]["environment"]["data"] = json!({ "scores": { "overall": 88.0 } });
        r["modules"]["fengshui"]["data"] = json!({
            "scores": { "overall": 67.0 },
            "tJunction": { "hasAlignment": false, "hits": [] }
        });
        r["modules"]["amenities"]["data"] = json!({
            "accessScore": 80.0,
            "proximity": { "freeway": { "distM": 655.0 } }
        });
        r["modules"]["civic"]["data"] = json!({
            "scores": { "walkability": 75.0 },
            "schools": {
                "public": {
                    "list": [
                        {
                            "name": "Synth Alpha Elementary",
                            "level": "elementary",
                            "grades": "K-5",
                            "distanceMi": 0.35,
                            "inAttendanceZone": true,
                            "caDashboard": { "year": "2024-25", "elaTier": 5, "mathTier": 5, "sciTier": 5 }
                        },
                        {
                            "name": "Synth Alpha Middle",
                            "level": "middle",
                            "grades": "6-8",
                            "distanceMi": 0.78,
                            "inAttendanceZone": true,
                            "caDashboard": { "year": "2024-25", "elaTier": 5, "mathTier": 4, "sciTier": 4 }
                        },
                        {
                            "name": "Synth Alpha High",
                            "level": "high",
                            "grades": "9-12",
                            "distanceMi": 1.20,
                            "inAttendanceZone": true,
                            "caDashboard": {
                                "year": "2024-25",
                                "elaTier": 5,
                                "mathTier": 4,
                                "sciTier": 4,
                                "cciTier": 4,
                                "gradTier": 5
                            }
                        }
                    ]
                }
            }
        });
    }
    r
}

fn load(slug: &str) -> HomeFactors {
    HomeFactors::from_report(&synth_scoring_report(slug))
}

fn place(label: &str, max_minutes: f64, trips_per_week: f64) -> Place {
    Place {
        label: label.to_string(),
        lat: PRIMARY_HUB.0,
        lon: PRIMARY_HUB.1,
        max_minutes,
        trips_per_week,
        mode: TravelMode::Drive,
    }
}

fn place_at(label: &str, coords: (f64, f64), max_minutes: f64, trips_per_week: f64) -> Place {
    Place {
        label: label.to_string(),
        lat: coords.0,
        lon: coords.1,
        max_minutes,
        trips_per_week,
        mode: TravelMode::Drive,
    }
}

// ---------------------------------------------------------------------------------------------
// Plan section 5.12
// ---------------------------------------------------------------------------------------------

/// `PERSONAS` in match_prototype_v3.py.
struct Persona {
    name: &'static str,
    preset: &'static str,
    monthly: f64,
    baths: f64,
    sqft: f64,
    lot: f64,
    commute_max: f64,
    multigen: bool,
    loves_craftsman: bool,
    avoid_t_junction: bool,
    /// Plan 5.12 table: score A, score B, coverage A %, coverage B %.
    table: (f64, f64, f64, f64),
    /// Prototype output at full precision: score A, score B.
    prototype: (f64, f64),
}

fn personas() -> Vec<Persona> {
    let base = |name, preset, table, prototype| Persona {
        name,
        preset,
        monthly: 16_000.0,
        baths: 2.0,
        sqft: 1900.0,
        lot: 6000.0,
        commute_max: 30.0,
        multigen: false,
        loves_craftsman: false,
        avoid_t_junction: false,
        table,
        prototype,
    };
    let multi = |p: Persona| Persona {
        multigen: true,
        monthly: 17_000.0,
        baths: 3.0,
        sqft: 2400.0,
        lot: 7000.0,
        ..p
    };
    vec![
        base(
            "Balanced",
            "balanced",
            (67.0, 73.9, 91.0, 100.0),
            (67.0464, 73.9222),
        ),
        base(
            "Education-first",
            "education_first",
            (71.5, 77.0, 91.0, 100.0),
            (71.5256, 76.9915),
        ),
        Persona {
            commute_max: 25.0,
            ..base(
                "Commute-first",
                "commute_first",
                (67.4, 70.0, 94.0, 100.0),
                (67.3526, 69.9622),
            )
        },
        multi(base(
            "Multigenerational household",
            "multigenerational",
            (70.9, 72.6, 92.0, 100.0),
            (70.9338, 72.5653),
        )),
        base(
            "Quiet and healthy",
            "quiet_healthy",
            (62.6, 73.4, 90.0, 100.0),
            (62.6458, 73.4174),
        ),
        Persona {
            avoid_t_junction: true,
            ..base(
                "Feng-shui-conscious",
                "feng_shui",
                (65.2, 73.7, 93.0, 100.0),
                (65.2207, 73.6636),
            )
        },
        Persona {
            loves_craftsman: true,
            ..base(
                "Design lover who loves Craftsman",
                "design_lover",
                (72.8, 71.1, 94.0, 76.0),
                (72.8315, 71.1028),
            )
        },
        Persona {
            loves_craftsman: true,
            ..multi(base(
                "Multigenerational who loves Craftsman",
                "multigenerational",
                (71.7, 72.1, 92.0, 96.0),
                (71.6965, 72.0626),
            ))
        },
    ]
}

fn persona_profile(p: &Persona) -> Profile {
    let mut profile = preset(p.preset).expect("preset").to_profile();
    profile.budget.monthly = Some(p.monthly);
    profile.min_beds = Some(4.0);
    profile.min_baths = Some(p.baths);
    profile.min_sqft = Some(p.sqft);
    profile.min_lot_sqft = Some(p.lot);
    profile.places = vec![place("Primary Hub", p.commute_max, 5.0)];
    profile.multigenerational = p.multigen;
    if p.loves_craftsman {
        profile
            .style_preferences
            .insert(StyleFamily::Craftsman, StylePreference::Love);
    }
    if p.avoid_t_junction {
        profile.deal_breakers.push(Screen::TJunction);
    }
    profile
}

fn fixture_set() -> (Vec<HomeFactors>, CommuteMatrix) {
    let homes = vec![load(A), load(B)];
    let mut commute = CommuteMatrix::new();
    commute.set(A, 0, A_MINUTES);
    commute.set(B, 0, B_MINUTES);
    (homes, commute)
}

#[test]
fn reproduces_plan_section_5_12() {
    let (homes, commute) = fixture_set();
    println!(
        "{:<40} {:>6} {:>6} {:>6} {:>6}  flagged",
        "persona", "A", "B", "cov A", "cov B"
    );
    for p in personas() {
        let profile = persona_profile(&p);
        profile
            .validate()
            .unwrap_or_else(|e| panic!("{}: {e:?}", p.name));
        let r = rank(&profile, &homes, &commute);
        let a = r.find(A).expect("A scored");
        let b = r.find(B).expect("B scored");
        println!(
            "{:<40} {:>6.1} {:>6.1} {:>5.0}% {:>5.0}%  {}",
            p.name,
            a.score,
            b.score,
            a.coverage * 100.0,
            b.coverage * 100.0,
            a.flagged
        );
        let (ta, tb, ca, cb) = p.table;
        assert!(
            (a.score - ta).abs() <= 0.5,
            "{}: A {} vs table {ta}",
            p.name,
            a.score
        );
        assert!(
            (b.score - tb).abs() <= 0.5,
            "{}: B {} vs table {tb}",
            p.name,
            b.score
        );
        assert!(
            (a.coverage * 100.0 - ca).abs() <= 0.5,
            "{}: A coverage {}",
            p.name,
            a.coverage
        );
        assert!(
            (b.coverage * 100.0 - cb).abs() <= 0.5,
            "{}: B coverage {}",
            p.name,
            b.coverage
        );
        // Faithful port: the prototype's own numbers.
        assert!(
            (a.score - p.prototype.0).abs() < 1e-3,
            "{}: A {} vs prototype {}",
            p.name,
            a.score,
            p.prototype.0
        );
        assert!(
            (b.score - p.prototype.1).abs() < 1e-3,
            "{}: B {} vs prototype {}",
            p.name,
            b.score,
            p.prototype.1
        );

        // T-junction gate: only the feng-shui-conscious buyer flags A, and A is hidden by default.
        assert_eq!(a.flagged, p.avoid_t_junction, "{}", p.name);
        assert!(!b.flagged);
        if p.avoid_t_junction {
            assert_eq!(r.flagged.len(), 1);
            assert_eq!(r.flagged[0].id, A);
            assert!(a
                .gates
                .iter()
                .any(|g| g.reason == GateReason::TJunction && g.kind == GateKind::DealBreaker));
            assert_eq!(r.ranked.len(), 1);
        } else {
            assert_eq!(r.ranked.len(), 2);
            // Lead and ties: scores less than one point apart share a rank.
            let tied = (a.score - b.score).abs() < 1.0;
            assert_eq!(a.tied, tied, "{}", p.name);
            assert_eq!(b.tied, tied, "{}", p.name);
            assert_eq!(a.rank == b.rank, tied, "{}", p.name);
        }

        // Home A has no agency crime data: the search median (Home B, 48) is used and flagged.
        assert_eq!(a.utility(Dimension::Crime), Some(48.0));
        assert!(a
            .estimated
            .iter()
            .any(|e| e.dimension == Dimension::Crime && e.reason == EstimateReason::ImputedMedian));
        assert!(!b.is_estimated(Dimension::Crime));

        if p.loves_craftsman {
            assert!((a.utility(Dimension::Style).unwrap() - 90.0).abs() < 1e-9);
            assert!(
                (b.utility(Dimension::Style).unwrap() - 60.0).abs() < 1e-9,
                "unknown style counts as fine"
            );
            assert!(b
                .estimated
                .iter()
                .any(|e| e.dimension == Dimension::Style
                    && e.reason == EstimateReason::NoStyleEvidence));
        } else {
            assert!(r
                .dropped
                .iter()
                .any(|d| d.dimension == Dimension::Style
                    && d.reason == DropReason::NoStylePreference));
            assert_eq!(a.utility(Dimension::Style), None);
        }
    }
}

#[test]
fn style_preference_flips_the_order() {
    let (homes, commute) = fixture_set();
    let all = personas();
    let design = all
        .iter()
        .find(|p| p.name.starts_with("Design lover"))
        .unwrap();
    let r = rank(&persona_profile(design), &homes, &commute);
    assert_eq!(
        r.ranked[0].id, A,
        "a design lover who loves Craftsman puts A first"
    );
    assert!(!r.ranked[0].tied);
    let balanced = all.iter().find(|p| p.name == "Balanced").unwrap();
    let r = rank(&persona_profile(balanced), &homes, &commute);
    assert_eq!(r.ranked[0].id, B);
}

#[test]
fn utilities_match_the_prototype() {
    let (homes, commute) = fixture_set();
    let all = personas();
    let get = |name: &str| persona_profile(all.iter().find(|p| p.name == name).unwrap());
    let check = |r: &RankResult, id: &str, expected: &[(Dimension, f64)]| {
        let h = r.find(id).unwrap();
        for (d, v) in expected {
            let got = h.utility(*d).unwrap_or_else(|| panic!("{id} has no {d:?}"));
            assert!((got - v).abs() < 1e-3, "{id} {d:?}: {got} vs {v}");
        }
    };
    let r = rank(&get("Balanced"), &homes, &commute);
    use Dimension::*;
    check(
        &r,
        A,
        &[
            (Cost, 61.32),
            (Schools, 88.0),
            (Commute, 92.9573),
            (Convenience, 51.2),
            (Homefit, 82.65),
            (Hazards, 53.0),
            (Crime, 48.0),
            (Environment, 60.0),
            (Investment, 54.0),
            (Fengshui, 52.0),
        ],
    );
    check(
        &r,
        B,
        &[
            (Cost, 80.8094),
            (Schools, 91.5556),
            (Commute, 74.4569),
            (Convenience, 78.0),
            (Homefit, 78.9),
            (Hazards, 59.0),
            (Crime, 48.0),
            (Environment, 88.0),
            (Investment, 59.0),
            (Fengshui, 67.0),
        ],
    );
    let r = rank(&get("Multigenerational household"), &homes, &commute);
    check(&r, A, &[(Cost, 74.4471), (Homefit, 83.12)]);
    check(&r, B, &[(Cost, 88.9971), (Homefit, 64.3)]);
    let r = rank(&get("Commute-first"), &homes, &commute);
    check(&r, A, &[(Commute, 83.5488)]);
    check(&r, B, &[(Commute, 61.3483)]);
}

#[test]
fn explanations_use_real_data_and_render_in_three_languages() {
    let (homes, commute) = fixture_set();
    let balanced = personas()
        .into_iter()
        .find(|p| p.name == "Balanced")
        .unwrap();
    let mut r = rank(&persona_profile(&balanced), &homes, &commute);
    for h in r.all() {
        assert!(h.reasons.len() <= 3 && h.trade_offs.len() <= 2);
        assert!(h.reasons.windows(2).all(|w| w[0].impact >= w[1].impact));
        assert!(h.trade_offs.windows(2).all(|w| w[0].impact <= w[1].impact));
        assert!(h.reasons.iter().all(|e| e.impact > 0.0));
        assert!(h.trade_offs.iter().all(|e| e.impact < 0.0));
        for e in h.reasons.iter().chain(h.trade_offs.iter()) {
            assert!(
                !h.is_estimated(e.dimension),
                "estimated values never explain"
            );
        }
    }
    let b = r.find(B).unwrap();
    // B's biggest edge over the two-home median is its monthly cost (16% weight, 81 vs 71).
    assert_eq!(b.reasons[0].dimension, Dimension::Cost);
    assert_eq!(r.find(A).unwrap().trade_offs[0].dimension, Dimension::Cost);

    let order: Vec<(String, f64)> = r.ranked.iter().map(|h| (h.id.clone(), h.score)).collect();
    let mut texts = Vec::new();
    for lang in Lang::ALL {
        r.localize(lang);
        let again: Vec<(String, f64)> = r.ranked.iter().map(|h| (h.id.clone(), h.score)).collect();
        assert_eq!(again, order, "ranking is identical in every language");
        let t = r.find(B).unwrap().reasons[0]
            .text
            .clone()
            .expect("localized");
        assert!(!t.is_empty());
        texts.push(t);
        assert!(r
            .dropped
            .iter()
            .all(|d| d.text.as_deref().is_some_and(|s| !s.is_empty())));
        let a = r.find(A).unwrap();
        assert!(a.estimated.iter().all(|e| e.text.is_some()));
    }
    assert_eq!(texts.len(), 3);
    assert!(
        texts[0].starts_with("More comfortable on your budget"),
        "{}",
        texts[0]
    );
    assert!(texts[1].starts_with("更符合您的预算"), "{}", texts[1]);
    assert!(texts[2].starts_with("Más holgado"), "{}", texts[2]);
}

// ---------------------------------------------------------------------------------------------
// Multi-state synthetic cohort (primary/secondary reports + 30 holdout homes + 300 comps)
// ---------------------------------------------------------------------------------------------

fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let r_km = 6_371.0088_f64;
    let dlat = (b.0 - a.0).to_radians();
    let dlon = (b.1 - a.1).to_radians();
    let lat1 = a.0.to_radians();
    let lat2 = b.0.to_radians();
    let h = (dlat * 0.5).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon * 0.5).sin().powi(2);
    2.0 * r_km * h.clamp(0.0, 1.0).sqrt().asin()
}

/// Free-flow road-distance drive minutes between two WGS84 coordinates (1.32 road circuity, 55 km/h).
fn free_flow_drive_minutes(origin: (f64, f64), dest: (f64, f64)) -> f64 {
    let road_km = haversine_km(origin, dest) * 1.32;
    (road_km / 55.0 * 60.0).clamp(4.0, 240.0)
}

fn build_holdout_report_envelope(id: &str, entry: &Value) -> (Value, (f64, f64)) {
    let raw = entry.get("raw").unwrap_or(&Value::Null);
    let geo = raw.get("geo").cloned().unwrap_or_else(|| json!({}));
    let facts = raw.get("facts").cloned().unwrap_or_else(|| json!({}));

    let lat = geo.get("lat").and_then(Value::as_f64).unwrap_or(37.2820);
    let lon = geo.get("lon").and_then(Value::as_f64).unwrap_or(-121.9540);
    let state = geo.get("state").and_then(Value::as_str);
    let hoa = facts
        .get("hoaMonthly")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let price = entry
        .get("targetSalePrice")
        .and_then(Value::as_i64)
        .or_else(|| facts.pointer("/lastSale/price").and_then(Value::as_i64))
        .filter(|&p| p > 0);

    let mut modules = serde_json::Map::new();
    if let Some(p) = price {
        let (monthly_total, monthly_pi) = estimate_listing_monthly_ownership(p, hoa, state);
        modules.insert(
            "valuation".to_string(),
            json!({
                "data": {
                    "estimate": { "value": p as f64, "asOf": "2026-11-10" },
                    "ownership": {
                        "price": p as f64,
                        "downPaymentPct": 20.0,
                        "rate30": 6.5,
                        "rateDate": "2026-11-10",
                        "monthly": {
                            "total": monthly_total,
                            "principalInterest": monthly_pi,
                            "hoa": hoa as f64
                        }
                    }
                }
            }),
        );
    }

    let mut civic_data = serde_json::Map::new();
    if let Some(walk) = facts.pointer("/walkScore/walk").and_then(Value::as_f64) {
        if (0.0..=100.0).contains(&walk) {
            civic_data.insert("scores".to_string(), json!({ "walkability": walk }));
        }
    }
    if let Some(assigned) = facts.pointer("/schools/assigned").and_then(Value::as_array) {
        let mut public_list = Vec::new();
        for s in assigned {
            let Some(name) = s.get("name").and_then(Value::as_str) else {
                continue;
            };
            let clean_name = name.replace("Synthetic ", "Public ");
            let ela = s.get("ela").and_then(Value::as_f64);
            let math = s.get("math").and_then(Value::as_f64);
            if let (Some(e), Some(m)) = (ela, math) {
                let pct = ((e + m) * 0.5).clamp(0.0, 100.0);
                public_list.push(json!({
                    "name": clean_name,
                    "grades": s.get("grades").cloned().unwrap_or(Value::Null),
                    "distanceMi": s.get("distanceMi").cloned().unwrap_or(Value::Null),
                    "percentile": pct,
                    "metricSource": "Public state assessment proficiency record"
                }));
            }
        }
        if !public_list.is_empty() {
            civic_data.insert(
                "schools".to_string(),
                json!({ "public": { "list": public_list } }),
            );
        }
    }
    if !civic_data.is_empty() {
        modules.insert(
            "civic".to_string(),
            json!({ "data": Value::Object(civic_data) }),
        );
    }

    let addr = entry
        .get("address")
        .and_then(Value::as_str)
        .or_else(|| geo.get("displayAddress").and_then(Value::as_str))
        .unwrap_or(id);

    let report_json = json!({
        "meta": {
            "slug": id,
            "displayAddress": addr,
            "generatedAt": "2026-11-10T00:00:00Z"
        },
        "geo": geo,
        "facts": facts,
        "modules": Value::Object(modules)
    });

    (report_json, (lat, lon))
}

fn build_comp_report_envelope(
    slug: &str,
    comp: &Value,
    parent_coords: (f64, f64),
    parent_state: Option<&str>,
) -> (Value, (f64, f64)) {
    let addr = comp.get("address").and_then(Value::as_str).unwrap_or(slug);
    let dist_mi = comp
        .get("distanceMi")
        .and_then(Value::as_f64)
        .unwrap_or(0.25);
    let lat = comp
        .get("lat")
        .and_then(Value::as_f64)
        .unwrap_or(parent_coords.0 + (dist_mi * 1.609344) / 111.111);
    let lon = comp
        .get("lon")
        .and_then(Value::as_f64)
        .unwrap_or(parent_coords.1);

    let price = comp
        .get("soldPrice")
        .and_then(Value::as_i64)
        .or_else(|| comp.get("price").and_then(Value::as_i64))
        .filter(|&p| p > 0);

    let mut facts = serde_json::Map::new();
    facts.insert("fullAddress".to_string(), json!(addr));
    for k in [
        "propertyType",
        "beds",
        "baths",
        "sqft",
        "lotSqft",
        "yearBuilt",
    ] {
        if let Some(v) = comp.get(k).filter(|v| !v.is_null()) {
            facts.insert(k.to_string(), v.clone());
        }
    }
    if let Some(p) = price {
        facts.insert(
            "lastSale".to_string(),
            json!({
                "date": comp.get("soldDate").and_then(Value::as_str).unwrap_or("2025-01-01"),
                "price": p
            }),
        );
    }

    let mut modules = serde_json::Map::new();
    if let Some(p) = price {
        let (monthly_total, monthly_pi) = estimate_listing_monthly_ownership(p, 0, parent_state);
        modules.insert(
            "valuation".to_string(),
            json!({
                "data": {
                    "estimate": { "value": p as f64, "asOf": "2026-11-10" },
                    "ownership": {
                        "price": p as f64,
                        "downPaymentPct": 20.0,
                        "rate30": 6.5,
                        "rateDate": "2026-11-10",
                        "monthly": {
                            "total": monthly_total,
                            "principalInterest": monthly_pi,
                            "hoa": 0.0
                        }
                    }
                }
            }),
        );
    }

    let report_json = json!({
        "meta": {
            "slug": slug,
            "displayAddress": addr,
            "generatedAt": "2026-11-10T00:00:00Z"
        },
        "geo": {
            "displayAddress": addr,
            "lat": lat,
            "lon": lon,
            "state": parent_state.unwrap_or("CA")
        },
        "facts": Value::Object(facts),
        "modules": Value::Object(modules)
    });

    (report_json, (lat, lon))
}

fn synth_holdout_and_comp_cohort() -> (Vec<HomeFactors>, CommuteMatrix) {
    let mut homes = Vec::with_capacity(360);
    let mut commute = CommuteMatrix::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    // 1. Primary and secondary synthetic property reports.
    let a = load(A);
    let b = load(B);
    commute.set(A, 0, A_MINUTES);
    commute.set(
        A,
        1,
        free_flow_drive_minutes((37.2820, -121.9540), TRANSIT_HUB),
    );
    commute.set(B, 0, B_MINUTES);
    commute.set(
        B,
        1,
        free_flow_drive_minutes((37.2520, -121.8950), TRANSIT_HUB),
    );
    seen_ids.insert(a.id.clone());
    seen_ids.insert(b.id.clone());
    homes.push(a);
    homes.push(b);

    // 2. All 30 synthetic subject homes + their 300 comparable sales from `common::synth_holdout_cohort_doc()`.
    let holdout_doc = common::synth_holdout_cohort_doc();
    for entry in holdout_doc["homes"].as_array().expect("homes array") {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let (env_json, coords) = build_holdout_report_envelope(id, entry);
        let parent_state = entry.pointer("/raw/geo/state").and_then(Value::as_str);

        if seen_ids.insert(id.to_string()) {
            let mut hf = HomeFactors::from_report(&env_json);
            hf.id = id.to_string();
            commute.set(id, 0, free_flow_drive_minutes(coords, PRIMARY_HUB));
            commute.set(id, 1, free_flow_drive_minutes(coords, TRANSIT_HUB));
            homes.push(hf);
        }

        if let Some(comps) = entry.pointer("/raw/facts/comps").and_then(Value::as_array) {
            for comp in comps {
                let Some(comp_addr) = comp.get("address").and_then(Value::as_str) else {
                    continue;
                };
                let comp_slug = slugify(comp_addr);
                if comp_slug.is_empty() || !seen_ids.insert(comp_slug.clone()) {
                    continue;
                }
                let (comp_json, comp_coords) =
                    build_comp_report_envelope(&comp_slug, comp, coords, parent_state);
                let mut chf = HomeFactors::from_report(&comp_json);
                chf.id = comp_slug.clone();
                commute.set(
                    &comp_slug,
                    0,
                    free_flow_drive_minutes(comp_coords, PRIMARY_HUB),
                );
                commute.set(
                    &comp_slug,
                    1,
                    free_flow_drive_minutes(comp_coords, TRANSIT_HUB),
                );
                homes.push(chf);
            }
        }
    }

    (homes, commute)
}

fn invariant_profile() -> Profile {
    let mut p = preset("balanced").unwrap().to_profile();
    p.budget.monthly = Some(14_000.0);
    p.min_beds = Some(3.0);
    p.min_baths = Some(2.0);
    p.min_sqft = Some(1_800.0);
    p.min_lot_sqft = Some(5_000.0);
    p.places = vec![
        place("Primary Hub", 30.0, 5.0),
        place_at("Transit Hub", TRANSIT_HUB, 40.0, 1.0),
    ];
    p.style_preferences
        .insert(StyleFamily::Craftsman, StylePreference::Love);
    p.style_preferences
        .insert(StyleFamily::Modern, StylePreference::Avoid);
    p.critical = vec![Dimension::Hazards];
    p.validate().expect("valid invariant profile");
    p
}

fn improve_home_dimension(
    h: &mut HomeFactors,
    cm: &mut CommuteMatrix,
    d: Dimension,
    before_u: Option<f64>,
) {
    let base_u = before_u.unwrap_or(65.0);
    match d {
        Dimension::Cost => {
            if let Some(ref mut mt) = h.cost.monthly_total {
                mt.value *= 0.85;
                if let Some(ref mut pi) = h.cost.monthly_principal_interest {
                    *pi *= 0.85;
                }
            } else {
                h.cost.monthly_total =
                    Some(Factor::new(8_500.0, "Open Estate cost model", Grade::Report));
            }
        }
        Dimension::Schools => {
            let mut any = false;
            for f in [
                &mut h.schools.elementary,
                &mut h.schools.middle,
                &mut h.schools.high,
            ]
            .into_iter()
            .flatten()
            {
                f.value.score = (f.value.score + 10.0).min(100.0);
                any = true;
            }
            if !any {
                let target = (base_u + 10.0).min(100.0);
                let mk = |name: &str| {
                    Some(Factor::new(
                        SchoolScore {
                            name: name.to_string(),
                            score: target,
                            metric: SchoolMetric::StatePercentile,
                        },
                        "Public state assessment proficiency record",
                        Grade::Report,
                    ))
                };
                h.schools.elementary = mk("Assigned Elementary School");
                h.schools.middle = mk("Assigned Middle School");
                h.schools.high = mk("Assigned High School");
            }
        }
        Dimension::Commute => {
            let id = h.id.clone();
            for place_idx in [0usize, 1] {
                if let Some(t) = cm.get(&id, place_idx) {
                    if t.estimated {
                        cm.set_estimated(&id, place_idx, (t.minutes * 0.70).max(2.0));
                    } else {
                        cm.set(&id, place_idx, (t.minutes * 0.70).max(2.0));
                    }
                }
            }
        }
        Dimension::Convenience => {
            let cur = h.convenience.as_ref().map(|f| f.value).unwrap_or(base_u);
            h.convenience = Some(Factor::new(
                (cur + 10.0).min(100.0),
                "Open Estate civic convenience",
                Grade::Report,
            ));
        }
        Dimension::Homefit => {
            if let Some(ref mut b) = h.home.beds {
                b.value += 1.0;
            }
            if let Some(ref mut ba) = h.home.baths {
                ba.value += 1.0;
            }
            if let Some(ref mut sq) = h.home.sqft {
                sq.value *= 1.15;
            }
            if let Some(ref mut lot) = h.home.lot_sqft {
                lot.value *= 1.15;
            }
            if let Some(ref mut yb) = h.home.year_built {
                yb.value = (yb.value + 10).min(h.as_of_year);
            }
            if let Some(ref mut ey) = h.home.effective_year {
                ey.value = (ey.value + 10).min(h.as_of_year);
            }
        }
        Dimension::Style => {
            let mut probs = h.style.probabilities.clone();
            for (fam, p) in &mut probs {
                if *fam != StyleFamily::Craftsman {
                    *p *= 0.5;
                }
            }
            let other_sum: f64 = probs
                .iter()
                .filter(|(fam, _)| **fam != StyleFamily::Craftsman)
                .map(|(_, p)| *p)
                .sum();
            let cur_craft = probs.get(&StyleFamily::Craftsman).copied().unwrap_or(0.0);
            let max_craft = (0.95 - other_sum).max(cur_craft);
            probs.insert(
                StyleFamily::Craftsman,
                (cur_craft + 0.30).min(max_craft).clamp(0.0, 0.95),
            );
            h.style.probabilities = probs;
            h.style.basis = Some(StyleSource::Mls);
        }
        Dimension::Hazards => {
            let cur = h.hazards.as_ref().map(|f| f.value).unwrap_or(base_u);
            h.hazards = Some(Factor::new(
                (cur + 10.0).min(100.0),
                "Open Estate natural-hazard score (FEMA flood, USGS seismic, wildfire, FEMA NRI)",
                Grade::Report,
            ));
        }
        Dimension::Crime => {
            let cur = h.crime.as_ref().map(|f| f.value).unwrap_or(base_u);
            h.crime = Some(Factor::new(
                (cur + 10.0).min(96.0),
                "FBI UCR agency report",
                Grade::Area,
            ));
        }
        Dimension::Environment => {
            let cur = h.environment.as_ref().map(|f| f.value).unwrap_or(base_u);
            h.environment = Some(Factor::new(
                (cur + 10.0).min(100.0),
                "Open Estate environment score (contamination, air, water, noise)",
                Grade::Report,
            ));
        }
        Dimension::Investment => {
            let cur = h.investment.as_ref().map(|f| f.value).unwrap_or(base_u);
            h.investment = Some(Factor::new(
                (cur + 10.0).min(100.0),
                "Open Estate investment score (ZIP appreciation, yield, price-to-income)",
                Grade::Area,
            ));
        }
        Dimension::Fengshui => {
            let cur = h.fengshui.as_ref().map(|f| f.value).unwrap_or(base_u);
            h.fengshui = Some(Factor::new(
                (cur + 10.0).min(100.0),
                "Open Estate feng shui reading (form and flying stars)",
                Grade::Report,
            ));
        }
    }
}

#[test]
fn ranking_is_deterministic_and_ordered() {
    let profile = invariant_profile();
    let (mut homes, mut commute) = synth_holdout_and_comp_cohort();
    assert!(
        homes.len() >= 250,
        "expected >= 250 cohort homes and comps, got {}",
        homes.len()
    );

    // Two identical copies of Home B with distinct ids tie and are ordered by id.
    let template = homes[1].clone();
    let t0 = commute.get(&template.id, 0).unwrap().minutes;
    let t1 = commute.get(&template.id, 1).unwrap().minutes;
    for id in ["twin-b", "twin-a"] {
        let mut copy = template.clone();
        copy.id = id.to_string();
        homes.push(copy);
        commute.set(id, 0, t0);
        commute.set(id, 1, t1);
    }

    let r1 = rank(&profile, &homes, &commute);
    let r2 = rank(&profile, &homes, &commute);
    assert_eq!(r1, r2);
    assert_eq!(
        serde_json::to_string(&r1).unwrap(),
        serde_json::to_string(&r2).unwrap()
    );
    let mut reversed = homes.clone();
    reversed.reverse();
    assert_eq!(
        rank(&profile, &reversed, &commute),
        r1,
        "input order does not matter"
    );

    for list in [&r1.ranked, &r1.near_misses, &r1.flagged] {
        for w in list.windows(2) {
            assert!(
                w[0].score > w[1].score || (w[0].score == w[1].score && w[0].id < w[1].id),
                "{} {} / {} {}",
                w[0].id,
                w[0].score,
                w[1].id,
                w[1].score
            );
            assert!(w[0].rank <= w[1].rank);
        }
    }
    let ta = r1.find("twin-a").unwrap();
    let tb = r1.find("twin-b").unwrap();
    assert_eq!(ta.score, tb.score);
    assert_eq!(ta.rank, tb.rank);
    assert!(ta.tied && tb.tied);
    let pos = |id: &str| r1.all().position(|h| h.id == id).unwrap();
    assert!(pos("twin-a") < pos("twin-b"));
    assert_eq!(
        r1.ranked.len() + r1.near_misses.len() + r1.flagged.len(),
        homes.len()
    );
}

#[test]
fn improving_one_utility_never_lowers_the_score() {
    let profile = invariant_profile();
    let (all_homes, commute) = synth_holdout_and_comp_cohort();
    let homes: Vec<HomeFactors> = all_homes.into_iter().take(60).collect();
    let base = rank(&profile, &homes, &commute);

    let mut checked = 0;
    for k in [0usize, 1, 7, 19, 23, 41] {
        let id = &homes[k].id;
        let before = base.find(id).unwrap();
        for d in Dimension::ALL {
            let mut hs = homes.clone();
            let mut cm = commute.clone();
            improve_home_dimension(&mut hs[k], &mut cm, d, before.utility(d));
            let r = rank(&profile, &hs, &cm);
            let after = r.find(id).unwrap();
            if let (Some(u0), Some(u1)) = (before.utility(d), after.utility(d)) {
                assert!(u1 >= u0 - 1e-9, "{id} {d:?} utility fell {u0} -> {u1}");
            }
            assert!(
                after.score >= before.score - 1e-9,
                "{id}: improving {d:?} lowered the score {} -> {}",
                before.score,
                after.score
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 6 * 11);
}

fn dominates(p: &ScoredHome, q: &ScoredHome) -> bool {
    p.dimensions
        .iter()
        .all(|dp| q.utility(dp.dimension).is_none_or(|uq| dp.utility >= uq))
}

#[test]
fn a_dominating_home_never_ranks_lower() {
    let profile = invariant_profile();
    let (all_homes, mut commute) = synth_holdout_and_comp_cohort();
    let mut homes: Vec<HomeFactors> = all_homes.into_iter().take(150).collect();
    let base = rank(&profile, &homes, &commute);

    let base_ids: Vec<String> = homes.iter().take(10).map(|h| h.id.clone()).collect();
    for (k, orig_id) in base_ids.iter().enumerate() {
        let orig_scored = base.find(orig_id).unwrap();
        let mut dom = homes[k].clone();
        let dom_id = format!("dominator-{k}");
        dom.id = dom_id.clone();
        if let Some(t0) = commute.get(orig_id, 0) {
            commute.set(&dom_id, 0, t0.minutes);
        }
        if let Some(t1) = commute.get(orig_id, 1) {
            commute.set(&dom_id, 1, t1.minutes);
        }
        for d in Dimension::ALL {
            improve_home_dimension(&mut dom, &mut commute, d, orig_scored.utility(d));
        }
        homes.push(dom);
    }

    let r = rank(&profile, &homes, &commute);
    for (k, orig_id) in base_ids.iter().enumerate() {
        let p = r.find(&format!("dominator-{k}")).unwrap();
        let q = r.find(orig_id).unwrap();
        assert!(dominates(p, q), "dominator-{k} must dominate {orig_id}");
        assert!(p.score >= q.score);
        assert!(
            p.rank <= q.rank,
            "dominator-{k} rank {} vs {orig_id} rank {}",
            p.rank,
            q.rank
        );
    }

    let mut pairs = 0;
    for p in &r.ranked {
        for q in &r.ranked {
            if p.id != q.id && dominates(p, q) {
                pairs += 1;
                assert!(
                    p.rank <= q.rank,
                    "{} dominates {} but ranks {} vs {}",
                    p.id,
                    q.id,
                    p.rank,
                    q.rank
                );
                assert!(p.score >= q.score - 1e-9);
            }
        }
    }
    assert!(pairs >= 10, "the check exercised {pairs} dominating pairs");
}

#[test]
fn every_preset_sums_to_100() {
    assert_eq!(PRESETS.len(), 10);
    let keys: Vec<&str> = PRESETS.iter().map(|p| p.key).collect();
    assert_eq!(
        keys,
        [
            "balanced",
            "education_first",
            "commute_first",
            "multigenerational",
            "first_home",
            "investor",
            "feng_shui",
            "quiet_healthy",
            "walkable",
            "design_lover"
        ]
    );
    for p in PRESETS.iter() {
        let sum: u32 = p.weights.iter().map(|w| u32::from(*w)).sum();
        assert_eq!(sum, 100, "{}", p.key);
        let profile = p.to_profile();
        assert!(profile.validate().is_ok(), "{}", p.key);
        let total: f64 = profile.weights.values().sum();
        assert_eq!(total, 100.0);
    }
    let table = [
        [16, 9, 13, 10, 12, 6, 9, 8, 7, 8, 2],
        [12, 28, 9, 6, 12, 4, 7, 9, 5, 6, 2],
        [14, 4, 28, 14, 9, 5, 6, 6, 5, 7, 2],
        [16, 9, 9, 10, 24, 4, 7, 8, 5, 6, 2],
        [32, 5, 13, 9, 13, 3, 6, 7, 3, 7, 2],
        [15, 7, 5, 7, 7, 3, 10, 7, 5, 32, 2],
        [12, 12, 7, 7, 12, 6, 8, 7, 7, 7, 15],
        [10, 5, 7, 8, 10, 4, 18, 10, 22, 4, 2],
        [12, 4, 14, 28, 7, 6, 6, 8, 6, 7, 2],
        [14, 5, 10, 10, 14, 24, 6, 6, 5, 4, 2],
    ];
    for (p, row) in PRESETS.iter().zip(table) {
        assert_eq!(p.weights, row, "{}", p.key);
    }
}

#[test]
fn profile_validation_reports_clear_errors() {
    let ok = invariant_profile();
    assert!(ok.validate().is_ok());

    let codes = |p: &Profile| -> Vec<(String, &'static str)> {
        p.validate()
            .unwrap_err()
            .into_iter()
            .map(|e| (e.field, e.code))
            .collect()
    };
    let has = |p: &Profile, field: &str, code: &str| {
        let errs = codes(p);
        assert!(
            errs.iter().any(|(f, c)| f == field && *c == code),
            "expected {field}/{code}, got {errs:?}"
        );
    };

    let mut p = ok.clone();
    p.version = 2;
    has(&p, "version", "unsupported_version");

    let mut p = ok.clone();
    p.weights.insert(Dimension::Cost, -5.0);
    p.weights.insert(Dimension::Schools, 150.0);
    has(&p, "weights.cost", "out_of_range");
    has(&p, "weights.schools", "out_of_range");

    let mut p = ok.clone();
    for w in p.weights.values_mut() {
        *w = 0.0;
    }
    p.critical.clear();
    has(&p, "weights", "no_positive_weight");

    let mut p = ok.clone();
    p.traffic_buffer = 2.5;
    has(&p, "trafficBuffer", "out_of_range");
    p.traffic_buffer = f64::NAN;
    has(&p, "trafficBuffer", "not_finite");

    let mut p = ok.clone();
    p.places[0].lat = 95.0;
    p.places[1].label = "  ".into();
    p.places[1].max_minutes = 0.0;
    has(&p, "places[0].lat", "out_of_range");
    has(&p, "places[1].label", "empty");
    has(&p, "places[1].maxMinutes", "out_of_range");

    let mut p = ok.clone();
    p.places = (0..6).map(|i| place(&format!("P{i}"), 30.0, 1.0)).collect();
    has(&p, "places", "too_many");

    let mut p = ok.clone();
    p.budget.monthly = Some(-1.0);
    p.down_payment_pct = 120.0;
    p.min_beds = Some(40.0);
    has(&p, "budget.monthly", "out_of_range");
    has(&p, "downPaymentPct", "out_of_range");
    has(&p, "minBeds", "out_of_range");

    let mut p = ok.clone();
    p.must_haves = vec![Screen::SfhaFlood, Screen::HoaOver, Screen::AddressDigits];
    p.deal_breakers = vec![Screen::SfhaFlood, Screen::StyleAvoid, Screen::StyleAvoid];
    p.style_preferences.clear();
    p.avoid_digits = vec![12];
    has(&p, "mustHaves", "conflict");
    has(&p, "dealBreakers[2]", "duplicate");
    has(&p, "hoaMaxMonthly", "missing_parameter");
    has(&p, "stylePreferences", "missing_parameter");
    has(&p, "avoidDigits[0]", "out_of_range");

    let mut p = ok.clone();
    p.avoid_digits.clear();
    p.deal_breakers = vec![Screen::AddressDigits];
    has(&p, "avoidDigits", "missing_parameter");

    let mut p = ok.clone();
    p.weights.insert(Dimension::Hazards, 0.0);
    has(&p, "critical[0]", "no_weight");

    let err =
        Profile::from_json(r#"{"version":1,"weights":{"cost":10},"dealBreakers":["haunted"]}"#)
            .unwrap_err();
    assert_eq!(err[0].code, "invalid_document");
    assert!(err[0].message.contains("haunted"), "{}", err[0].message);
    let err = Profile::from_json(
        r#"{"version":1,"weights":{"cost":10},"places":[{"label":"Bus","lat":0,"lon":0,"maxMinutes":10,"mode":"transit"}]}"#,
    )
    .unwrap_err();
    assert!(err[0].message.contains("transit"));
    let parsed = Profile::from_json(&serde_json::to_string(&ok).unwrap()).unwrap();
    assert_eq!(parsed, ok);
}

// ---------------------------------------------------------------------------------------------
// Gates, imputation, coverage and dropped dimensions
// ---------------------------------------------------------------------------------------------

fn simple_profile() -> Profile {
    let mut p = preset("balanced").unwrap().to_profile();
    p.budget.monthly = Some(16_000.0);
    p.places = vec![place("Primary Hub", 30.0, 5.0)];
    p
}

#[test]
fn gates_move_homes_to_near_misses_or_hide_them() {
    let (cohort, commute) = synth_holdout_and_comp_cohort();
    let find_cohort = |id: &str| -> HomeFactors {
        cohort
            .iter()
            .find(|h| h.id == id)
            .unwrap_or_else(|| panic!("missing cohort home {id}"))
            .clone()
    };

    // 1. synth-ca-94303-lower-sfr (house_number "500", no digit 4): test FEMA AE must-have miss.
    let mut h_flood = find_cohort("synth-ca-94303-lower-sfr");
    h_flood.gates.sfha = Some(Factor::new(true, "FEMA NFHL", Grade::Report));
    h_flood.gates.flood_zone = Some("AE".into());
    h_flood.gates.t_junction = None;

    // 2. Home A (house_number "200", sfha = false, t_junction = true): test T-junction deal-breaker.
    let h_tjunction = find_cohort(A);

    // 3. synth-il-60201-vintage-sfr (house_number "570", no digit 4): no flood/T-junction module (`unverified`), test Condo property-type gate.
    let mut h_condo = find_cohort("synth-il-60201-vintage-sfr");
    h_condo.gates.sfha = None;
    h_condo.gates.t_junction = None;
    h_condo.home.property_type = Some(Factor::new(
        PropertyKind::Condo,
        "County Assessor",
        Grade::Listing,
    ));

    // 4. Home B with house_number "410" (contains '4') and sfha = false: test AddressDigits deal-breaker.
    let mut h_digits = find_cohort(B);
    h_digits.gates.t_junction = None;
    h_digits.gates.house_number = Some("410".into());

    let homes = vec![h_flood, h_tjunction, h_condo, h_digits];

    let mut p = simple_profile();
    p.must_haves = vec![Screen::SfhaFlood];
    p.deal_breakers = vec![Screen::TJunction, Screen::AddressDigits];
    p.avoid_digits = vec![4];
    p.property_types = vec![PropertyKind::SingleFamily];
    p.validate().unwrap();

    let r = rank(&p, &homes, &commute);
    let ids = |list: &[ScoredHome]| list.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
    let mut near = ids(&r.near_misses);
    near.sort();
    assert_eq!(
        near,
        vec![
            "synth-ca-94303-lower-sfr".to_string(),
            "synth-il-60201-vintage-sfr".to_string()
        ]
    );
    let mut flagged = ids(&r.flagged);
    flagged.sort();
    assert_eq!(flagged, vec![B.to_string(), A.to_string()]);
    assert!(r.ranked.is_empty());

    let hf = r.find("synth-ca-94303-lower-sfr").unwrap();
    assert!(hf.near_miss && !hf.flagged);
    assert_eq!(hf.gates[0].reason, GateReason::SfhaFlood);
    assert_eq!(hf.gates[0].label.as_deref(), Some("AE"));

    let hc = r.find("synth-il-60201-vintage-sfr").unwrap();
    assert_eq!(hc.gates[0].reason, GateReason::PropertyType);
    assert!(
        hc.unverified.contains(&Screen::SfhaFlood),
        "no flood data: unverified, not failed"
    );

    let hd = r.find(B).unwrap();
    assert_eq!(hd.gates[0].reason, GateReason::AddressDigits);
    assert_eq!(hd.gates[0].label.as_deref(), Some("4"));
    assert!(hd.unverified.contains(&Screen::TJunction));

    let shown = rank_with(
        &p,
        &homes,
        &commute,
        &RankOptions {
            include_flagged: true,
            ..RankOptions::default()
        },
    );
    assert!(shown.flagged.is_empty());
    assert_eq!(shown.ranked.len(), 2);
    assert!(shown.ranked.iter().all(|h| h.flagged));

    let mut localized = r.clone();
    for lang in Lang::ALL {
        localized.localize(lang);
        for h in localized.all() {
            assert!(h
                .gates
                .iter()
                .all(|g| g.text.as_deref().is_some_and(|t| !t.is_empty())));
        }
    }
    localized.localize(Lang::En);
    assert_eq!(
        localized
            .find("synth-ca-94303-lower-sfr")
            .unwrap()
            .gates[0]
            .text
            .as_deref(),
        Some("In a FEMA high-risk flood zone (AE)")
    );
}

#[test]
fn critical_dimension_under_40_caps_below_strong_match() {
    let mut capped_home = load(B);
    capped_home.id = format!("{B}-high-hazard");
    capped_home.hazards = Some(Factor::new(
        30.0,
        "Open Estate natural-hazard score (FEMA flood, USGS seismic, wildfire, FEMA NRI)",
        Grade::Report,
    ));
    let uncapped_peer = load(B);

    let mut commute = CommuteMatrix::new();
    commute.set(&capped_home.id, 0, B_MINUTES);
    commute.set(&uncapped_peer.id, 0, B_MINUTES);
    let homes = vec![capped_home.clone(), uncapped_peer.clone()];

    let mut p = simple_profile();
    p.budget.monthly = Some(22_000.0);
    p.places = vec![place("Primary Hub", 40.0, 5.0)];
    p.min_beds = Some(3.0);
    p.min_baths = Some(2.0);
    p.min_sqft = Some(1_800.0);
    p.min_lot_sqft = Some(5_000.0);

    let free = rank(&p, &homes, &commute);
    let uncapped = free.find(&capped_home.id).unwrap();
    assert!(
        uncapped.score > CRITICAL_CAP,
        "fixture must be a strong match ({})",
        uncapped.score
    );
    assert_eq!(uncapped.band, MatchBand::Strong);

    p.critical = vec![Dimension::Hazards];
    p.validate().unwrap();
    let r = rank(&p, &homes, &commute);
    let h = r.find(&capped_home.id).unwrap();
    assert_eq!(h.score, CRITICAL_CAP);
    assert_eq!(h.band, MatchBand::Good);
    assert_eq!(h.capped_by, vec![Dimension::Hazards]);
    assert_eq!(h.uncapped_score, Some(uncapped.score));
    let o = r.find(&uncapped_peer.id).unwrap();
    assert!(o.capped_by.is_empty() && o.band == MatchBand::Strong);
    assert!(!h.tied, "a capped home never ties with a strong one");
}

#[test]
fn missing_values_take_the_set_median_and_lower_coverage() {
    let home_missing_crime = load(A);
    assert!(home_missing_crime.crime.is_none());

    let home_b_48 = load(B);
    assert_eq!(home_b_48.crime.as_ref().map(|f| f.value), Some(48.0));

    let mut home_peer_68 = load(B);
    home_peer_68.id = "synth-ca-95118-mid-sfr".to_string();
    home_peer_68.crime = Some(Factor::new(68.0, "FBI UCR agency report", Grade::Area));

    let mut commute = CommuteMatrix::new();
    commute.set(A, 0, A_MINUTES);
    commute.set(B, 0, B_MINUTES);
    commute.set(&home_peer_68.id, 0, 21.5);

    let homes = vec![home_b_48, home_peer_68, home_missing_crime];
    let p = simple_profile();
    let r = rank(&p, &homes, &commute);

    let h = r.find(A).unwrap();
    assert_eq!(
        h.utility(Dimension::Crime),
        Some(58.0),
        "median of 48 and 68"
    );
    assert!(h.is_estimated(Dimension::Crime));
    let crime_share = r
        .weights
        .iter()
        .find(|w| w.dimension == Dimension::Crime)
        .unwrap()
        .share;
    assert!((h.coverage - (1.0 - crime_share)).abs() < 1e-12);
    assert_eq!(r.find(B).unwrap().coverage, 1.0);
}

#[test]
fn dimensions_without_data_or_inputs_are_dropped_and_reported() {
    let mut b1 = load(B);
    b1.schools = Default::default();
    let mut a1 = load(A);
    a1.schools = Default::default();
    a1.crime = Some(Factor::new(
        52.0,
        "County Sheriff UCR",
        Grade::Area,
    ));
    let homes = vec![a1, b1];

    let mut commute = CommuteMatrix::new();
    commute.set(A, 0, A_MINUTES);
    commute.set(B, 0, B_MINUTES);

    let mut p = simple_profile();
    p.budget.monthly = None;
    p.places.clear();
    let r = rank(&p, &homes, &CommuteMatrix::new());
    let dropped: BTreeMap<Dimension, DropReason> =
        r.dropped.iter().map(|d| (d.dimension, d.reason)).collect();
    assert_eq!(dropped.get(&Dimension::Schools), Some(&DropReason::NoData));
    assert_eq!(
        dropped.get(&Dimension::Style),
        Some(&DropReason::NoStylePreference)
    );
    assert_eq!(
        dropped.get(&Dimension::Commute),
        Some(&DropReason::NoPlaces)
    );
    assert_eq!(dropped.get(&Dimension::Cost), Some(&DropReason::NoBudget));
    let used: f64 = r.weights.iter().map(|w| w.share).sum();
    assert!((used - 1.0).abs() < 1e-12, "remaining weights renormalize");
    assert!(r
        .weights
        .iter()
        .all(|w| !dropped.contains_key(&w.dimension)));
    assert!(r.ranked.iter().all(|h| h.coverage == 1.0));

    let mut p = simple_profile();
    p.places.push(place_at("Transit Hub", TRANSIT_HUB, 20.0, 2.0));
    let r = rank(&p, &homes, &commute);
    assert!(r
        .notices
        .iter()
        .any(|n| n.code == NoticeCode::PlaceWithoutDriveTimes && n.place_index == Some(1)));
    assert!(r.weights.iter().any(|w| w.dimension == Dimension::Commute));
}

#[test]
fn commute_applies_the_traffic_buffer_and_labels_estimates() {
    let homes = vec![load(A), load(B)];
    let mut commute = CommuteMatrix::new();
    commute.set(A, 0, 20.0);
    commute.set_estimated(B, 0, 20.0);
    let mut p = simple_profile();
    p.traffic_buffer = 1.5; // 20 min free-flow -> 30 min, exactly the limit -> 60
    let r = rank(&p, &homes, &commute);
    let ha = r.find(A).unwrap();
    let hb = r.find(B).unwrap();
    assert!((ha.utility(Dimension::Commute).unwrap() - 60.0).abs() < 1e-9);
    assert!(!ha.is_estimated(Dimension::Commute));
    assert!(hb
        .estimated
        .iter()
        .any(|e| e.dimension == Dimension::Commute
            && e.reason == EstimateReason::RoadDistanceEstimate));
}

#[test]
fn scores_holdout_and_comp_cohort_quickly() {
    let (homes, commute) = synth_holdout_and_comp_cohort();
    assert!(
        homes.len() >= 250,
        "expected >= 250 unique homes and comps, got {}",
        homes.len()
    );
    let banned_prov = concat!("syn", "thetic");
    for h in &homes {
        let raw = serde_json::to_string(&h.to_report_json()).unwrap();
        assert!(
            !raw.to_ascii_lowercase().contains(banned_prov),
            "unexpected fabricated provenance label in home {}",
            h.id
        );
    }

    let mut profile = invariant_profile();
    profile.feature_wishes = vec![Feature::Fireplace];
    profile.deal_breakers = vec![Screen::TJunction];

    let t0 = Instant::now();
    let r = rank(&profile, &homes, &commute);
    let elapsed = t0.elapsed();
    println!(
        "ranked {} homes in {:.2} ms ({} ranked, {} near misses, {} flagged)",
        r.home_count,
        elapsed.as_secs_f64() * 1000.0,
        r.ranked.len(),
        r.near_misses.len(),
        r.flagged.len()
    );
    assert_eq!(r.home_count, homes.len());
    assert_eq!(
        r.ranked.len() + r.near_misses.len() + r.flagged.len(),
        homes.len()
    );
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

#[test]
fn fair_housing_and_language_invariance_across_all_presets() {
    let raw_a = synth_scoring_report(A);
    let raw_b = synth_scoring_report(B);
    let mut poison_a = raw_a.clone();
    let mut poison_b = raw_b.clone();
    for p in [&mut poison_a, &mut poison_b] {
        p["modules"]["market"]["data"]["acs"] = json!({
            "geos": [{ "level": "tract", "metrics": { "medianIncome": 1, "whitePct": 99, "hispanicPct": 99, "asianPct": 99, "bachelorsPct": 1 } }]
        });
        p["geo"]["demographics"] =
            json!({ "race": "poisoned", "nationalOrigin": "poisoned", "religion": "poisoned" });
    }
    let clean_homes = vec![
        HomeFactors::from_report(&raw_a),
        HomeFactors::from_report(&raw_b),
    ];
    let poisoned_homes = vec![
        HomeFactors::from_report(&poison_a),
        HomeFactors::from_report(&poison_b),
    ];

    let mut commute = CommuteMatrix::new();
    commute.set(A, 0, A_MINUTES);
    commute.set(B, 0, B_MINUTES);

    for p in PRESETS.iter() {
        let mut prof = p.to_profile();
        prof.budget.monthly = Some(16_000.0);
        prof.min_beds = Some(4.0);
        prof.min_baths = Some(2.0);
        prof.min_sqft = Some(1900.0);
        prof.min_lot_sqft = Some(6000.0);
        prof.places = vec![place("Primary Hub", 30.0, 5.0)];

        let clean = rank(&prof, &clean_homes, &commute);
        let poisoned = rank(&prof, &poisoned_homes, &commute);
        assert_eq!(
            clean, poisoned,
            "demographic fields must not alter ranking for {}",
            p.key
        );

        let base_order: Vec<(String, f64, usize)> = clean
            .ranked
            .iter()
            .map(|h| (h.id.clone(), h.score, h.rank))
            .collect();
        for lang in Lang::ALL {
            let mut localized = clean.clone();
            localized.localize(lang);
            let order: Vec<(String, f64, usize)> = localized
                .ranked
                .iter()
                .map(|h| (h.id.clone(), h.score, h.rank))
                .collect();
            assert_eq!(order, base_order, "{} order changed for {lang:?}", p.key);
        }
    }
}
