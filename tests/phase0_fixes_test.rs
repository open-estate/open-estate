// Phase 0 fixes for "Your Match" (implementation plan, section 4):
// 1. `wildfire_zone` is filled from the hazards module's real paths as a short normalized label.
// 2. The high-wildfire-risk exclusion filter understands the new labels.
// 3. `walk_score` stays on a 0–100 scale and never takes the 1–20 EPA National Walkability Index.
// 4. `summary.verdictTone` uses the same bands as the verdict text.

mod common;

use open_estate::db::{DbStore, SqlParam};
use open_estate::search::{
    compile_filters_to_sql, extract_listings_from_report, upsert_listings_batch, ListingInput,
    SearchFilters,
};
use open_estate::shared::EnvConfig;
use open_estate::web::summary::summarize_report;
use serde_json::{json, Value};
use std::collections::BTreeSet;

const PRIMARY: &str = common::SYNTH_PRIMARY_SLUG;
const SECONDARY: &str = common::SYNTH_SECONDARY_SLUG;

const FHSZ: &str = "/modules/hazards/data/wildfire/california";
const WHP: &str = "/modules/hazards/data/wildfire/whp";

fn fixture(slug: &str) -> Value {
    let mut rep = common::synth_report_for_slug(slug);
    let is_primary = slug == PRIMARY;
    rep["modules"]["hazards"]["data"]["wildfire"]["california"] = json!({
        "responsibilityArea": "LRA",
        "fhsz": "NonWildland",
        "fhszClass": 0,
        "inFhsz": false
    });
    rep["modules"]["hazards"]["data"]["wildfire"]["whp"] = json!({
        "class": 6,
        "label": "Non-burnable",
        "dataset": "USFS Wildfire Hazard Potential 2023 (270 m)"
    });
    rep["facts"]["walkScore"] = json!({
        "walk": if is_primary { 74.0 } else { 21.0 }
    });
    rep["modules"]["civic"]["data"]["walkability"]["index"] = json!(if is_primary { 14.67 } else { 12.83 });
    rep["modules"]["civic"]["data"]["scores"]["walkability"] = json!(if is_primary { 72.0 } else { 62.0 });

    if let Some(comps) = rep["facts"]["comps"].as_array_mut() {
        for c in comps.iter_mut() {
            c["source"] = json!("County Recorder");
        }
    }
    if let Some(vcomps) = rep["modules"]["valuation"]["data"]["comps"].as_array_mut() {
        for c in vcomps.iter_mut() {
            c["source"] = json!("County Recorder");
        }
    }
    if is_primary {
        let nearby: Vec<Value> = (1..=6)
            .map(|idx| {
                json!({
                    "address": format!("{} Synthlisting{:02} Dr, San Jose, CA 95118", 400 + idx, idx),
                    "price": 1_850_000 + (idx as i64) * 20_000,
                    "beds": 4,
                    "baths": 2.0,
                    "sqft": 1_900 + idx * 25,
                    "yearBuilt": 1974,
                    "status": "for_sale",
                    "source": "Public Feed"
                })
            })
            .collect();
        rep["facts"]["nearbyListings"] = Value::Array(nearby);
    }
    rep
}

/// The subject home is the first listing `extract_listings_from_report` returns; the rest are
/// nearby listings and comparable sales.
fn subject(report: &Value) -> ListingInput {
    let first = extract_listings_from_report(report)
        .into_iter()
        .next()
        .expect("subject listing");
    assert_eq!(first.source.as_deref(), Some("Open Estate Report"));
    first
}

fn set(report: &mut Value, pointer: &str, value: Value) {
    *report
        .pointer_mut(pointer)
        .unwrap_or_else(|| panic!("fixture has no {pointer}")) = value;
}

// ---------------------------------------------------------------- wildfire_zone

#[test]
fn wildfire_zone_from_fixture_reports() {
    for slug in [PRIMARY, SECONDARY] {
        let report = fixture(slug);
        // Both homes are outside every CAL FIRE Fire Hazard Severity Zone (LRA NonWildland) and
        // sit on USFS WHP class 6; the California designation takes precedence.
        assert_eq!(
            report.pointer(&format!("{FHSZ}/inFhsz")),
            Some(&json!(false))
        );
        assert_eq!(report.pointer(&format!("{WHP}/class")), Some(&json!(6)));
        assert_eq!(
            subject(&report).wildfire_zone.as_deref(),
            Some("CA FHSZ none"),
            "{slug}"
        );
    }
}

#[test]
fn wildfire_zone_labels_for_each_fhsz_class() {
    for (class, description, expected) in [
        (1, "Moderate", "CA FHSZ Moderate"),
        (2, "High", "CA FHSZ High"),
        (3, "Very High", "CA FHSZ Very High"),
    ] {
        let mut report = fixture(PRIMARY);
        set(
            &mut report,
            FHSZ,
            json!({
                "responsibilityArea": "SRA",
                "fhsz": description,
                "fhszClass": class,
                "inFhsz": true,
                "source": "SRA map effective 2024-04-01",
                "nearestHighOrVeryHigh": null,
                "searchRadiusM": 5000
            }),
        );
        assert_eq!(subject(&report).wildfire_zone.as_deref(), Some(expected));
    }
}

