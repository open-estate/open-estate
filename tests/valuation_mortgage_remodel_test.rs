mod common;

use open_estate::modules::deed_title_marketplace::{
    build_deed_title_and_marketplace_with_options, MarketplaceQueryOptions,
};
use open_estate::modules::remodel_rebuild::build_remodel_and_rebuild;
use open_estate::property::{
    conventional_llpa_points_vs_optimal, conventional_pmi_annual_pct, pmi_dropoff_month,
    run_valuation, DEFAULT_OPTIMAL_CREDIT_SCORE,
};
use open_estate::web::render::render_report_html;
use serde_json::{json, Value};

fn synth_primary_with_county_comps() -> Value {
    let mut rep = common::synth_report_primary();
    if let Some(comps) = rep["facts"]["comps"].as_array_mut() {
        for c in comps {
            c["source"] = json!("County Recorder");
        }
    }
    rep
}

fn synth_holdout_with_county_comps(id: &str) -> Value {
    let mut home = common::synth_holdout_by_id(id);
    if let Some(comps) = home["raw"]["facts"]["comps"].as_array_mut() {
        for c in comps {
            c["source"] = json!("County Recorder");
        }
    }
    home
}

#[test]
fn valuation_huber_outlier_downweighting_permit_credit_condo_index_and_avm_cap() {
    let primary = synth_primary_with_county_comps();
    let geo = &primary["geo"];
    let market = &primary["modules"]["market"]["data"];

    let mut facts = primary["facts"].clone();
    let base_comps = primary["facts"]["comps"].as_array().expect("primary comps");
    let upper_holdout = synth_holdout_with_county_comps("synth-ca-94303-upper-sfr");
    let mut outlier_comp = base_comps[4].clone();
    outlier_comp["address"] = upper_holdout["address"].clone();
    outlier_comp["soldDate"] = json!("2026-04-28");
    outlier_comp["soldPrice"] = json!(5_450_000);
    outlier_comp["price"] = json!(5_450_000);
    outlier_comp["source"] = json!("County Recorder");

    let mut reo_comp = base_comps[7].clone();
    reo_comp["soldPrice"] = json!(1_340_000);
    reo_comp["price"] = json!(1_340_000);
    reo_comp["status"] = json!("REO Bank Owned Foreclosure");
    reo_comp["source"] = json!("County Recorder");

    facts["comps"] = json!([
        base_comps[0],
        base_comps[1],
        base_comps[2],
        base_comps[3],
        outlier_comp,
        reo_comp,
    ]);
    facts["saleHistory"] = json!([
        { "date": "2022-06-15", "event": "Sold", "price": 1_850_000 },
        { "date": "2024-03-10", "event": "Sold (Quitclaim Interspousal Transfer)", "price": 15_000 }
    ]);
    facts["lastSale"] = json!({ "date": "2024-03-10", "price": 15_000, "deedType": "Quitclaim" });

    facts["permits"] = json!([
        {
            "date": "2023-09-12",
            "type": "Kitchen & Bath Addition/Remodel",
            "status": "Finaled",
            "valuation": 160_000
        }
    ]);
    facts["avms"] = json!([
        { "source": "Model Alpha", "value": 2_260_000, "asOf": "2026-09" },
        { "source": "Model Beta", "value": 2_280_000, "asOf": "2026-09" },
        { "source": "Model Gamma", "value": 2_300_000, "asOf": "2026-09" },
        { "source": "Model Delta", "value": 2_320_000, "asOf": "2025-01" }
    ]);

    let out = run_valuation(&facts, market, geo);
    let est_val = out.data["estimate"]["value"].as_i64().unwrap();
    let comps_count = out.data["compsAnalysis"]["count"].as_i64().unwrap();
    assert_eq!(
        comps_count, 6,
        "all 6 supplied comps must be preserved in compsAnalysis"
    );

    let diag = &out.data["estimate"]["valuationDiagnostics"];
    assert!(
        diag["outlierCompsDownweighted"].as_i64().unwrap_or(0) >= 1,
        "extreme $5.45M outlier comp should be Huber-downweighted: {diag}"
    );
    assert!(
        diag["permitValueCreditUsd"].as_i64().unwrap_or(0) >= 80_000,
        "post-2022 $160k finaled permit should add contributory credit to repeat-sales: {diag}"
    );
    assert_eq!(diag["avmCount"], json!(4));

    assert!(
        (1_850_000..=2_350_000).contains(&est_val),
        "expected robust valuation in $1.85M..=$2.35M despite $5.45M outlier, got {est_val}"
    );

    // Verify AVM group diminishing weights: each of the 4 AVMs should have weight <= 0.45
    let methods = out.data["methods"].as_array().unwrap();
    for m in methods.iter().filter(|m| {
        m["key"]
            .as_str()
            .map(|k| k.starts_with("avm-"))
            .unwrap_or(false)
    }) {
        let w = m["weight"].as_f64().unwrap();
        assert!(
            w <= 0.45,
            "expected ensemble-capped AVM weight <= 0.45, got {w}"
        );
    }

    // Verify attached Condo (`synth-ma-02139-condo`) selects All-Homes ZHVI (`zip_all`) rather than Detached SFR ZHVI
    let condo = synth_holdout_with_county_comps("synth-ma-02139-condo");
    let condo_out = run_valuation(
        &condo["raw"]["facts"],
        &condo["raw"]["market"],
        &condo["raw"]["geo"],
    );
    let condo_seg = condo_out.data["estimate"]["valuationDiagnostics"]["zhviSegment"]
        .as_str()
        .unwrap_or("");
    assert!(
        condo_seg.to_lowercase().contains("all-homes")
            || condo_seg.to_lowercase().contains("condo"),
        "expected condo ZHVI segment to use all-homes index, got {condo_seg}"
    );
}

