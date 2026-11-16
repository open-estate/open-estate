//! Phase 0 school cleanup: school numbers come only from official public data.
//!
//! Uses procedural synthetic reports constructed from `common` in memory.

mod common;

use open_estate::db::{DbStore, ListingRow};
use open_estate::modules::civic::{
    assigned_official_schools, official_school_score, SCORE_SOURCE_CA_DASHBOARD,
};
use open_estate::search::{
    assign_schools_to_listing, compile_filters_to_sql, extract_listings_from_report,
    get_zip_school_context, score_listing_deterministic, upsert_listings_batch, CachedSchoolEntry,
    ListingInput, SearchFilters, ZipSchoolContext,
};
use open_estate::shared::EnvConfig;
use open_estate::web::summary::{backfill_facts_schools, summarize_report};
use serde_json::{json, Value};

const SECONDARY: &str = common::SYNTH_SECONDARY_SLUG;
const PRIMARY: &str = common::SYNTH_PRIMARY_SLUG;

fn synth_school_fixture(slug: &str) -> Value {
    let is_secondary = slug == SECONDARY;
    let mut rep = common::synth_report_for_slug(slug);
    if let Some(comps) = rep["facts"]["comps"].as_array_mut() {
        for c in comps.iter_mut() {
            c["source"] = json!("County Recorder");
        }
    }

    let cards = if is_secondary {
        json!([
            {
                "name": "Synth Beta Elementary",
                "level": "Elementary",
                "role": "Elementary",
                "grades": "K-5",
                "type": "Public",
                "assigned": true,
                "ncesId": "060000200201",
                "distMi": 0.42,
                "caDashboard": { "year": "2024-25", "elaTier": 4, "mathTier": 4, "sciTier": 4 }
            },
            {
                "name": "Synth Beta Middle",
                "level": "Middle",
                "role": "Middle",
                "grades": "6-8",
                "type": "Public",
                "assigned": true,
                "ncesId": "060000200202",
                "distMi": 0.88,
                "caDashboard": { "year": "2024-25", "elaTier": 5, "mathTier": 5, "sciTier": 5 }
            },
            {
                "name": "Synth Beta High",
                "level": "High",
                "role": "High",
                "grades": "9-12",
                "type": "Public",
                "assigned": true,
                "ncesId": "060000200203",
                "distMi": 1.35,
                "caDashboard": { "year": "2024-25", "elaTier": 4, "mathTier": 4, "sciTier": 4, "cciTier": 5, "gradTier": 4 }
            }
        ])
    } else {
        json!([
            {
                "name": "Synth Alpha Elementary",
                "level": "Elementary",
                "role": "Elementary",
                "grades": "K-5",
                "type": "Public",
                "assigned": true,
                "ncesId": "060000100101",
                "distMi": 0.38,
                "caDashboard": { "year": "2024-25", "elaTier": 5, "mathTier": 5, "sciTier": 5 }
            },
            {
                "name": "Synth Alpha Middle",
                "level": "Middle",
                "role": "Middle",
                "grades": "6-8",
                "type": "Public",
                "assigned": true,
                "ncesId": "060000100102",
                "distMi": 0.79,
                "caDashboard": { "year": "2024-25", "elaTier": 4, "mathTier": 5, "sciTier": 4 }
            },
            {
                "name": "Synth Alpha High",
                "level": "High",
                "role": "High",
                "grades": "9-12",
                "type": "Public",
                "assigned": true,
                "ncesId": "060000100103",
                "distMi": 1.24,
                "caDashboard": { "year": "2024-25", "elaTier": 5, "mathTier": 4, "sciTier": 4, "cciTier": 5, "gradTier": 4 }
            }
        ])
    };

    rep["modules"]["civic"]["data"]["schools"]["assignedOrNearest"] = cards.clone();
    rep["modules"]["civic"]["data"]["schools"]["public"] = json!({ "list": cards });
    let civ_clone = rep["modules"]["civic"]["data"].clone();
    let geo_clone = rep["geo"].clone();
    backfill_facts_schools(&mut rep["facts"], Some(&civ_clone), &geo_clone);
    rep["summary"] = summarize_report(&rep);
    rep
}

fn civic(report: &Value) -> &Value {
    &report["modules"]["civic"]["data"]
}