#[test]
fn wildfire_zone_uses_whp_outside_california() {
    // Outside California the hazards module emits `wildfire.california: null`.
    for (class, label, expected) in [
        (1, "Very Low", "WHP 1 Very Low"),
        (2, "Low", "WHP 2 Low"),
        (3, "Moderate", "WHP 3 Moderate"),
        (4, "High", "WHP 4 High"),
        (5, "Very High", "WHP 5 Very High"),
        (6, "Non-burnable", "WHP 6 Non-burnable"),
        (7, "Water", "WHP 7 Water"),
    ] {
        let mut report = fixture(SECONDARY);
        set(&mut report, FHSZ, Value::Null);
        set(
            &mut report,
            WHP,
            json!({ "class": class, "label": label, "dataset": "USFS Wildfire Hazard Potential 2023 (270 m)" }),
        );
        assert_eq!(subject(&report).wildfire_zone.as_deref(), Some(expected));
    }

    // Neither source answered: unknown, not "none".
    let mut report = fixture(SECONDARY);
    set(&mut report, FHSZ, Value::Null);
    set(&mut report, WHP, Value::Null);
    assert_eq!(subject(&report).wildfire_zone, None);
}

// ---------------------------------------------------------------- wildfire exclusion filter

async fn memory_db() -> DbStore {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .expect("in-memory SQLite");
    db.ensure_schema().await.expect("schema");
    db
}

async fn matching_ids(db: &DbStore, filters: &SearchFilters) -> BTreeSet<String> {
    let q = compile_filters_to_sql(filters, db.dialect(), false, None, 50);
    db.fetch_listings_sql(&q.sql, &q.params)
        .await
        .unwrap_or_else(|e| panic!("query failed: {e}\n{}", q.sql_preview))
        .into_iter()
        .map(|row| row.id)
        .collect()
}

#[test]
fn wildfire_exclusion_sql_is_parameterized_in_both_dialects() {
    let filters = SearchFilters {
        exclude_high_wildfire_risk: true,
        ..Default::default()
    };
    for dialect in ["sqlite", "mysql"] {
        let q = compile_filters_to_sql(&filters, dialect, false, None, 24);
        let sql = q.sql.to_ascii_lowercase();
        assert!(
            sql.contains("wildfire_zone") && sql.contains("not like"),
            "{dialect}: {}",
            q.sql
        );
        assert!(
            !q.sql.contains("%high%"),
            "{dialect}: literal in SQL: {}",
            q.sql
        );
        assert!(
            q.params.contains(&SqlParam::Text("%high%".to_string())),
            "{dialect}: {:?}",
            q.params
        );
    }
}

#[tokio::test]
async fn wildfire_exclusion_filter_handles_new_and_legacy_labels() {
    let db = memory_db().await;
    // (id, wildfire_zone, kept when high wildfire risk is excluded)
    let cases = [
        ("ca-none", Some("CA FHSZ none"), true),
        ("ca-moderate", Some("CA FHSZ Moderate"), true),
        ("ca-high", Some("CA FHSZ High"), false),
        ("ca-very-high", Some("CA FHSZ Very High"), false),
        ("whp-1", Some("WHP 1 Very Low"), true),
        ("whp-2", Some("WHP 2 Low"), true),
        ("whp-3", Some("WHP 3 Moderate"), true),
        ("whp-4", Some("WHP 4 High"), false),
        ("whp-5", Some("WHP 5 Very High"), false),
        ("whp-6", Some("WHP 6 Non-burnable"), true),
        ("whp-7", Some("WHP 7 Water"), true),
        ("legacy-high", Some("High"), false),
        ("legacy-very-high", Some("Very High"), false),
        ("unknown", None, true),
    ];
    let synth_95118 = extract_listings_from_report(&fixture(PRIMARY));
    assert!(
        synth_95118.len() >= cases.len(),
        "expected >= {} synthetic 95118 listings from primary fixture, got {}",
        cases.len(),
        synth_95118.len()
    );
    let items: Vec<ListingInput> = cases
        .iter()
        .zip(synth_95118.into_iter())
        .map(|((id, zone, _), mut row)| {
            row.id = Some((*id).to_string());
            row.zip = "95118".to_string();
            row.status = Some("for_sale".to_string());
            row.wildfire_zone = zone.map(str::to_string);
            row
        })
        .collect();
    upsert_listings_batch(&db, None, &EnvConfig::default(), &items, false)
        .await
        .expect("upsert");

    let mut filters = SearchFilters {
        zips: vec!["95118".to_string()],
        ..Default::default()
    };
    let all: BTreeSet<String> = cases.iter().map(|c| c.0.to_string()).collect();
    assert_eq!(matching_ids(&db, &filters).await, all);

    filters.exclude_high_wildfire_risk = true;
    let kept: BTreeSet<String> = cases
        .iter()
        .filter(|c| c.2)
        .map(|c| c.0.to_string())
        .collect();
    assert_eq!(matching_ids(&db, &filters).await, kept);
}

