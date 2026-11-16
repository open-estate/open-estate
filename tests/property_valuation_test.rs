#![recursion_limit = "512"]

mod common;

use open_estate::property::{
    connector_status, geocode_from_coordinates, load_zillow_for_zip, run_market_trends,
    run_valuation, series_stats, split_us_address, state_fips_for_abbr, strip_pii_keys,
    value_array_to_series,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Instant;

fn has_any_pii_key(v: &Value, path: &str) -> Option<String> {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let kl = k.to_lowercase();
                let is_pii = kl.starts_with("owner")
                    || kl.starts_with("own_")
                    || kl.starts_with("assessee")
                    || kl.starts_with("mailing")
                    || kl.starts_with("mail_addr")
                    || kl.starts_with("mailaddr")
                    || kl.starts_with("careof")
                    || kl.starts_with("care_of")
                    || kl.starts_with("incareof");
                let cur_path = format!("{path}.{k}");
                if is_pii {
                    return Some(cur_path);
                }
                if let Some(hit) = has_any_pii_key(val, &cur_path) {
                    return Some(hit);
                }
            }
            None
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                if let Some(hit) = has_any_pii_key(item, &format!("{path}[{i}]")) {
                    return Some(hit);
                }
            }
            None
        }
        _ => None,
    }
}