#[test]
fn optimal_mortgage_rates_llpa_credit_score_tiers_pmi_dropoff_and_marketplace_pricing() {
    assert_eq!(DEFAULT_OPTIMAL_CREDIT_SCORE, 760);

    let llpa_780 = conventional_llpa_points_vs_optimal(790, 80.0, "primary", false, false, false);
    let llpa_760 = conventional_llpa_points_vs_optimal(770, 80.0, "primary", false, false, false);
    let llpa_680 = conventional_llpa_points_vs_optimal(690, 80.0, "primary", false, false, false);
    let llpa_inv =
        conventional_llpa_points_vs_optimal(770, 80.0, "investment", false, false, false);
    assert!(llpa_780 < llpa_760, "780+ FICO should beat 760-779 LLPA");
    assert_eq!(
        llpa_760, 0.0,
        "760-779 @ 80% LTV primary is the 0.00-point optimal par baseline"
    );
    assert!(
        llpa_680 > 1.0,
        "680-699 FICO @ 80% LTV should carry >1.0 LLPA point vs optimal"
    );
    assert!(
        llpa_inv > 3.0,
        "investment occupancy @ 80% LTV should add >3.0 LLPA points"
    );

    assert_eq!(conventional_pmi_annual_pct(770, 80.0), 0.0);
    let pmi_760_90 = conventional_pmi_annual_pct(770, 90.0);
    let pmi_660_90 = conventional_pmi_annual_pct(670, 90.0);
    assert!(pmi_660_90 > pmi_760_90 * 2.0);
    assert_eq!(pmi_dropoff_month(640_000.0, 800_000.0, 6.4, 30), None);
    let drop_mo =
        pmi_dropoff_month(720_000.0, 800_000.0, 6.4, 30).expect("90% LTV should have PMI drop-off");
    assert!(
        (70..=135).contains(&drop_mo),
        "unexpected 78% LTV drop-off month: {drop_mo}"
    );

    let il_home = synth_holdout_with_county_comps("synth-il-60201-vintage-sfr");
    let geo = &il_home["raw"]["geo"];
    let market = &il_home["raw"]["market"];
    let facts = &il_home["raw"]["facts"];
    let val = run_valuation(facts, market, geo);
    let own = &val.data["ownership"];
    assert_eq!(own["defaultCreditScore"], json!(760));
    let opt30 = own["optimalRate30"].as_f64().unwrap();
    let pmms30 = own["pmmsSurveyRate30"].as_f64().unwrap();
    let apr30 = own["apr30"].as_f64().unwrap();
    assert!(
        opt30 < pmms30,
        "760-FICO optimal par rate ({opt30}) should beat PMMS survey average ({pmms30})"
    );
    assert!(
        apr30 >= opt30,
        "TILA Reg Z APR ({apr30}) must be >= note rate ({opt30})"
    );

    let tiers = own["creditScoreTiers"]
        .as_array()
        .expect("creditScoreTiers array");
    assert_eq!(tiers.len(), 10);
    let r_780 = tiers[0]["rateAt20DownPct"].as_f64().unwrap();
    let r_760 = tiers[1]["rateAt20DownPct"].as_f64().unwrap();
    let r_680 = tiers[5]["rateAt20DownPct"].as_f64().unwrap();
    let r_620 = tiers[8]["rateAt20DownPct"].as_f64().unwrap();
    assert!(r_780 <= r_760 && r_760 < r_680 && r_680 < r_620);
    assert_eq!(tiers[9]["conventionalEligible"], json!(false));
    assert_eq!(tiers[9]["recommendedProgram"], json!("FHA 3.5% Down"));

    let base_rep = json!({
        "geo": geo,
        "facts": facts,
        "modules": { "market": { "data": market }, "valuation": { "data": val.data } }
    });
    let env = open_estate::shared::EnvConfig::from_map(std::collections::HashMap::new());
    let mp_780 = build_deed_title_and_marketplace_with_options(
        &base_rep,
        &env,
        &MarketplaceQueryOptions {
            credit_score: Some(790),
            loan_term_years: Some(30),
            discount_points: Some(1.0),
            occupancy: Some("primary".into()),
            ..Default::default()
        },
    )
    .marketplace;
    let mp_660 = build_deed_title_and_marketplace_with_options(
        &base_rep,
        &env,
        &MarketplaceQueryOptions {
            credit_score: Some(665),
            loan_term_years: Some(30),
            discount_points: Some(0.0),
            occupancy: Some("investment".into()),
            ..Default::default()
        },
    )
    .marketplace;
    let conv_780 = mp_780["verticals"]["mortgage"]["benchmarks"][0]["ratePct"]
        .as_f64()
        .unwrap();
    let conv_660 = mp_660["verticals"]["mortgage"]["benchmarks"][0]["ratePct"]
        .as_f64()
        .unwrap();
    assert!(
        conv_780 + 0.8 < conv_660,
        "expected 790-FICO 1pt-buydown primary rate ({conv_780}%) << 665-FICO investment rate ({conv_660}%)"
    );
}