#[tokio::test]
async fn wildfire_exclusion_on_subjects_harvested_from_reports() {
    let db = memory_db().await;
    let primary = fixture(PRIMARY);
    let mut secondary = fixture(SECONDARY);
    set(
        &mut secondary,
        FHSZ,
        json!({ "responsibilityArea": "LRA", "fhsz": "Very High", "fhszClass": 3, "inFhsz": true }),
    );
    let subjects = vec![subject(&primary), subject(&secondary)];
    assert_eq!(
        subjects[1].wildfire_zone.as_deref(),
        Some("CA FHSZ Very High")
    );
    upsert_listings_batch(&db, None, &EnvConfig::default(), &subjects, false)
        .await
        .expect("upsert");

    let mut filters = SearchFilters {
        zips: vec!["95118".to_string(), "95008".to_string()],
        statuses: ["for_sale", "pending", "sold", "off_market"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    };
    let both: BTreeSet<String> = [PRIMARY, SECONDARY].map(String::from).into();
    assert_eq!(matching_ids(&db, &filters).await, both);

    filters.exclude_high_wildfire_risk = true;
    let only_primary: BTreeSet<String> = [PRIMARY].map(String::from).into();
    assert_eq!(matching_ids(&db, &filters).await, only_primary);
}

// ---------------------------------------------------------------- walk_score

#[test]
fn walk_score_from_fixture_reports() {
    // `facts.walkScore.walk` is used when present.
    assert_eq!(subject(&fixture(PRIMARY)).walk_score, Some(74.0));
    assert_eq!(subject(&fixture(SECONDARY)).walk_score, Some(21.0));
}

#[test]
fn walk_score_falls_back_to_civic_score_and_never_to_epa_index() {
    for (slug, civic_score, epa_index) in [(PRIMARY, 72.0, 14.67), (SECONDARY, 62.0, 12.83)] {
        let mut report = fixture(slug);
        assert_eq!(
            report
                .pointer("/modules/civic/data/walkability/index")
                .and_then(Value::as_f64),
            Some(epa_index)
        );

        // Without a Walk Score, the civic module's 0–100 walkability score is used.
        report["facts"].as_object_mut().unwrap().remove("walkScore");
        assert_eq!(subject(&report).walk_score, Some(civic_score), "{slug}");

        // Without any 0–100 source the field stays empty: the 1–20 EPA index is never copied in.
        report["modules"]["civic"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("scores");
        assert_eq!(subject(&report).walk_score, None, "{slug}");
    }
}

// ---------------------------------------------------------------- verdictTone

/// A copy of the secondary synthetic report whose only module score is feng shui, so `summary.scores.overall` equals `overall`.
fn report_with_overall(overall: i64) -> Value {
    let mut rep = fixture(SECONDARY);
    rep["modules"] = json!({
        "fengshui": { "data": { "scores": { "overall": overall } } }
    });
    rep
}

#[test]
fn verdict_tone_follows_the_verdict_bands() {
    for (overall, tone, verdict) in [
        (54, "watch", "Mixed — proceed with diligence"),
        (68, "watch", "Solid, with specific risks to price in"),
        (75, "good", "Strong overall profile"),
        (40, "bad", "High-risk profile"),
        (74, "watch", "Solid, with specific risks to price in"),
        (62, "watch", "Solid, with specific risks to price in"),
        (50, "watch", "Mixed — proceed with diligence"),
        (49, "bad", "High-risk profile"),
    ] {
        let summary = summarize_report(&report_with_overall(overall));
        assert_eq!(summary["scores"]["overall"], json!(overall));
        assert_eq!(summary["verdictTone"], json!(tone), "overall {overall}");
        assert_eq!(summary["verdict"], json!(verdict), "overall {overall}");
    }
}

#[test]
fn verdict_tone_is_null_without_a_score() {
    let mut geocode_only = fixture(SECONDARY);
    geocode_only["modules"] = json!({});
    let summary = summarize_report(&geocode_only);
    assert!(summary["scores"]["overall"].is_null());
    assert!(summary["verdict"].is_null());
    let obj = summary.as_object().expect("summary object");
    assert_eq!(obj.get("verdictTone"), Some(&Value::Null));
}

#[test]
fn verdict_tone_on_fixture_reports() {
    for slug in [SECONDARY, PRIMARY] {
        let report = common::synth_report_for_slug(slug);
        let summary = summarize_report(&report);
        let overall = summary["scores"]["overall"]
            .as_i64()
            .expect("synthetic report overall score");
        let expected_tone = if overall >= 75 {
            "good"
        } else if overall >= 50 {
            "watch"
        } else {
            "bad"
        };
        assert_eq!(report["summary"]["scores"]["overall"], json!(overall), "{slug}");
        assert_eq!(summary["verdictTone"], json!(expected_tone), "{slug}");
        assert!(summary["verdict"].as_str().is_some_and(|v| !v.is_empty()), "{slug}");
    }
}