/// Third-party and legacy values a careless implementation might read: GreatSchools / Niche ratings in
/// `facts.schools.assigned`, plus the non-official fields older civic versions wrote on school objects.
fn poison(report: &Value) -> Value {
    let mut r = report.clone();
    for s in r["facts"]["schools"]["assigned"]
        .as_array_mut()
        .expect("facts.schools.assigned")
    {
        s["rating"] = json!(10);
        s["gsRating"] = json!(10);
        s["nicheGrade"] = json!("A+");
        s["score"] = json!(10);
        s["ela"] = json!(99);
        s["math"] = json!(99);
    }
    let sc = &mut r["modules"]["civic"]["data"]["schools"];
    sc["summaryScore"] = json!(10);
    sc["hudProficiencyIndex"] = json!(100);
    for s in sc["assignedOrNearest"].as_array_mut().unwrap() {
        s["score"] = json!(10);
        s["gsRating"] = json!(10);
        s["nicheGrade"] = json!("A+");
        s["staffingScore"] = json!(9);
        s["readingProficiencyPct"] = json!(99.0);
        s["estGrade"] = json!("A+");
    }
    for s in sc["public"]["list"].as_array_mut().unwrap() {
        s["score"] = json!(10);
        s["gsRating"] = json!(10);
        s["staffingScore"] = json!(9);
        s["readingProficiencyPct"] = json!(99.0);
    }
    r
}

fn school_fields(l: &ListingInput) -> Value {
    json!({
        "score": l.school_score,
        "grade": l.school_grade,
        "elem": [l.school_elem_name, l.school_elem_rating],
        "mid": [l.school_mid_name, l.school_mid_rating],
        "high": [l.school_high_name, l.school_high_rating],
    })
}

fn school_badge(report: &Value) -> Option<Value> {
    summarize_report(report)["badges"]
        .as_array()
        .and_then(|b| b.iter().find(|x| x["k"] == "schools").cloned())
}

fn approx(a: Option<f64>, b: f64) -> bool {
    a.map(|x| (x - b).abs() < 0.051).unwrap_or(false)
}

#[test]
fn official_scores_for_the_six_assigned_schools() {
    let expected = [
        (
            SECONDARY,
            [
                ("Synth Beta Elementary", 8.0),
                ("Synth Beta Middle", 10.0),
                ("Synth Beta High", 8.4),
            ],
        ),
        (
            PRIMARY,
            [
                ("Synth Alpha Elementary", 10.0),
                ("Synth Alpha Middle", 8.7),
                ("Synth Alpha High", 8.8),
            ],
        ),
    ];
    for (file, schools) in expected {
        let report = synth_school_fixture(file);
        let assigned = assigned_official_schools(civic(&report));
        let got: Vec<_> = assigned.iter().collect();
        assert_eq!(got.len(), 3, "{file}: one assigned school per level");
        for ((name, score), a) in schools.iter().zip(got) {
            assert_eq!(&a.name, name, "{file}");
            let o = a
                .official
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: official score"));
            assert!(
                approx(Some(o.score), *score),
                "{name}: got {}, want {score}",
                o.score
            );
            assert_eq!(o.source, SCORE_SOURCE_CA_DASHBOARD);
            assert_eq!(o.year, "2024-25");
        }
        let cards = civic(&report)["schools"]["assignedOrNearest"]
            .as_array()
            .unwrap();
        for c in cards {
            let o = official_school_score(c).expect("card has Dashboard tiers");
            assert!(o.score <= 10.0 && o.score >= 2.0);
        }
    }
    let alpha_mid = assigned_official_schools(civic(&synth_school_fixture(PRIMARY)))
        .middle
        .unwrap()
        .official
        .unwrap();
    assert_eq!(alpha_mid.score, 8.7, "mean(4, 5, 4) × 2 = 8.67 → 8.7");
}