#[test]
fn remodel_teardown_rebuild_lawn_quotes_and_html_render_end_to_end() {
    let report = synth_primary_with_county_comps();

    let rm_out = build_remodel_and_rebuild(&report);
    let rm = &rm_out.data;

    let mult = rm["regionalCostIndex"]["regionalMultiplier"]
        .as_f64()
        .unwrap();
    assert!(
        mult > 1.45,
        "Santa Clara County + BLS multiplier should exceed 1.45x, got {mult}"
    );

    let projs = rm["remodelProjects"].as_array().unwrap();
    assert!(
        projs.len() >= 11,
        "expected >=11 remodel projects incl. ADU & seismic, got {}",
        projs.len()
    );
    for p in projs {
        let low = p["lowUsd"].as_i64().unwrap();
        let typ = p["typicalUsd"].as_i64().unwrap();
        let high = p["highUsd"].as_i64().unwrap();
        let recoup = p["costRecoupedPct"].as_f64().unwrap();
        assert!(low < typ && typ < high, "invalid range on project {p}");
        assert!(
            (35.0..=110.0).contains(&recoup),
            "invalid recoup % on project {p}"
        );
    }

    let td = &rm["tearDownAndRebuild"];
    assert_eq!(td["applicableForTearDown"], json!(true));
    assert_eq!(
        td["demolitionAndSitePrep"]["isPre1978AbatementAllowance"],
        json!(true)
    );
    let tiers = td["tiers"].as_array().unwrap();
    assert_eq!(tiers.len(), 3);
    let std_tot = tiers[0]["currentSize"]["turnkeyRebuildTotalUsd"]
        .as_i64()
        .unwrap();
    let cus_tot = tiers[1]["currentSize"]["turnkeyRebuildTotalUsd"]
        .as_i64()
        .unwrap();
    let lux_tot = tiers[2]["currentSize"]["turnkeyRebuildTotalUsd"]
        .as_i64()
        .unwrap();
    assert!(std_tot < cus_tot && cus_tot < lux_tot);
    let stage_sum: f64 = td["nahbStageBreakdown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sharePct"].as_f64().unwrap())
        .sum();
    assert!(
        (stage_sum - 100.0).abs() < 0.2,
        "NAHB stages must sum to 100%, got {stage_sum}"
    );

    let lawn = &rm["lawnAndExterior"];
    assert_eq!(lawn["turfRebateProgram"]["eligibleRegion"], json!(true));
    assert!(
        lawn["turfRebateProgram"]["agencyName"]
            .as_str()
            .unwrap()
            .contains("Valley Water"),
        "expected Santa Clara Valley Water turf rebate program"
    );
    let quotes = lawn["quotes"].as_array().unwrap();
    assert_eq!(quotes.len(), 10);
    assert_eq!(quotes[0]["id"], json!("lawn-sod-rebuild"));

    let html = render_report_html(&report);
    assert!(html.contains("id=\"remodel\""));
    assert!(html.contains("renderRemodel"));
    assert!(html.contains("remodelAndRebuild"));
    assert!(html.contains("id=\"selFico\""));
    assert!(html.contains("remodelModeSeg"));
    for forbidden in [
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
    ] {
        assert!(
            !html.contains(forbidden),
            "rendered HTML must never contain forbidden token {forbidden:?}"
        );
    }
}
