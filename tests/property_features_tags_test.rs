mod common;

use open_estate::modules::property_features::{
    attach_property_features_and_tags, build_property_features_and_tags, encode_w3c_text_fragment,
};
use open_estate::web::render::{
    normalize_stored_report, render_report_html, sanitize_report_for_html,
};
use open_estate::web::summary::summarize_report;
use serde_json::{json, Value};
use std::collections::HashSet;

const FORBIDDEN_HTML_TOKENS: &[&str] = &[
    "gpt-4o-mini",
    "gpt-4.1",
    "rust-sqlx-sqlite",
    "fallbackUsed",
    "deterministic-fallback",
    "max reasoning",
    "aiModel",
    "OPENAI_API_KEY",
    "effort",
    "provider",
];

fn load_fixture(slug: &str) -> Value {
    common::synth_report_for_slug(slug)
}

fn load_holdout_home(id: &str) -> Value {
    common::synth_holdout_by_id(id)
}

fn assert_clean_and_valid_urls(pf: &Value, tags: &Value, ctx: &str) {
    let dumped = serde_json::to_string(pf).unwrap();
    assert!(
        !open_estate::shared::has_retired_brand(&dumped),
        "{ctx} leaked retired brand"
    );
    for tok in FORBIDDEN_HTML_TOKENS {
        assert!(
            !dumped.contains(tok),
            "{ctx} leaked forbidden token `{tok}` in propertyFeatures"
        );
    }

    let tag_arr = tags.as_array().expect("propertyTags array");
    assert_eq!(pf["tags"], *tags, "{ctx} pf.tags must match propertyTags");
    assert!(
        (6..=14).contains(&tag_arr.len()),
        "{ctx} expected 6..=14 tags, got {}",
        tag_arr.len()
    );
    let mut seen = HashSet::new();
    for t in tag_arr {
        let id = t["id"].as_str().expect("tag id");
        assert!(seen.insert(id.to_string()), "{ctx} duplicate tag id {id}");
        assert!(!t["label"].as_str().unwrap_or("").is_empty());
        assert!(!t["icon"].as_str().unwrap_or("").is_empty());
        assert!(matches!(
            t["category"].as_str(),
            Some("energy" | "layout_adu" | "interior" | "outdoor_lot" | "systems_ownership")
        ));
        assert!(matches!(
            t["tone"].as_str(),
            Some("emerald" | "blue" | "amber" | "slate")
        ));
        assert!(matches!(
            t["status"].as_str(),
            Some("confirmed" | "permitted" | "potential" | "watchout")
        ));
        let url = t["sourceUrl"].as_str().expect("tag sourceUrl");
        assert!(
            url.starts_with("https://") && !url.contains(' '),
            "{ctx} invalid tag sourceUrl: {url}"
        );
    }

    let cats = pf["categories"].as_array().expect("categories array");
    assert_eq!(cats.len(), 5, "{ctx} must have 5 categories");
    let expected_keys = [
        "energy",
        "layout_adu",
        "interior",
        "outdoor_lot",
        "systems_ownership",
    ];
    for (idx, ek) in expected_keys.iter().enumerate() {
        assert_eq!(cats[idx]["key"].as_str(), Some(*ek));
        let items = cats[idx]["items"].as_array().expect("category items");
        assert!(!items.is_empty());
        let mut active = 0usize;
        for it in items {
            let st = it["status"].as_str().expect("item status");
            assert!(matches!(
                st,
                "confirmed" | "permitted" | "not_detected" | "watchout" | "opportunity"
            ));
            if st != "not_detected" {
                active += 1;
            }
            let url = it["sourceUrl"].as_str().expect("item sourceUrl");
            assert!(
                url.starts_with("https://") && !url.contains(' '),
                "{ctx} invalid item sourceUrl on {}: {url}",
                it["id"]
            );
        }
        assert_eq!(cats[idx]["detectedCount"].as_u64(), Some(active as u64));
    }

    let qs = pf["buyerQuestionsToVerify"]
        .as_array()
        .expect("buyerQuestionsToVerify");
    assert!(
        (2..=5).contains(&qs.len()),
        "{ctx} expected 2..=5 buyerQuestionsToVerify, got {}",
        qs.len()
    );
}