#[test]
fn listing_school_scores_come_from_official_data() {
    for (file, ratings, mean, grade) in [
        (SECONDARY, [8.0, 10.0, 8.4], 8.8, "A"),
        (PRIMARY, [10.0, 8.7, 8.8], 9.2, "A+"),
    ] {
        let report = synth_school_fixture(file);
        let listings = extract_listings_from_report(&report);
        assert!(!listings.is_empty(), "{file}");
        let subject = &listings[0];
        assert!(
            approx(subject.school_elem_rating, ratings[0]),
            "{file}: {:?}",
            subject.school_elem_rating
        );
        assert!(
            approx(subject.school_mid_rating, ratings[1]),
            "{file}: {:?}",
            subject.school_mid_rating
        );
        assert!(
            approx(subject.school_high_rating, ratings[2]),
            "{file}: {:?}",
            subject.school_high_rating
        );
        assert!(
            approx(subject.school_score, mean),
            "{file}: {:?}",
            subject.school_score
        );
        assert_eq!(subject.school_grade.as_deref(), Some(grade), "{file}");
        // Nearby listings and comps harvested from the same report share the same official values.
        for l in &listings {
            assert_eq!(
                school_fields(l),
                school_fields(subject),
                "{file}: {}",
                l.address
            );
        }
    }
}

#[test]
fn poisoned_third_party_ratings_change_nothing() {
    for file in [SECONDARY, PRIMARY] {
        let clean = synth_school_fixture(file);
        let poisoned = poison(&clean);
        let a = extract_listings_from_report(&clean);
        let b = extract_listings_from_report(&poisoned);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(school_fields(x), school_fields(y), "{file}: {}", x.address);
        }
        let badge_clean = school_badge(&clean).expect("school badge");
        let badge_poisoned = school_badge(&poisoned).expect("school badge");
        assert_eq!(badge_clean, badge_poisoned, "{file}");
        assert_eq!(
            assigned_official_schools(civic(&clean)),
            assigned_official_schools(civic(&poisoned)),
            "{file}"
        );
    }
}

#[test]
fn school_badge_is_official_and_shows_source_and_year() {
    for (file, score) in [(SECONDARY, 8.8), (PRIMARY, 9.2)] {
        let badge = school_badge(&synth_school_fixture(file)).expect("school badge");
        assert!(approx(badge["score"].as_f64(), score), "{file}: {badge}");
        assert_eq!(badge["source"], json!(SCORE_SOURCE_CA_DASHBOARD));
        assert_eq!(badge["year"], json!("2024-25"));
        let value = badge["value"].as_str().unwrap();
        assert!(value.starts_with(&format!("{score:.1}/10")), "{value}");
        assert!(
            value.contains("California School Dashboard (CDE)") && value.contains("2024-25"),
            "{value}"
        );
    }
}

#[test]
fn no_official_data_means_null_never_a_rating() {
    // Only GreatSchools / Niche values from web research, no civic module: everything stays unknown.
    let mut report = synth_school_fixture(PRIMARY);
    report["modules"].as_object_mut().unwrap().remove("civic");
    let report = poison_facts_only(report);
    let listings = extract_listings_from_report(&report);
    let subject = &listings[0];
    assert_eq!(subject.school_score, None);
    assert_eq!(subject.school_grade, None);
    assert_eq!(subject.school_elem_rating, None);
    assert_eq!(subject.school_mid_rating, None);
    assert_eq!(subject.school_high_rating, None);
    // Names are facts and may still be listed.
    assert_eq!(
        subject.school_elem_name.as_deref(),
        Some("Synthetic Elementary School")
    );
    assert!(
        school_badge(&report).is_none(),
        "no badge without official data"
    );

    // Civic schools without official academic data (e.g. outside California) are unknown too.
    let mut report = synth_school_fixture(SECONDARY);
    for s in report["modules"]["civic"]["data"]["schools"]["assignedOrNearest"]
        .as_array_mut()
        .unwrap()
    {
        s.as_object_mut().unwrap().remove("caDashboard");
    }
    let listings = extract_listings_from_report(&report);
    assert_eq!(listings[0].school_score, None);
    assert_eq!(
        listings[0].school_elem_name.as_deref(),
        Some("Synth Beta Elementary")
    );
    assert!(school_badge(&report).is_none());
}

fn poison_facts_only(mut r: Value) -> Value {
    for s in r["facts"]["schools"]["assigned"].as_array_mut().unwrap() {
        s["rating"] = json!(10);
        s["nicheGrade"] = json!("A+");
    }
    r
}

