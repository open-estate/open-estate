//! Factor extraction from synthetic property reports (`cargo test --test scoring_factors_test`).

mod common;

use open_estate::scoring::catalog::{Feature, PropertyKind, SchoolLevel, StyleFamily};
use open_estate::scoring::factors::{Grade, HomeFactors, SchoolMetric, StyleSource};
use serde_json::{json, Value};

const A: &str = common::SYNTH_SECONDARY_SLUG;
const B: &str = common::SYNTH_PRIMARY_SLUG;

fn load(slug: &str) -> Value {
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
                "summaryScore": 8.8,
                "summaryScore100": 88,
                "assignedOrNearest": [
                    { "name": "Synth Beta Elementary", "level": "elementary", "grades": "K-5", "score": 8.0 },
                    { "name": "Synth Beta Middle", "level": "middle", "grades": "6-8", "score": 10.0 },
                    { "name": "Synth Beta High", "level": "high", "grades": "9-12", "score": 8.4 }
                ],
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
                "summaryScore": 9.2,
                "summaryScore100": 92,
                "assignedOrNearest": [
                    { "name": "Synth Alpha Elementary", "level": "elementary", "grades": "K-5", "score": 10.0 },
                    { "name": "Synth Alpha Middle", "level": "middle", "grades": "6-8", "score": 8.7 },
                    { "name": "Synth Alpha High", "level": "high", "grades": "9-12", "score": 8.8 }
                ],
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

fn school(f: &HomeFactors, level: SchoolLevel) -> (String, f64) {
    let s = f
        .schools
        .get(level)
        .unwrap_or_else(|| panic!("{} has no {level:?} school", f.id));
    (s.value.name.clone(), s.value.score / 10.0)
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn schools_come_from_public_dashboard_data() {
    let a = HomeFactors::from_report(&load(A));
    let b = HomeFactors::from_report(&load(B));
    let expect = [
        (&a, SchoolLevel::Elementary, "Synth Beta Elementary", 8.0),
        (&a, SchoolLevel::Middle, "Synth Beta Middle", 10.0),
        (&a, SchoolLevel::High, "Synth Beta High", 8.4),
        (&b, SchoolLevel::Elementary, "Synth Alpha Elementary", 10.0),
        (
            &b,
            SchoolLevel::Middle,
            "Synth Alpha Middle",
            8.0 + 2.0 / 3.0,
        ),
        (&b, SchoolLevel::High, "Synth Alpha High", 8.8),
    ];
    for (f, level, name, value) in expect {
        let (got_name, got) = school(f, level);
        assert_eq!(got_name, name);
        assert!(
            close(got, value, 1e-9),
            "{name}: {got} (0-10 scale) != {value}"
        );
        let s = f.schools.get(level).unwrap();
        assert_eq!(s.value.metric, SchoolMetric::CaDashboardTiers);
        assert_eq!(s.year, Some(2025), "2024-25 Dashboard is recorded as 2025");
        assert!(
            s.source.contains("California School Dashboard"),
            "{}",
            s.source
        );
        assert_eq!(s.grade, Grade::Report);
    }
}

#[test]
fn non_public_school_ratings_never_reach_a_factor() {
    let original = HomeFactors::from_report(&load(A));
    let mut r = load(A);
    for s in r["facts"]["schools"]["assigned"].as_array_mut().unwrap() {
        s["rating"] = json!(1);
        s["gsRating"] = json!(1);
        s["nicheGrade"] = json!("F");
        s["ela"] = json!(1);
        s["math"] = json!(1);
    }
    let civic = &mut r["modules"]["civic"]["data"]["schools"];
    civic["summaryScore"] = json!(0.1);
    civic["summaryScore100"] = json!(1);
    civic["hudProficiencyIndex"] = json!({ "index": 1 });
    for s in civic["public"]["list"].as_array_mut().unwrap() {
        s["score"] = json!(0.5);
        s["staffingScore"] = json!(0);
        s["gsRating"] = json!(1);
        s["readingProficiencyPct"] = json!(1.0);
    }
    for s in civic["assignedOrNearest"].as_array_mut().unwrap() {
        s["score"] = json!(0.5);
    }
    let poisoned = HomeFactors::from_report(&r);
    assert_eq!(poisoned.schools, original.schools);
}

#[test]
fn state_percentile_is_preferred_when_present() {
    let mut r = load(A);
    for s in r["modules"]["civic"]["data"]["schools"]["public"]["list"]
        .as_array_mut()
        .unwrap()
    {
        if s["name"] == "Synth Beta Elementary" {
            s["percentile"] = json!(72.5);
            s["metricYear"] = json!("2025-26");
            s["metricSource"] = json!("CAASPP 2025-26 research file, within-state percentile");
        }
    }
    let f = HomeFactors::from_report(&r);
    let e = f.schools.elementary.as_ref().unwrap();
    assert_eq!(e.value.score, 72.5);
    assert_eq!(e.value.metric, SchoolMetric::StatePercentile);
    assert_eq!(e.year, Some(2026));
    assert!(e.source.starts_with("CAASPP"));
    // Other levels keep the Dashboard tiers.
    assert_eq!(
        f.schools.middle.as_ref().unwrap().value.metric,
        SchoolMetric::CaDashboardTiers
    );
}

#[test]
fn attendance_zone_is_the_fallback_without_assigned_names() {
    let mut r = load(B);
    r["facts"]["schools"]["assigned"] = json!([]);
    let f = HomeFactors::from_report(&r);
    assert_eq!(school(&f, SchoolLevel::Elementary).0, "Synth Alpha Elementary");
    assert_eq!(school(&f, SchoolLevel::Middle).0, "Synth Alpha Middle");
    assert_eq!(school(&f, SchoolLevel::High).0, "Synth Alpha High");
}

#[test]
fn crime_is_agency_level_and_missing_when_agency_unpublished() {
    let a = HomeFactors::from_report(&load(A));
    let b = HomeFactors::from_report(&load(B));
    assert!(a.crime.is_none(), "A has no agency crime data");
    let c = b.crime.as_ref().expect("B agency score");
    assert_eq!(c.value, 48.0);
    assert_eq!(c.year, Some(2025));
    assert_eq!(c.grade, Grade::Area);
    assert!(c.source.contains("Metro Area Police Department"));
}

#[test]
fn style_is_craftsman_for_a_and_unknown_for_b() {
    let a = HomeFactors::from_report(&load(A));
    let b = HomeFactors::from_report(&load(B));
    assert_eq!(a.style.top_family(), Some(StyleFamily::Craftsman));
    assert_eq!(a.style.basis, Some(StyleSource::ListingText));
    assert!(close(a.style.probability(StyleFamily::Craftsman), 0.75, 1e-12));
    assert!(
        close(a.style.known_mass(), 0.75, 1e-12),
        "unexplained mass stays unknown"
    );

    assert_eq!(b.style.top_family(), None, "B has no style evidence");
    assert!(!b.style.is_evidence_backed());
    // Only a low-trust "likely" guess from build era and form.
    assert_eq!(b.style.basis, Some(StyleSource::EraPrior));
    assert!(b.style.known_mass() <= 0.3 + 1e-12);
}

#[test]
fn structured_style_sources_outrank_text() {
    let mut r = load(B);
    r["facts"]["architectureType"] = json!("Cape Cod");
    let f = HomeFactors::from_report(&r);
    assert_eq!(f.style.top_family(), Some(StyleFamily::CapeCod));
    assert_eq!(f.style.basis, Some(StyleSource::CountyRecord));
    assert!(close(f.style.known_mass(), 0.85, 1e-12));

    r["facts"]["architecturalStyle"] = json!(["Ranch", "Traditional"]);
    let f = HomeFactors::from_report(&r);
    assert_eq!(f.style.basis, Some(StyleSource::Mls));
    assert!(close(f.style.probability(StyleFamily::Ranch), 0.475, 1e-12));
    assert!(close(
        f.style.probability(StyleFamily::Traditional),
        0.475,
        1e-12
    ));

    // "See Remarks" is unknown, so the next source decides.
    r["facts"]["architecturalStyle"] = json!(["See Remarks"]);
    let f = HomeFactors::from_report(&r);
    assert_eq!(f.style.basis, Some(StyleSource::CountyRecord));

    // An MLS value overrides listing text on the Craftsman.
    let mut a = load(A);
    a["facts"]["architecturalStyle"] = json!("Victorian");
    let f = HomeFactors::from_report(&a);
    assert_eq!(f.style.top_family(), Some(StyleFamily::Victorian));
    assert_eq!(f.style.probability(StyleFamily::Craftsman), 0.0);
}

#[test]
fn t_junction_is_read_from_the_feng_shui_module() {
    let a = HomeFactors::from_report(&load(A));
    let b = HomeFactors::from_report(&load(B));
    assert_eq!(a.gates.t_junction.as_ref().map(|f| f.value), Some(true));
    assert_eq!(b.gates.t_junction.as_ref().map(|f| f.value), Some(false));

    // The structured block wins over the form entries when present.
    let mut r = load(A);
    r["modules"]["fengshui"]["data"]["tJunction"] = json!({ "hasAlignment": false, "hits": [] });
    let f = HomeFactors::from_report(&r);
    assert_eq!(f.gates.t_junction.as_ref().map(|f| f.value), Some(false));
}

#[test]
fn home_fit_inputs_including_the_adu_and_effective_year() {
    let a = HomeFactors::from_report(&load(A));
    let b = HomeFactors::from_report(&load(B));
    let adu = a.home.adu.as_ref().expect("A has a permitted ADU");
    assert_eq!(adu.value.sqft, Some(584.0));
    assert_eq!(adu.value.beds, Some(1.0));
    assert_eq!(adu.value.baths, Some(1.0));
    assert!(b.home.adu.is_none());

    assert_eq!(a.home.year_built.as_ref().unwrap().value, 1925);
    assert_eq!(
        a.home.effective_year.as_ref().unwrap().value,
        1960,
        "assessor effective year in the facts text"
    );
    assert_eq!(b.home.year_built.as_ref().unwrap().value, 1975);
    assert!(b.home.effective_year.is_none());

    for (f, beds, baths, sqft, lot, stories) in [
        (&a, 4.0, 2.0, 1990.0, 10018.0, 1.0),
        (&b, 4.0, 2.0, 1956.0, 6300.0, 2.0),
    ] {
        assert_eq!(f.home.beds.as_ref().unwrap().value, beds);
        assert_eq!(f.home.baths.as_ref().unwrap().value, baths);
        assert_eq!(f.home.sqft.as_ref().unwrap().value, sqft);
        assert_eq!(f.home.lot_sqft.as_ref().unwrap().value, lot);
        assert_eq!(f.home.stories.as_ref().unwrap().value, stories);
        assert!(f.home.strong_adu_potential());
        assert_eq!(
            f.home.property_type.as_ref().unwrap().value,
            PropertyKind::SingleFamily
        );
    }
}

#[test]
fn module_scores_cost_and_gate_inputs() {
    let a = HomeFactors::from_report(&load(A));
    let b = HomeFactors::from_report(&load(B));
    let v = |f: &Option<open_estate::scoring::factors::Factor<f64>>| f.as_ref().map(|x| x.value);
    assert_eq!(a.as_of_year, 2026);
    assert_eq!(a.id, A);
    assert_eq!(v(&a.cost.monthly_total), Some(16496.0));
    assert_eq!(v(&b.cost.monthly_total), Some(14847.0));
    assert_eq!(a.cost.assumed_down_payment_pct, Some(20.0));
    assert_eq!(v(&a.cost.estimate), Some(2378000.0));
    assert!(close(v(&a.convenience).unwrap(), 51.2, 1e-9));
    assert!(close(v(&b.convenience).unwrap(), 78.0, 1e-9));
    assert_eq!(
        (
            v(&a.hazards),
            v(&a.environment),
            v(&a.investment),
            v(&a.fengshui)
        ),
        (Some(53.0), Some(60.0), Some(54.0), Some(52.0))
    );
    assert_eq!(
        (
            v(&b.hazards),
            v(&b.environment),
            v(&b.investment),
            v(&b.fengshui)
        ),
        (Some(59.0), Some(88.0), Some(59.0), Some(67.0))
    );

    for f in [&a, &b] {
        assert_eq!(f.gates.sfha.as_ref().map(|x| x.value), Some(false));
        assert_eq!(f.gates.very_high_fire.as_ref().map(|x| x.value), Some(false));
        assert_eq!(
            f.gates.freeway_within_500ft.as_ref().map(|x| x.value),
            Some(false)
        );
    }
    assert_eq!(a.gates.freeway_distance_m, Some(1430.0));
    assert_eq!(b.gates.freeway_distance_m, Some(655.0));
    assert_eq!(a.gates.house_number.as_deref(), Some("200"));
    assert_eq!(b.gates.house_number.as_deref(), Some("100"));
    assert_eq!(a.gates.flood_zone.as_deref(), Some("X"));

    let fa = &a.features.as_ref().unwrap().value;
    assert!(
        fa.contains(&Feature::Hardwood)
            && fa.contains(&Feature::Fireplace)
            && fa.contains(&Feature::HighCeilings)
    );
    assert!(fa.contains(&Feature::MainLevelPrimary), "single-story home");
    assert!(
        !fa.contains(&Feature::Pool),
        "an above-ground spa is not a pool"
    );
    let fb = &b.features.as_ref().unwrap().value;
    assert!(fb.contains(&Feature::NaturalLight) && fb.contains(&Feature::MainLevelPrimary));
    assert!(!fb.contains(&Feature::Hardwood));
}

#[test]
fn demographic_fields_cannot_change_any_factor() {
    for slug in [A, B] {
        let original = HomeFactors::from_report(&load(slug));
        let mut r = load(slug);
        r["modules"]["market"]["data"]["acs"] = json!({
            "geos": [{
                "level": "tract",
                "metrics": {
                    "medianIncome": 1,
                    "whitePct": 99,
                    "hispanicPct": 99,
                    "asianPct": 99,
                    "bachelorsPct": 1
                }
            }]
        });
        r["geo"]["demographics"] = json!({ "race": "poisoned" });
        r["decisionFactors"]["connectivity"]["tractBroadbandPct"] = json!(0);
        assert_eq!(HomeFactors::from_report(&r), original, "{slug}");
    }
}

#[test]
fn report_json_is_compact_and_free_of_personal_data() {
    for slug in [A, B] {
        let f = HomeFactors::from_report(&load(slug));
        let j = f.to_report_json();
        assert_eq!(j["version"], 1);
        let text = serde_json::to_string(&j).unwrap();
        assert!(!text.contains("null"), "None fields are omitted: {text}");
        for banned in [
            "owner", "Owner", "mailing", "agent", "Agent", "phone", "trustee", "Trustee", "@",
        ] {
            assert!(!text.contains(banned), "{slug} factors contain {banned:?}");
        }
        assert!(
            text.len() < 12_000,
            "factors stay compact ({} bytes)",
            text.len()
        );
        // Round trip.
        let back: HomeFactors = serde_json::from_value(j).unwrap();
        assert_eq!(back, f);
        // Deterministic.
        assert_eq!(HomeFactors::from_report(&load(slug)), f);
    }
}

#[test]
fn empty_or_partial_reports_do_not_panic() {
    let f = HomeFactors::from_report(&json!({}));
    assert!(f.crime.is_none() && f.hazards.is_none() && f.schools.elementary.is_none());
    let f = HomeFactors::from_report(
        &json!({ "facts": { "beds": "3", "lotSqft": "6,000" }, "modules": { "civic": { "data": null } } }),
    );
    assert_eq!(f.home.beds.as_ref().unwrap().value, 3.0);
    assert_eq!(f.home.lot_sqft.as_ref().unwrap().value, 6000.0);
}

#[test]
fn null_or_zero_placeholders_stay_missing_while_zero_hoa_is_valid() {
    let f = HomeFactors::from_report(&json!({
        "facts": {
            "sqft": 0,
            "lotSqft": 0,
            "hoaMonthly": 0,
            "mlsListing": { "listPrice": 0 },
            "schools": {
                "assigned": [{ "name": "Synth Gamma Elementary", "grades": "K-5" }]
            }
        },
        "modules": {
            "valuation": {
                "data": {
                    "estimate": { "value": 0 },
                    "ownership": { "price": 0, "monthly": { "total": 0 } }
                }
            },
            "civic": {
                "data": {
                    "schools": {
                        "public": {
                            "list": [
                                { "name": "Synth Gamma High", "level": "high", "grades": "9-12", "caDashboard": { "elaTier": 5 } }
                            ]
                        }
                    }
                }
            }
        }
    }));
    assert!(f.home.sqft.is_none(), "zero sqft is treated as missing");
    assert!(
        f.home.lot_sqft.is_none(),
        "zero lotSqft is treated as missing"
    );
    assert!(f.cost.list_price.is_none());
    assert!(f.cost.estimate.is_none());
    assert!(f.cost.price.is_none());
    assert!(f.cost.monthly_total.is_none());
    assert_eq!(
        f.cost.hoa_monthly.as_ref().map(|h| h.value),
        Some(0.0),
        "$0 HOA is valid"
    );
    assert!(
        f.schools.elementary.is_none(),
        "high school must not match elementary school"
    );
}