#[test]
fn synthetic_secondary_and_primary_fixtures_populate_accurate_features_and_tags() {
    // 1. Secondary synthetic home (1-story, Permitted 600 sqft ADU, Solar PV, oversized 10,200 sqft lot, No HOA)
    let mut rep_sec = load_fixture(common::SYNTH_SECONDARY_SLUG);
    rep_sec["facts"]["lotSqft"] = json!(10_200);
    attach_property_features_and_tags(&mut rep_sec);
    assert_clean_and_valid_urls(
        &rep_sec["propertyFeatures"],
        &rep_sec["propertyTags"],
        "synth-secondary",
    );

    let sum_sec = &rep_sec["propertyFeatures"]["energyAndEquipmentSummary"];
    assert_eq!(sum_sec["hasSolar"], json!(true));
    assert_ne!(
        sum_sec["solarOwnership"],
        json!("leased_or_ppa"),
        "diligenceExtra 'owned vs. PPA/lease' prompt must NEVER misclassify solar as leased"
    );
    assert_eq!(sum_sec["hasPermittedAdu"], json!(true));
    assert_eq!(sum_sec["isSingleStory"], json!(true));
    assert_eq!(sum_sec["hasCentralAc"], json!(true));
    assert_eq!(sum_sec["hasNoHoa"], json!(true));
    assert_eq!(sum_sec["hasPool"], json!(false));

    let sec_tag_ids: Vec<&str> = rep_sec["propertyTags"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert!(sec_tag_ids.contains(&"permitted_adu"), "tags={sec_tag_ids:?}");
    assert!(sec_tag_ids.contains(&"solar_pv"), "tags={sec_tag_ids:?}");
    assert!(sec_tag_ids.contains(&"single_story"), "tags={sec_tag_ids:?}");
    assert!(sec_tag_ids.contains(&"lot_character"), "tags={sec_tag_ids:?}");
    assert!(sec_tag_ids.contains(&"no_hoa"), "tags={sec_tag_ids:?}");

    // 2. Primary synthetic home (2-story, ground-floor primary suite, Central A/C, Walkability 74, ADU-ready lot)
    let mut rep_pri = load_fixture(common::SYNTH_PRIMARY_SLUG);
    rep_pri["facts"]["features"] = json!([
        "Ground-floor primary bedroom suite",
        "Hardwood flooring",
        "Dual-pane windows",
        "Smart thermostat"
    ]);
    rep_pri["modules"]["civic"]["data"]["scores"]["walkability"] = json!(74);
    normalize_stored_report(&mut rep_pri);

    let summary_out = summarize_report(&rep_pri);
    assert!(summary_out["propertyTags"].as_array().unwrap().len() >= 6);

    let html_pri = render_report_html(&rep_pri);
    for tok in FORBIDDEN_HTML_TOKENS {
        assert!(!html_pri.contains(tok), "HTML leaked `{tok}`");
    }
    assert!(html_pri.contains("\"propertyFeatures\""));
    assert!(html_pri.contains("\"propertyTags\""));

    assert_clean_and_valid_urls(
        &rep_pri["propertyFeatures"],
        &rep_pri["propertyTags"],
        "synth-primary",
    );
    let sum_pri = &rep_pri["propertyFeatures"]["energyAndEquipmentSummary"];
    assert_eq!(sum_pri["isSingleStory"], json!(false));
    assert_eq!(sum_pri["hasCentralAc"], json!(true));
    assert_eq!(sum_pri["hasNoHoa"], json!(true));

    let pri_tag_ids: Vec<&str> = rep_pri["propertyTags"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert!(
        pri_tag_ids.contains(&"main_level_suite"),
        "tags={pri_tag_ids:?}"
    );
    assert!(
        pri_tag_ids.contains(&"walkable_top_schools"),
        "tags={pri_tag_ids:?}"
    );
    assert!(pri_tag_ids.contains(&"adu_potential"), "tags={pri_tag_ids:?}");
}

#[test]
fn detects_powerwall_owned_vs_ucc1_leased_solar_ev_200a_heat_pump_seismic_and_sprinkler_guard() {
    let pa = load_holdout_home("synth-ca-94303-upper-sfr");
    let mut clean_energy_home = json!({
        "meta": {
            "displayAddress": pa["address"],
            "generatedAt": "2026-11-12T00:00:00Z"
        },
        "geo": pa["raw"]["geo"],
        "facts": pa["raw"]["facts"],
    });
    clean_energy_home["facts"]["apn"] = json!("000-00-300");
    clean_energy_home["facts"]["stories"] = json!(1);
    clean_energy_home["facts"]["hoaMonthly"] = json!(0);
    clean_energy_home["facts"]["garage"] = json!("2-car attached garage");
    clean_energy_home["facts"]["cooling"] = json!("Heat pump multi-zone");
    clean_energy_home["facts"]["heating"] = json!("Electric heat pump");
    clean_energy_home["facts"]["poolFeatures"] = json!("Heated saltwater in-ground pool");
    clean_energy_home["facts"]["view"] = json!("Panoramic foothills view");
    clean_energy_home["facts"]["features"] = json!([
        "Cul-de-sac court location",
        "Dedicated home office",
        "Dual-pane windows & induction cooktop",
        "Automatic lawn sprinklers in yard"
    ]);
    clean_energy_home["facts"]["permits"] = json!([
        { "number": "SYNTH-2023-10482", "date": "2023-08-14", "status": "Finaled", "description": "8.4 kW rooftop solar photovoltaic + 2x Tesla Powerwall 2 energy storage", "sourceUrl": "https://permits.cityofpaloalto.org/record/SYNTH-2023-10482" },
        { "number": "SYNTH-2022-04110", "date": "2022-05-03", "status": "Finaled", "description": "200-amp main electrical panel upgrade & Tesla Wall Connector Level 2 EV charger" },
        { "number": "SYNTH-2021-01920", "date": "2021-03-19", "status": "Finaled", "description": "Earthquake Brace & Bolt seismic foundation retrofit & whole-house PEX repipe" },
        { "number": "SYNTH-2024-00811", "date": "2024-02-10", "status": "Finaled", "description": "Heat pump water heater (HPWH) installation" }
    ]);
    clean_energy_home["facts"]["countyRecords"] = json!({
        "countyName": "Santa Clara County",
        "assessorUrl": "https://www.sccassessor.org/property-information",
        "permitPortalUrl": "https://permits.cityofpaloalto.org/search"
    });
    clean_energy_home["buyerDiligence"] = json!({
        "connectivity": {
            "fcc": {
                "providers": [
                    { "name": "Sonic Fiber", "technology": "Fiber", "maxDownloadMbps": 10000 }
                ]
            }
        }
    });

    let pf = build_property_features_and_tags(&clean_energy_home);
    assert_clean_and_valid_urls(&pf, &pf["tags"], "synth-94303-powerwall");

    let eq = &pf["energyAndEquipmentSummary"];
    assert_eq!(eq["hasSolar"], json!(true));
    assert_eq!(eq["solarOwnership"], json!("owned"));
    assert_eq!(eq["hasBatteryBackup"], json!(true));
    assert_eq!(eq["batteryBrand"], json!("Tesla Powerwall"));
    assert_eq!(eq["hasEvCharger"], json!(true));
    assert_eq!(eq["has200AmpPanel"], json!(true));
    assert_eq!(eq["hasHeatPump"], json!(true));
    assert_eq!(eq["hasCentralAc"], json!(true));
    assert_eq!(eq["isSingleStory"], json!(true));
    assert_eq!(eq["hasPool"], json!(true));
    assert_eq!(eq["hasNoHoa"], json!(true));

    // Top tag must be tesla_powerwall with deep permit anchor URL
    let first_tag = &pf["tags"][0];
    assert_eq!(first_tag["id"], json!("tesla_powerwall"));
    assert_eq!(first_tag["label"], json!("Tesla Powerwall"));
    assert_eq!(first_tag["tone"], json!("emerald"));
    assert!(first_tag["sourceUrl"]
        .as_str()
        .unwrap()
        .contains("SYNTH-2023-10482"));

    // Verify lawn sprinklers did NOT falsely trigger smart_water_security / fire sprinklers
    let interior_items = pf["categories"][2]["items"].as_array().unwrap();
    let sws = interior_items
        .iter()
        .find(|i| i["id"] == "smart_water_security")
        .unwrap();
    assert_eq!(
        sws["status"],
        json!("not_detected"),
        "automatic lawn sprinklers must NEVER be misclassified as interior fire sprinklers"
    );

    // Verify Leased / UCC-1 solar watchout detection + W3C fragment encoding of commas & leading/trailing hyphens
    assert_eq!(
        encode_w3c_text_fragment("-2,053 sq ft-"),
        "%2D2%2C053%20sq%20ft%2D"
    );

    // Clone primary synthetic fixture and attach UCC-1 filing + Enphase battery in-test
    let mut leased_solar_home = load_fixture(common::SYNTH_PRIMARY_SLUG);
    if let Some(obj) = leased_solar_home.as_object_mut() {
        obj.remove("propertyFeatures");
        obj.remove("propertyTags");
    }
    leased_solar_home["facts"]["features"] = json!([
        "Enphase IQ Battery backup",
        "Hardwood floors & fireplace",
        "Pergola & outdoor kitchen"
    ]);
    leased_solar_home["facts"]["titleAndLiens"] = json!({
        "uccFilings": [
            { "type": "UCC-1 Solar Financing Statement", "creditor": "Sunrun Inc.", "note": "20-year residential solar PPA fixture filing" }
        ]
    });
    let clean_leased = sanitize_report_for_html(&leased_solar_home);
    let pf_leased = &clean_leased["propertyFeatures"];
    assert_eq!(
        pf_leased["energyAndEquipmentSummary"]["hasSolar"],
        json!(true)
    );
    assert_eq!(
        pf_leased["energyAndEquipmentSummary"]["solarOwnership"],
        json!("leased_or_ppa")
    );
    assert_eq!(
        pf_leased["energyAndEquipmentSummary"]["batteryBrand"],
        json!("Enphase IQ Battery")
    );
    let solar_item = &pf_leased["categories"][0]["items"][0];
    assert_eq!(solar_item["id"], json!("solar_pv"));
    assert_eq!(solar_item["status"], json!("watchout"));
    let watch_tag = pf_leased["tags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "solar_leased_watchout")
        .expect("solar_leased_watchout tag");
    assert_eq!(watch_tag["tone"], json!("amber"));
    assert_eq!(watch_tag["status"], json!("watchout"));
}

#[test]
fn detects_all_battery_brands_generator_fire_sprinklers_official_school_and_strips_pii() {
    for (feature_txt, expected_brand, expected_tag_id) in [
        (
            "FranklinWH aPower whole-home battery",
            "FranklinWH Battery",
            "franklinwh_battery",
        ),
        (
            "Generac PWRcell solar battery cabinet",
            "Generac PWRcell",
            "generac_pwrcell",
        ),
        (
            "LG RESU10H energy storage battery",
            "LG RESU Battery",
            "lg_resu_battery",
        ),
    ] {
        let mut rep = load_fixture(common::SYNTH_PRIMARY_SLUG);
        rep["facts"]["stories"] = json!(1);
        rep["facts"]["features"] = json!([
            feature_txt,
            "Kohler standby generator with automatic transfer switch",
            "Navien tankless water heater",
            "NFPA 13D interior fire sprinkler system",
            "Class-A fire-rated standing seam metal roof & defensible space"
        ]);
        rep["facts"]["countyRawAttributes"] = json!({
            "OWNER_NAME": "REDACTED_ASSESSOR_OWNER",
            "MAIL_ADDRESS": "REDACTED_MAILING_RECORD"
        });
        if let Some(arr) = rep
            .pointer_mut("/modules/civic/data/schools/assignedOrNearest")
            .and_then(Value::as_array_mut)
        {
            if let Some(elem) = arr.first_mut() {
                elem["ncesId"] = json!("060000001001");
                elem["percentile"] = json!(91.0);
                elem["metricSource"] = json!("CAASPP");
                elem["metricYear"] = json!("2024-25");
            }
        }
        let pf = build_property_features_and_tags(&rep);
        assert_clean_and_valid_urls(&pf, &pf["tags"], expected_tag_id);
        let raw = serde_json::to_string(&pf).unwrap();
        assert!(
            !raw.contains("REDACTED_ASSESSOR_OWNER"),
            "PII owner name must never appear"
        );
        assert!(
            !raw.contains("REDACTED_MAILING_RECORD"),
            "PII mail address must never appear"
        );

        assert_eq!(
            pf["energyAndEquipmentSummary"]["hasBatteryBackup"],
            json!(true)
        );
        assert_eq!(
            pf["energyAndEquipmentSummary"]["batteryBrand"],
            json!(expected_brand)
        );
        assert_eq!(pf["tags"][0]["id"], json!(expected_tag_id));

        let energy_items = pf["categories"][0]["items"].as_array().unwrap();
        assert_eq!(
            energy_items
                .iter()
                .find(|i| i["id"] == "backup_generator")
                .unwrap()["status"],
            json!("confirmed")
        );
        assert_eq!(
            energy_items
                .iter()
                .find(|i| i["id"] == "water_heater")
                .unwrap()["status"],
            json!("confirmed")
        );

        let interior_items = pf["categories"][2]["items"].as_array().unwrap();
        let sws = interior_items
            .iter()
            .find(|i| i["id"] == "smart_water_security")
            .unwrap();
        assert_eq!(sws["status"], json!("confirmed"));
        assert_eq!(sws["badge"], json!("Interior Fire Sprinklers"));

        let sys_items = pf["categories"][4]["items"].as_array().unwrap();
        assert_eq!(
            sys_items
                .iter()
                .find(|i| i["id"] == "fire_wind_hardening")
                .unwrap()["status"],
            json!("confirmed")
        );
        let sch = sys_items
            .iter()
            .find(|i| i["id"] == "walkable_top_schools")
            .unwrap();
        assert_eq!(sch["status"], json!("confirmed"));
        assert!(
            sch["sourceUrl"]
                .as_str()
                .unwrap()
                .contains("060000001001"),
            "unexpected school sourceUrl: {}",
            sch["sourceUrl"]
        );
    }
}
