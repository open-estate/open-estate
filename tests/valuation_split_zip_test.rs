#![recursion_limit = "512"]

mod common;

use open_estate::property::{build_facts_and_mls, run_valuation};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::normalize_stored_report;
use serde_json::{json, Value};

/// Strip sale/comp/AVM signals from a synthetic holdout parcel to test zero-comp tract-calibrated valuation.
fn strip_sales_for_zero_comp_test(home: &Value) -> (Value, Value, Value) {
    let mut facts = home["raw"]["facts"].clone();
    facts["comps"] = json!([]);
    facts["avms"] = json!([]);
    facts["saleHistory"] = json!([]);
    facts["lastSale"] = Value::Null;
    facts["price"] = Value::Null;
    facts["soldPrice"] = Value::Null;
    facts["listPrice"] = Value::Null;
    if let Some(obj) = facts.as_object_mut() {
        obj.remove("mlsListing");
        obj.remove("nearbyListings");
    }
    let market = home["raw"]["market"].clone();
    let geo = home["raw"]["geo"].clone();
    (facts, market, geo)
}

fn assert_split_zip_94025_zero_comp_calibrated(
    facts: &Value,
    market: &Value,
    geo: &Value,
    label: &str,
) {
    let out = run_valuation(facts, market, geo);
    let d = &out.data;
    let est = d.get("estimate").expect("estimate present");
    let val = est["value"].as_i64().expect("estimate.value");
    let low = est["low"].as_i64().expect("estimate.low");
    let high = est["high"].as_i64().expect("estimate.high");
    let ppsf = est["pricePerSqft"].as_i64().expect("estimate.pricePerSqft");
    let conf = est["confidence"].as_str().expect("estimate.confidence");
    let conf_score = est["confidenceScore"]
        .as_i64()
        .expect("estimate.confidenceScore");

    assert!(
        (1_050_000..=1_800_000).contains(&val),
        "[{label}] expected tract-calibrated split-ZIP value in $1.05M–$1.80M, got ${val}"
    );
    assert!(
        low < val && val < high,
        "[{label}] expected low ({low}) < val ({val}) < high ({high})"
    );
    assert!(
        (high - low) as f64 / (val as f64) >= 0.25,
        "[{label}] expected wide honest uncertainty band without comps (low={low}, val={val}, high={high})"
    );
    assert!(
        (700..=1_300).contains(&ppsf),
        "[{label}] expected reasonable split-ZIP $/sqft, got {ppsf}"
    );
    assert_eq!(
        conf, "Low",
        "[{label}] expected Low confidence with zero verified sales/AVMs"
    );
    assert!(
        conf_score <= 45,
        "[{label}] expected confidenceScore <= 45 without comps, got {conf_score}"
    );

    assert_eq!(
        d["comps"].as_array().map(|a| a.len()),
        Some(0),
        "[{label}] comps must be empty when no verified sales exist"
    );
    assert_eq!(
        d["compsAnalysis"]["count"].as_u64(),
        Some(0),
        "[{label}] compsAnalysis.count must be 0"
    );
    assert_eq!(
        d["compsAnalysis"]
            .get("syntheticExcludedCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        0,
        "[{label}] must have zero synthetic comps"
    );

    let methods = d["methods"].as_array().expect("methods array");
    assert!(
        !methods.iter().any(|m| m["key"] == json!("comps")),
        "[{label}] must not emit 'comps' method without verified sales"
    );
    assert!(
        methods
            .iter()
            .any(|m| m["key"] == json!("zip-zhvi-calibrated")),
        "[{label}] expected tract-localized zip-zhvi-calibrated method"
    );
    assert!(
        methods.iter().any(|m| m["key"] == json!("tract-acs")),
        "[{label}] expected tract-acs method"
    );
    assert_eq!(
        est["tractCalibration"]["applied"],
        json!(true),
        "[{label}] expected tractCalibration.applied = true"
    );
    assert_eq!(
        est["tractCalibration"]["zipTopCoded"],
        json!(true),
        "[{label}] expected zipTopCoded = true for 94025"
    );

    let warn_joined = out.warnings.join(" | ").to_lowercase();
    assert!(
        warn_joined.contains("no comparable sales") || warn_joined.contains("verified"),
        "[{label}] expected warning about missing comparable sales: {:?}",
        out.warnings
    );
    assert!(
        warn_joined.contains("94025") && warn_joined.contains("census"),
        "[{label}] expected warning about split ZIP 94025 across Census tract: {:?}",
        out.warnings
    );
}

#[test]
fn test_split_zip_94025_zero_comp_tract_calibration_on_synth_holdout_parcels() {
    for hid in ["synth-ca-94025-lower-sfr", "synth-ca-94025-remodel-sfr"] {
        let home = common::synth_holdout_by_id(hid);
        let (facts, market, geo) = strip_sales_for_zero_comp_test(&home);
        assert_split_zip_94025_zero_comp_calibrated(&facts, &market, &geo, hid);
    }
}

#[test]
fn test_comp_backed_controls_retain_medium_confidence_and_accuracy() {
    for (slug, min_v, max_v, expected_comps) in [
        (common::SYNTH_PRIMARY_SLUG, 1_850_000, 2_250_000, 8u64),
        (common::SYNTH_SECONDARY_SLUG, 2_150_000, 2_600_000, 8u64),
    ] {
        let mut rep = common::synth_report_for_slug(slug);
        if let Some(comps) = rep["facts"]["comps"].as_array_mut() {
            for c in comps {
                c["source"] = json!("County Recorder");
            }
        }
        let out = run_valuation(
            &rep["facts"],
            &rep["modules"]["market"]["data"],
            &rep["geo"],
        );
        let est = &out.data["estimate"];
        let val = est["value"].as_i64().unwrap();
        let conf = est["confidence"].as_str().unwrap();
        assert!(
            (min_v..=max_v).contains(&val),
            "{slug}: expected ${min_v}..=${max_v}, got ${val}"
        );
        assert!(
            conf == "Medium" || conf == "High",
            "{slug}: comp-backed control should keep Medium/High confidence, got {conf}"
        );
        assert_eq!(
            out.data["compsAnalysis"]["count"].as_u64(),
            Some(expected_comps),
            "{slug}: expected {expected_comps} comps"
        );
        assert_eq!(
            out.data["compsAnalysis"]
                .get("syntheticExcludedCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            0,
            "{slug}: expected 0 synthetic comps"
        );
        for c in out.data["comps"].as_array().expect("comps array") {
            assert_ne!(c["isSynthetic"], json!(true));
            assert_ne!(c["synthetic"], json!(true));
        }
        assert_eq!(
            est["tractCalibration"]["applied"],
            json!(false),
            "{slug}: homogeneous ZIP should not apply split-ZIP discount"
        );
    }
}

#[test]
fn test_top_coded_upper_tract_not_falsely_discounted() {
    let upper = common::synth_holdout_by_id("synth-ca-94025-upper-sfr");
    let (facts, market, geo) = strip_sales_for_zero_comp_test(&upper);

    let out = run_valuation(&facts, &market, &geo);
    let est = &out.data["estimate"];
    let val = est["value"].as_i64().unwrap();
    assert!(
        val >= 3_200_000,
        "top-coded $2M+ upper tract should not be discounted toward $2M; got ${val}"
    );
    assert_eq!(est["confidence"], json!("Low"));
    assert_eq!(est["tractCalibration"]["applied"], json!(false));
    assert!(
        !out.data["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["key"] == json!("tract-acs")),
        "top-coded $2M+ tract median must not anchor valuation downward"
    );
    assert!(out.data["comps"].as_array().unwrap().is_empty());
}

#[test]
fn test_split_zip_non_top_coded_94303_lower_tract_calibration() {
    let lower_94303 = common::synth_holdout_by_id("synth-ca-94303-lower-sfr");
    let (facts, mut market, geo) = strip_sales_for_zero_comp_test(&lower_94303);
    // Configure non-top-coded ZIP ACS median (< $2,000,000) paired with lower Census tract median
    market["acs"]["geos"][1]["medianHomeValue"] = json!(1_826_200);
    market["acs"]["geos"][1]["metrics"]["medianHomeValue"] = json!(1_826_200);
    market["zhvi"]["latest"] = json!(2_370_000);
    market["zhviSfr"]["latest"] = json!(2_370_000);

    let out = run_valuation(&facts, &market, &geo);
    let est = &out.data["estimate"];
    let val = est["value"].as_i64().unwrap();
    assert!(
        (1_050_000..=1_750_000).contains(&val),
        "expected lower-tract localized estimate in $1.05M–$1.75M rather than $2.37M ZIP SFR ZHVI, got ${val}"
    );
    assert_eq!(est["confidence"], json!("Low"));
    assert!(est["confidenceScore"].as_i64().unwrap() <= 45);
    assert_eq!(est["tractCalibration"]["applied"], json!(true));
    assert_eq!(est["tractCalibration"]["zipTopCoded"], json!(false));
    assert!(out.data["comps"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_offline_facts_zero_synthetic_comps_and_normalize_preserves_verified_sales() {
    let http = HttpClient::offline_loopback_only(None);
    let env = EnvConfig::default().with_overrides([("COMMERCIAL_DATA_MODE", "strict")]);
    let split_home = common::synth_holdout_by_id("synth-ca-94025-lower-sfr");
    let addr = split_home["address"].as_str().unwrap();

    let facts_out = build_facts_and_mls(&http, &env, addr, &split_home["raw"]["geo"], None)
        .await
        .expect("offline build_facts_and_mls");
    let f = &facts_out.data;
    assert_eq!(
        f.get("syntheticCompsCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        0,
        "must never emit synthetic comps"
    );
    let comps = f["comps"].as_array().expect("comps array");
    assert!(
        comps.is_empty(),
        "when offline and no verified recorded sales exist, comps must be strictly empty — got {comps:?}"
    );

    // Verify `normalize_stored_report` on secondary synthetic report preserves all 8 verified comps with zero synthetic flags
    let mut secondary = common::synth_report_secondary();
    if let Some(arr) = secondary["facts"]["comps"].as_array_mut() {
        for c in arr {
            c["source"] = json!("County Recorder");
        }
    }
    if let Some(arr) = secondary["modules"]["valuation"]["data"]["comps"].as_array_mut() {
        for c in arr {
            c["source"] = json!("County Recorder");
        }
    }
    normalize_stored_report(&mut secondary);
    let val_comps = secondary["modules"]["valuation"]["data"]["comps"]
        .as_array()
        .expect("valuation comps");
    assert_eq!(val_comps.len(), 8);
    for c in val_comps {
        assert_ne!(c.get("isSynthetic").and_then(|v| v.as_bool()), Some(true));
        assert_ne!(c.get("synthetic").and_then(|v| v.as_bool()), Some(true));
    }
}