#[test]
fn backfilled_facts_carry_official_scores_only() {
    let report = poison(&synth_school_fixture(SECONDARY));
    let mut facts = json!({});
    backfill_facts_schools(&mut facts, Some(civic(&report)), &report["geo"]);
    let sc = &facts["schools"];
    assert!(approx(sc["summaryScore"].as_f64(), 8.8), "{sc}");
    assert_eq!(sc["summaryScoreSource"], json!(SCORE_SOURCE_CA_DASHBOARD));
    assert_eq!(sc["summaryScoreYear"], json!("2024-25"));
    assert!(sc["noteZh"].as_str().is_some());
    let assigned = sc["assigned"].as_array().unwrap();
    assert_eq!(assigned.len(), 3);
    for (a, want) in assigned.iter().zip([8.0, 10.0, 8.4]) {
        assert!(approx(a["score"].as_f64(), want), "{a}");
        assert_eq!(a["scoreSource"], json!(SCORE_SOURCE_CA_DASHBOARD));
        assert_eq!(a["scoreYear"], json!("2024-25"));
        for k in [
            "rating",
            "gsRating",
            "nicheGrade",
            "staffingScore",
            "readingProficiencyPct",
            "estGrade",
        ] {
            assert!(
                a.get(k).is_none(),
                "backfilled entry must not carry {k}: {a}"
            );
        }
    }
}

fn listing_row(id: &str, school_score: Option<f64>) -> ListingRow {
    let subject = extract_listings_from_report(&synth_school_fixture(PRIMARY))
        .into_iter()
        .next()
        .expect("synthetic San Jose subject listing");
    serde_json::from_value(json!({
        "id": id,
        "address": subject.address,
        "zip": subject.zip,
        "status": "for_sale",
        "price": subject.price.unwrap_or(2_320_000),
        "beds": subject.beds.unwrap_or(4.0),
        "school_score": school_score,
        "school_grade": school_score.and_then(|s| open_estate::db::score_to_school_grade(Some(s))),
        "first_seen_at": "2026-11-11T00:00:00Z",
        "last_seen_at": "2026-11-11T00:00:00Z",
        "status_changed_at": "2026-11-11T00:00:00Z",
    }))
    .expect("ListingRow")
}

#[test]
fn unknown_school_data_is_kept_ranked_below_known_matches_and_explained() {
    let filters = SearchFilters {
        zips: vec!["95118".to_string()],
        min_school_score: Some(8.0),
        min_school_grade: Some("A".to_string()),
        ..Default::default()
    };
    let known = score_listing_deterministic(&listing_row("known", Some(9.1)), &filters);
    let unknown = score_listing_deterministic(&listing_row("unknown", None), &filters);
    let below = score_listing_deterministic(&listing_row("below", Some(5.0)), &filters);
    assert!(
        known.match_score > unknown.match_score,
        "{known:?} vs {unknown:?}"
    );
    assert!(
        unknown.match_score > below.match_score,
        "{unknown:?} vs {below:?}"
    );
    let expl = format!(
        "{} {}",
        unknown.why_it_matches,
        unknown.tradeoffs.clone().unwrap_or_default()
    );
    assert!(expl.contains("school data unavailable"), "{expl}");
    assert!(!known
        .tradeoffs
        .clone()
        .unwrap_or_default()
        .contains("school data unavailable"));

    for dialect in ["sqlite", "mysql"] {
        let sql = compile_filters_to_sql(&filters, dialect, false, None, 24).sql;
        assert!(
            sql.contains("IS NULL"),
            "{dialect}: unknown school data must not be filtered out: {sql}"
        );
        let sorted = compile_filters_to_sql(
            &SearchFilters {
                sort_by: "school_desc".into(),
                ..filters.clone()
            },
            dialect,
            false,
            None,
            24,
        )
        .sql;
        assert!(
            sorted.contains("CASE WHEN school_score IS NULL THEN 1 ELSE 0 END"),
            "{sorted}"
        );
    }
}