#[test]
fn test_1_valuation_engine_unit_and_synth_primary_facts() {
    let mut primary_report = common::synth_report_primary();
    if let Some(comps) = primary_report["facts"]["comps"].as_array_mut() {
        for c in comps.iter_mut() {
            c["source"] = json!("County Recorder");
        }
    }

    let facts = &primary_report["facts"];
    let market = &primary_report["modules"]["market"]["data"];
    let geo = &primary_report["geo"];

    let zip_mid_arr = &market["series"]["zip"];
    let zip_sfr_arr = &market["series"]["zipSfr"];
    let zs_mid = value_array_to_series(Some(zip_mid_arr));
    let zs_sfr = value_array_to_series(Some(zip_sfr_arr));
    assert!(
        zs_mid.len() >= 120 && zs_sfr.len() >= 120,
        "expected >=10 years of synthetic 95118 monthly series, got mid={} sfr={}",
        zs_mid.len(),
        zs_sfr.len()
    );
    let _zhvi_stats = series_stats(&zs_mid, 12).expect("zhvi stats");
    let _zhvi_sfr_stats = series_stats(&zs_sfr, 12).expect("zhvi_sfr stats");

    let out = run_valuation(facts, market, geo);
    let d = &out.data;

    let est = d.get("estimate").expect("estimate object");
    let val = est["value"].as_i64().expect("estimate.value");
    let low = est["low"].as_i64().expect("estimate.low");
    let high = est["high"].as_i64().expect("estimate.high");
    let ppsf = est["pricePerSqft"].as_i64().expect("estimate.pricePerSqft");
    let conf_score = est["confidenceScore"]
        .as_i64()
        .expect("estimate.confidenceScore");

    assert!(
        (1_750_000..=2_450_000).contains(&val),
        "expected realistic synthetic 95118 value, got {val}"
    );
    assert!(
        low < val && val < high,
        "expected low ({low}) < value ({val}) < high ({high})"
    );
    assert!(ppsf > 850, "expected $/sqft > 850, got {ppsf}");
    assert!((35..=98).contains(&conf_score));

    // Verify compsAnalysis & adjustments: all 8 comps are arm's-length sales (ZERO synthetic proxy flags)
    let ca = d.get("compsAnalysis").expect("compsAnalysis");
    assert_eq!(ca["count"].as_u64(), Some(8));
    assert_eq!(
        ca.get("syntheticExcludedCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        0,
        "must have zero synthetic proxy comps"
    );
    for c in ca["comps"].as_array().expect("compsAnalysis.comps") {
        assert_ne!(c["isSynthetic"], json!(true));
        assert_ne!(c["synthetic"], json!(true));
    }
    let first_comp = &ca["comps"][0];
    assert!(first_comp["adjustedPrice"].as_i64().unwrap_or(0) > 1_000_000);
    assert!(first_comp["adjustments"]["sizePct"].is_number());
    assert!(first_comp["listToSoldPct"].is_number());

    // Verify California Prop 13 monthly tax
    let own = d.get("ownershipCost").expect("ownershipCost");
    let prop_tax_mo = own["monthly"]["propertyTax"]
        .as_i64()
        .expect("monthly.propertyTax");
    let tax_rate = facts["taxRate"].as_f64().expect("facts.taxRate");
    let expected_prop13_mo = ((val as f64 * tax_rate) / 12.0).round() as i64;
    assert!(
        (prop_tax_mo - expected_prop13_mo).abs() <= 2,
        "Prop 13 monthly tax {prop_tax_mo} should match reassessed {expected_prop13_mo}"
    );

    // Verify scenarios P10 <= P50 <= P90 across 1y, 3y, 5y + stress test
    let sc = d.get("scenarios").expect("scenarios");
    for h_key in ["1y", "3y", "5y"] {
        let h = sc.get(h_key).unwrap_or_else(|| panic!("missing {h_key}"));
        let p10 = h["p10"].as_f64().expect("p10");
        let p50 = h["p50"].as_f64().expect("p50");
        let p90 = h["p90"].as_f64().expect("p90");
        assert!(
            p10 <= p50 && p50 <= p90,
            "{h_key}: expected p10 ({p10}) <= p50 ({p50}) <= p90 ({p90})"
        );
        let bear = h["bear"].as_i64().unwrap();
        let base = h["base"].as_i64().unwrap();
        let bull = h["bull"].as_i64().unwrap();
        assert!(bear <= base && base <= bull);
    }
    assert!(sc["stress"]["value"].as_i64().unwrap_or(0) < val);

    // Verify investorMetrics, rentResearch & drivers
    let inv = d.get("investorMetrics").expect("investorMetrics");
    let est_rent = inv["estimatedMonthlyRent"].as_i64().expect("estimatedMonthlyRent");
    assert!(est_rent > 3_500, "expected monthly rent > $3,500, got {est_rent}");
    assert_eq!(
        d["rentResearch"]["recommendedMonthlyRent"].as_i64(),
        Some(est_rent)
    );
    assert!(d["rentResearch"]["adjustedMedianMonthly"].as_i64().unwrap_or(0) > 3500);
    assert!(d["rentResearch"]["adjustedAverageMonthly"].as_i64().unwrap_or(0) > 3500);
    assert!(inv["grossYieldPct"].as_f64().unwrap_or(0.0) > 1.5);
    assert!(inv["capRatePct"].is_number());
    assert!(inv["onePercentRule"].as_f64().unwrap_or(0.0) > 0.1);
    assert!(!d["drivers"]["upside"].as_array().unwrap().is_empty());
    assert!(!d["drivers"]["downside"].as_array().unwrap().is_empty());

    // Verify synthetic holdout home (`synth-ca-95118-mid-sfr`) when `comps` is empty relies
    // strictly on repeat-sales + ZIP ZHVI with ZERO fabricated comps, and computes positive
    // CA Prop 13 supplemental tax against a lower prior assessed roll.
    let mid_home = common::synth_holdout_by_id("synth-ca-95118-mid-sfr");
    let mut mid_facts = mid_home["raw"]["facts"].clone();
    mid_facts["comps"] = json!([]);
    mid_facts["avms"] = json!([]);
    mid_facts["taxRate"] = json!(0.0124);
    mid_facts["taxHistory"] = json!([
        { "year": 2025, "assessed": 890_000, "assessedValue": 890_000, "tax": 11_036, "taxAmount": 11_036 }
    ]);
    mid_facts["saleHistory"] = json!([
        { "date": "2018-04-12", "price": 1_360_000, "event": "Sold", "armsLength": true }
    ]);
    mid_facts["lastSale"] = mid_facts["saleHistory"][0].clone();

    let mid_out = run_valuation(
        &mid_facts,
        &mid_home["raw"]["market"],
        &mid_home["raw"]["geo"],
    );
    assert!(
        mid_out.data["comps"]
            .as_array()
            .expect("comps array")
            .is_empty(),
        "must never invent comps when facts.comps is empty"
    );
    let pv = mid_out.data["estimate"]["value"].as_i64().unwrap_or(0);
    assert!(pv > 1_000_000);
    assert!(
        mid_out.data["estimate"]["low"].as_i64().unwrap() < pv
            && pv < mid_out.data["estimate"]["high"].as_i64().unwrap()
    );
    let ca_supp = mid_out.data["ownershipCost"]["closingAndSupplemental"]["caSupplementalEst"]
        .as_i64()
        .expect("caSupplementalEst from prior roll");
    assert!(
        ca_supp > 5_000,
        "expected positive CA supplemental tax delta vs prior roll, got {ca_supp}"
    );
}

#[test]
fn test_2_connector_registry_address_parser_and_zero_pii_sanitizer() {
    let env = EnvConfig::default();
    let connectors = connector_status(&env);
    assert!(connectors.len() >= 10);
    assert!(connectors
        .iter()
        .any(|c| c["id"] == "county_gis" && c["active"] == true));

    assert_eq!(state_fips_for_abbr("CA"), Some("06"));
    assert_eq!(state_fips_for_abbr("IL"), Some("17"));

    let (disp_full, street, secondary_loc) = split_us_address(common::SYNTH_PRIMARY_ADDRESS);
    assert_eq!(disp_full, common::SYNTH_PRIMARY_ADDRESS);
    assert_eq!(street, "100 Synthparcelalpha Ln");
    assert_eq!(secondary_loc, "San Jose, CA 95118");

    let mut raw_parcel = json!({
        "apn": "000-00-100",
        "lotSqft": 6600,
        "ownerName": "REDACTED_OWNER",
        "mailingAddress": "REDACTED_MAILING",
        "nested": {
            "assesseeName": "REDACTED_ASSESSEE",
            "careOf": "REDACTED_CARE_OF",
            "yearBuilt": 1974
        }
    });
    strip_pii_keys(&mut raw_parcel);
    if let Some(bad_key) = has_any_pii_key(&raw_parcel, "facts") {
        panic!("Owner PII key leaked after strip_pii_keys: {bad_key}");
    }
    assert_eq!(raw_parcel["apn"], json!("000-00-100"));
    assert_eq!(raw_parcel["nested"]["yearBuilt"], json!(1974));
}

#[tokio::test]
#[ignore = "live external Census coordinates API"]
async fn test_3_live_census_coordinates_geocode_synthetic_label() {
    let http = HttpClient::new(Some(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache/http"),
    ));
    let geo_out = geocode_from_coordinates(
        &http,
        common::SYNTH_PRIMARY_ADDRESS,
        37.2560,
        -121.8890,
        "synthetic",
    )
    .await
    .expect("geocode synthetic coordinates");
    let g = &geo_out.data;
    assert_eq!(g["matched"], json!(true));
    assert_eq!(g["countyFips"], json!("06085"));
    assert_eq!(g["state"], json!("CA"));
    assert!(g["tract"].as_str().unwrap_or("").starts_with("06085"));
}

#[tokio::test]
#[ignore = "live external Zillow/ACS/Freddie Mac datasets"]
async fn test_4_live_zillow_acs_freddie_mac_market_and_valuation_e2e_95118() {
    let env = EnvConfig::from_map(std::collections::HashMap::from([(
        "PUBLIC_DATA_MODE".to_string(),
        "local_csv".to_string(),
    )]));
    let http = HttpClient::from_env(
        &env,
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache/http")),
    );
    let datasets_dir = &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache/datasets");

    let synth_rep = common::synth_report_primary();
    let geo_data = &synth_rep["geo"];
    assert_eq!(geo_data["zip"], json!("95118"));

    let zillow = load_zillow_for_zip(&http, "95118", Some("41940"), Some(datasets_dir))
        .await
        .expect("load_zillow_for_zip 95118");

    let t_cached = Instant::now();
    let zillow_cached = load_zillow_for_zip(&http, "95118", Some("41940"), Some(datasets_dir))
        .await
        .expect("cached load_zillow_for_zip 95118");
    let cached_us = t_cached.elapsed().as_micros();
    assert!(
        cached_us < 1000,
        "expected L1 moka repeat lookup < 1000us (<1ms), took {cached_us}us"
    );
    assert_eq!(
        zillow["zipSeries"]["zhvi_mid"].as_array().unwrap().len(),
        zillow_cached["zipSeries"]["zhvi_mid"]
            .as_array()
            .unwrap()
            .len()
    );

    let market_out = run_market_trends(&http, &env, geo_data, &zillow)
        .await
        .expect("run_market_trends 95118");
    let val_out = run_valuation(&synth_rep["facts"], &market_out.data, geo_data);
    let est_val = val_out.data["estimate"]["value"]
        .as_i64()
        .expect("end-to-end 95118 estimate.value");
    assert!(est_val > 500_000);
}