#[tokio::test]
async fn school_filter_keeps_unknown_listings_in_sqlite() {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();
    let env = EnvConfig::default();
    let mut synth_listings = extract_listings_from_report(&synth_school_fixture(PRIMARY));
    assert!(
        synth_listings.len() >= 3,
        "expected >= 3 synthetic listings in primary report"
    );
    let mut items: Vec<ListingInput> = synth_listings.drain(..3).collect();
    for (row, score) in items.iter_mut().zip([Some(9.0), None, Some(5.5)]) {
        row.zip = "95118".to_string();
        row.status = Some("for_sale".to_string());
        row.school_score = score;
        row.school_grade = open_estate::db::score_to_school_grade(score);
        row.school_elem_rating = None;
        row.school_mid_rating = None;
        row.school_high_rating = None;
    }
    let expected_addresses = vec![items[0].address.clone(), items[1].address.clone()];
    upsert_listings_batch(&db, None, &env, &items, false)
        .await
        .unwrap();

    let filters = SearchFilters {
        zips: vec!["95118".to_string()],
        min_school_score: Some(8.0),
        sort_by: "school_desc".to_string(),
        ..Default::default()
    };
    let q = compile_filters_to_sql(&filters, db.dialect(), false, None, 24);
    let rows = db.fetch_listings_sql(&q.sql, &q.params).await.unwrap();
    let got_addresses: Vec<_> = rows.iter().map(|r| r.address.clone()).collect();
    assert_eq!(
        got_addresses, expected_addresses,
        "unknown kept, after the known match"
    );
}

#[tokio::test]
async fn zip_school_context_never_rates_from_staffing() {
    // A cache row written by legacy code carrying student–teacher-ratio "ratings".
    let legacy = json!([
        { "nces_id": "060000100101", "name": "Synth Alpha Elementary", "district": "Synthetic Union Elementary", "level": "elementary", "lat": 37.255, "lon": -121.899, "rating": 7.8, "rating_source": "nces_staffing_proxy" },
        { "nces_id": "060000100103", "name": "Synth Alpha High", "district": "Synthetic Union High", "level": "high", "lat": 37.260, "lon": -121.897, "rating": 8.5, "rating_source": "nces_staffing_proxy" }
    ]);
    let entries: Vec<CachedSchoolEntry> = serde_json::from_value(legacy.clone()).unwrap();
    assert!(entries
        .iter()
        .all(|e| e.rating.is_none() && e.rating_source.is_none()));
    let reserialized = serde_json::to_string(&entries).unwrap();
    assert!(!reserialized.contains("rating"), "{reserialized}");

    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();
    db.upsert_row(
        "zip_school_cache",
        &[
            ("zip", "95118".into()),
            ("lat", Some(37.258).into()),
            ("lon", Some(-121.898).into()),
            ("schools_json", legacy.to_string().into()),
            ("updated_at", open_estate::shared::now_iso().into()),
        ],
        &["zip"],
    )
    .await
    .unwrap();
    let ctx = get_zip_school_context(&db, None, &EnvConfig::default(), "95118", None, None)
        .await
        .unwrap();
    assert_eq!(ctx.schools.len(), 2);
    assert!(
        ctx.schools.iter().all(|s| s.rating.is_none()),
        "{:?}",
        ctx.schools
    );

    let primary_subject = extract_listings_from_report(&synth_school_fixture(PRIMARY))
        .into_iter()
        .next()
        .expect("synthetic subject listing");
    let mut item = ListingInput {
        address: primary_subject.address,
        zip: primary_subject.zip,
        lat: primary_subject.lat,
        lon: primary_subject.lon,
        ..Default::default()
    };
    assign_schools_to_listing(&mut item, Some(&ctx));
    assert_eq!(item.school_elem_name.as_deref(), Some("Synth Alpha Elementary"));
    assert_eq!(item.school_high_name.as_deref(), Some("Synth Alpha High"));
    assert_eq!(item.school_score, None, "no official data → unknown");
    assert_eq!(item.school_grade, None);
    assert_eq!(item.school_elem_rating, None);

    let empty = ZipSchoolContext::default();
    let mut other = item.clone();
    assign_schools_to_listing(&mut other, Some(&empty));
    assert_eq!(other.school_score, None);
}

#[test]
fn report_template_never_scores_third_party_ratings() {
    let tpl = std::fs::read_to_string(format!(
        "{}/assets/template.html",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    for banned in [
        "hudProficiencyIndex",
        "readingProficiencyPct",
        "staffingScore",
        "sdRankStars",
        "a.rating ?? a.gsRating",
        "s.gsRating ??",
        "(GS / Niche)",
    ] {
        assert!(!tpl.contains(banned), "template still references {banned}");
    }
    for required in [
        "function officialSchoolScore(",
        "California School Dashboard (CDE)",
        "metricSource",
        "metricYear",
    ] {
        assert!(tpl.contains(required), "template must contain {required}");
    }
}
