mod common;

use open_estate::modules::insurance::run_insurance_with_condition;
use open_estate::modules::insurance_quotes::{
    build_insurance_quotes, ensure_insurance_quotes_on_report,
};
use open_estate::web::render::render_report_html;
use serde_json::{json, Value};

fn load_holdout_home_as_report(id: &str) -> Value {
    let home = common::synth_holdout_by_id(id);
    json!({
        "meta": {
            "displayAddress": home["address"],
        },
        "geo": home["raw"]["geo"],
        "facts": home["raw"]["facts"],
        "modules": {}
    })
}

fn assert_zero_forbidden_substrings(text: &str, label: &str) {
    let lc = text.to_ascii_lowercase();
    let forbidden: &[&str] = &[
        concat!("pro", "vider"),
        concat!("pro", "viders"),
        concat!("ef", "fort"),
        concat!("gpt-custom-", "primary"),
        concat!("gpt-custom-", "fallback"),
        concat!("fallback", "used"),
        concat!("home", "lens"),
        concat!("deterministic-", "fallback"),
        concat!("rust-sqlx-", "sqlite"),
        concat!("max ", "reasoning"),
        concat!("ai", "model"),
        concat!("openai_", "api_key"),
    ];
    for tok in forbidden {
        assert!(
            !lc.contains(tok),
            "{label} contained forbidden substring `{tok}`"
        );
    }
}

fn assert_zero_fabricated_commercial_carrier_quotes(q: &Value) {
    let fabricated_brands = [
        "Amica",
        "Chubb",
        "USAA",
        "Bamboo",
        "Farmers",
        "State Farm",
        "Hippo",
        "Lemonade",
        "Palomar",
        "Neptune",
    ];
    if let Some(carriers) = q.get("carriers").and_then(Value::as_array) {
        for c in carriers {
            let name = c
                .get("carrierName")
                .or_else(|| c.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let has_dollar_quote = c
                .get("estAnnual")
                .and_then(Value::as_i64)
                .map(|v| v > 0)
                .unwrap_or(false)
                || c.get("estMonthly")
                    .and_then(Value::as_i64)
                    .map(|v| v > 0)
                    .unwrap_or(false);
            if has_dollar_quote {
                for brand in fabricated_brands {
                    assert!(
                        !name.contains(brand),
                        "commercial carrier `{name}` must never have fabricated dollar quotes without a live carrier extension: {c}"
                    );
                }
            }
        }
    }
}

fn assert_statutory_benchmark_schema(q: &Value) {
    let obj = q
        .as_object()
        .expect("insuranceQuotes must be a JSON object");
    for key in [
        "annualTotal",
        "monthlyTotal",
        "allInWithRecommendedHazardsMonthly",
        "confidence",
        "coverage",
        "deductibleOptions",
        "specialtyAddOns",
        "riskDrivers",
        "underwritingProfile",
    ] {
        assert!(
            obj.contains_key(key),
            "missing top-level key `{key}` in {q}"
        );
    }

    assert!(q["annualTotal"].as_i64().unwrap() > 0);
    assert!(q["monthlyTotal"].as_i64().unwrap() > 0);
    assert!(
        q["allInWithRecommendedHazardsMonthly"].as_i64().unwrap()
            >= q["monthlyTotal"].as_i64().unwrap()
    );
    assert!(!q["confidence"].as_str().unwrap().is_empty());

    let cov = &q["coverage"];
    for k in [
        "dwellingA",
        "otherStructuresB",
        "personalPropertyC",
        "lossOfUseD",
        "liabilityE",
        "medicalF",
        "rebuildCostPerSqft",
    ] {
        assert!(
            cov[k].as_i64().unwrap() > 0,
            "coverage.{k} must be positive i64, got {:?}",
            cov[k]
        );
    }

    let deds = q["deductibleOptions"]
        .as_array()
        .expect("deductibleOptions array");
    assert_eq!(deds.len(), 4);
    let expected_deds = [1_000_i64, 2_500, 5_000, 10_000];
    for (i, d) in deds.iter().enumerate() {
        assert_eq!(d["deductible"].as_i64(), Some(expected_deds[i]));
        assert_eq!(d["recommended"].as_bool(), Some(expected_deds[i] == 2_500));
        assert!(d["ho3Annual"].as_i64().unwrap() > 0);
        assert!(d["ho3Monthly"].as_i64().unwrap() > 0);
        if i > 0 {
            assert!(
                deds[i - 1]["ho3Annual"].as_i64().unwrap() > d["ho3Annual"].as_i64().unwrap(),
                "higher deductible must yield lower HO-3 annual benchmark"
            );
        }
    }

    assert_zero_fabricated_commercial_carrier_quotes(q);

    let drivers = q["riskDrivers"].as_array().expect("riskDrivers array");
    assert!(!drivers.is_empty());
    for drv in drivers {
        assert!(!drv["factor"].as_str().unwrap_or("").is_empty());
        assert!(drv["impactPct"].as_i64().is_some());
        assert!(!drv["note"].as_str().unwrap_or("").is_empty());
    }
}

#[test]
fn test_synth_secondary_seismic_adu_and_zero_fabricated_carriers() {
    let mut report = common::synth_report_secondary();
    if let Some(obj) = report.as_object_mut() {
        obj.remove("insuranceQuotes");
        obj.remove("remodelAndRebuild");
    }
    report["facts"]["yearBuilt"] = json!(1925);
    report["facts"]["permits"] = json!([]);
    report["modules"]["hazards"]["data"]["earthquake"]["pga"] = json!(0.83);
    report["modules"]["hazards"]["data"]["earthquake"]["designMaps"] =
        json!({ "pgaM": 0.83, "ss": 2.15, "sdc": "D" });

    let quotes = build_insurance_quotes(&report);
    let raw_quotes = serde_json::to_string_pretty(&quotes).unwrap();

    assert_zero_forbidden_substrings(&raw_quotes, "synth secondary insuranceQuotes JSON");
    assert_statutory_benchmark_schema(&quotes);

    // CA Bay Area rebuild $/sqft (~$460-$515/sqft) + 2,200 sqft + 600 sqft ADU allowance
    let rebuild_sqft = quotes["coverage"]["rebuildCostPerSqft"].as_i64().unwrap();
    assert!(
        (460..=515).contains(&rebuild_sqft),
        "expected Bay Area rebuildCostPerSqft in 460..=515, got {rebuild_sqft}"
    );
    let dwelling_a = quotes["coverage"]["dwellingA"].as_i64().unwrap();
    assert!(
        (900_000..=1_750_000).contains(&dwelling_a),
        "expected Coverage A in $900k..=$1.75M, got {dwelling_a}"
    );

    // $2,500 default deductible matches top-level annualTotal & monthlyTotal
    let ded_2500 = quotes["deductibleOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["deductible"].as_i64() == Some(2_500))
        .unwrap();
    assert_eq!(ded_2500["recommended"].as_bool(), Some(true));
    assert_eq!(ded_2500["ho3Annual"], quotes["annualTotal"]);
    assert_eq!(ded_2500["ho3Monthly"], quotes["monthlyTotal"]);

    // Specialty Add-Ons: High seismic CEA 15% deductible + 25% Brace+Bolt discount on 1925 wood-frame
    let eq = &quotes["specialtyAddOns"]["earthquake"];
    assert_eq!(eq["applicable"].as_bool(), Some(true));
    assert_eq!(eq["recommended"].as_bool(), Some(true));
    assert_eq!(eq["deductiblePct"].as_i64(), Some(15));
    assert_eq!(eq["pgaG"], json!(0.83));
    assert_eq!(eq["retrofitDiscountPct"].as_i64(), Some(25));
    assert!(eq["postRetrofitAnnual"].as_i64().unwrap() < eq["annual"].as_i64().unwrap());

    // Flood Zone X optional + Wildfire FAIR Plan not required in urban Campbell LRA
    let fl = &quotes["specialtyAddOns"]["flood"];
    assert_eq!(fl["femaZone"].as_str(), Some("X"));
    assert_eq!(fl["mandatory"].as_bool(), Some(false));

    let wf = &quotes["specialtyAddOns"]["wildfireFairPlan"];
    assert_eq!(wf["applicable"].as_bool(), Some(false));

    // Underwriting profile reflects synthetic attributes (1925, 2200 sqft)
    let uw = &quotes["underwritingProfile"];
    assert_eq!(uw["yearBuilt"].as_i64(), Some(1925));
    assert_eq!(uw["livingAreaSqft"].as_i64(), Some(2200));
    assert!(uw["clueLossHistoryTip"]
        .as_str()
        .unwrap()
        .contains("C.L.U.E."));
    assert!(uw["copyableFactSheet"]
        .as_str()
        .unwrap()
        .contains("200 Synthparcelbeta Ave"));
}

#[test]
fn test_ensure_insurance_quotes_idempotency_and_html_guardrails_on_synthetic_reports() {
    for (label, mut report) in [
        ("synth-secondary", common::synth_report_secondary()),
        ("synth-primary", common::synth_report_primary()),
    ] {
        if let Some(obj) = report.as_object_mut() {
            obj.remove("insuranceQuotes");
        }

        assert!(ensure_insurance_quotes_on_report(&mut report));
        assert!(!ensure_insurance_quotes_on_report(&mut report));

        let q = &report["insuranceQuotes"];
        assert_statutory_benchmark_schema(q);
        assert_zero_forbidden_substrings(&q.to_string(), label);

        let html = render_report_html(&report);
        assert_zero_forbidden_substrings(&html, &format!("rendered {label} HTML"));
    }
}

#[test]
fn test_missing_sqft_returns_honest_missing_input_instead_of_inventing_1850_sqft() {
    let mut sparse = common::synth_report_secondary();
    sparse["facts"] = json!({
        "address": common::SYNTH_SECONDARY_ADDRESS,
        "propertyType": "Single-family (detached)"
    });
    if let Some(obj) = sparse.as_object_mut() {
        obj.remove("insurance");
        obj.remove("insuranceQuotes");
    }

    let q = build_insurance_quotes(&sparse);
    assert_zero_forbidden_substrings(&q.to_string(), "sparse missing-sqft insuranceQuotes");
    assert_zero_fabricated_commercial_carrier_quotes(&q);

    // Zero-mock invariant: must NEVER invent livingAreaSqft = 1850 or yearBuilt = 1985 when missing!
    let status = q.get("status").and_then(Value::as_str).unwrap_or("");
    let annual_missing = q.get("annualTotal").map(Value::is_null).unwrap_or(true);
    assert!(
        status == "missing_required_input" || annual_missing,
        "missing livingAreaSqft must report missing_required_input or null annualTotal rather than inventing 1,850 sqft: {q}"
    );
    assert_ne!(
        q.pointer("/coverage/livingAreaSqft")
            .and_then(Value::as_i64),
        Some(1850),
        "must never invent 1850 livingAreaSqft"
    );
    assert_ne!(
        q.pointer("/underwritingProfile/yearBuilt")
            .and_then(Value::as_i64),
        Some(1985),
        "must never invent 1985 yearBuilt"
    );
}

#[test]
fn test_synth_holdout_homes_statutory_wui_fair_plan_sfha_and_brace_bolt_credit() {
    // Case A: Synthetic San Diego CA holdout home (`synth-ca-92104-historic-sfr`) under Very High FHSZ statutory overlay
    let mut sd_wui = load_holdout_home_as_report("synth-ca-92104-historic-sfr");
    sd_wui["modules"]["naturalHazards"] = json!({
        "data": {
            "wildfire": {
                "california": { "fhsz": "Very High", "fhszClass": 3 },
                "whp": { "class": 5, "label": "Very High" }
            },
            "earthquake": { "designMaps": { "pgaM": 0.44, "ss": 1.12, "sdc": "D" } },
            "flood": { "zone": "X", "sfha": false }
        }
    });
    let wui_q = build_insurance_quotes(&sd_wui);
    assert_zero_forbidden_substrings(&wui_q.to_string(), "San Diego WUI FAIR Plan JSON");
    assert_statutory_benchmark_schema(&wui_q);
    assert_eq!(
        wui_q["specialtyAddOns"]["wildfireFairPlan"]["applicable"].as_bool(),
        Some(true)
    );
    let fair_ann = wui_q["specialtyAddOns"]["wildfireFairPlan"]["fairPlanAnnual"]
        .as_i64()
        .unwrap();
    let dic_ann = wui_q["specialtyAddOns"]["wildfireFairPlan"]["dicWrapAnnual"]
        .as_i64()
        .unwrap();
    assert!(fair_ann > 0 && dic_ann > 0);
    assert_eq!(
        wui_q["specialtyAddOns"]["wildfireFairPlan"]["annual"].as_i64(),
        Some(fair_ann + dic_ann)
    );

    // Case B: Synthetic Midwest holdout home (`synth-il-60201-newbuild-sfr`, built 2019) with Zone AE SFHA statutory flood
    let mut il_sfha = load_holdout_home_as_report("synth-il-60201-newbuild-sfr");
    il_sfha["hazards"] = json!({
        "flood": {
            "zone": "AE",
            "sfha": true,
            "nearestSfha": { "distM": 0 }
        },
        "earthquake": {
            "designMaps": { "pgaM": 0.05, "ss": 0.12, "sdc": "A" }
        }
    });
    let mw_q = build_insurance_quotes(&il_sfha);
    assert_zero_forbidden_substrings(&mw_q.to_string(), "Evanston IL SFHA JSON");
    assert_statutory_benchmark_schema(&mw_q);
    assert_eq!(
        mw_q["specialtyAddOns"]["flood"]["mandatory"].as_bool(),
        Some(true)
    );
    assert!(mw_q["specialtyAddOns"]["flood"]["annual"].as_i64().unwrap() >= 1_500);

    // Case C: Synthetic pre-1980 Campbell report comparing unretrofitted vs verified Brace+Bolt seismic retrofit permit & Zone VE statutory flood
    let mut base_pre1980 = common::synth_report_secondary();
    if let Some(obj) = base_pre1980.as_object_mut() {
        obj.remove("insuranceQuotes");
    }
    base_pre1980["facts"]["yearBuilt"] = json!(1925);
    base_pre1980["facts"]["permits"] = json!([]);
    base_pre1980["modules"]["hazards"]["data"]["flood"] = json!({ "zone": "VE", "sfha": true });

    let mut retrofitted = base_pre1980.clone();
    retrofitted["facts"]["permits"] = json!([
        { "date": "2021-06-15", "type": "Seismic", "description": "Earthquake Brace + Bolt foundation anchor bolt and cripple wall retrofit" }
    ]);

    let q_unretro = build_insurance_quotes(&base_pre1980);
    let q_retro = build_insurance_quotes(&retrofitted);

    assert_zero_forbidden_substrings(&q_unretro.to_string(), "unretrofitted VE JSON");
    assert_zero_forbidden_substrings(&q_retro.to_string(), "retrofitted VE JSON");
    assert_zero_fabricated_commercial_carrier_quotes(&q_unretro);
    assert_zero_fabricated_commercial_carrier_quotes(&q_retro);

    assert_eq!(
        q_unretro["specialtyAddOns"]["flood"]["femaZone"],
        json!("VE")
    );
    assert_eq!(
        q_unretro["specialtyAddOns"]["flood"]["mandatory"],
        json!(true)
    );
    assert_eq!(
        q_retro["specialtyAddOns"]["earthquake"]["retrofitVerified"],
        json!(true)
    );
    assert!(
        q_retro["specialtyAddOns"]["earthquake"]["annual"]
            .as_i64()
            .unwrap()
            < q_unretro["specialtyAddOns"]["earthquake"]["annual"]
                .as_i64()
                .unwrap(),
        "verified Brace+Bolt permit must lower annual seismic benchmark"
    );
}

#[test]
fn solar_pv_permit_never_triggers_reroof_and_missing_hazards_never_fakes_high_confidence_or_zone_x() {
    let mut rep = load_holdout_home_as_report("synth-ca-95008-compact-sfr");
    if let Some(fobj) = rep["facts"].as_object_mut() {
        fobj.remove("roofYear");
        fobj.remove("roofAgeYears");
        fobj.remove("permitNotes");
    }
    rep["facts"]["yearBuilt"] = json!(2013);
    rep["facts"]["roof"] = json!("Composition shingle");
    rep["facts"]["permits"] = json!([
        {
            "category": "solar",
            "rawType": "Roof Mounted PV",
            "type": "Roof Mounted PV",
            "description": "Roof Mounted PV System 2.870 kW (7 Panels)",
            "status": "Permit Issued",
            "issueDate": "2024-06-15",
            "finalDate": null,
            "finaled": false
        }
    ]);
    // Empty wildfire object + absent flood + absent earthquake PGA
    rep["modules"]["hazards"] = json!({ "data": { "wildfire": {}, "flood": { "checked": false } } });

    let ins_out = run_insurance_with_condition(
        &rep["facts"],
        &rep["modules"]["hazards"]["data"],
        &Value::Null,
        &rep["geo"],
        None,
        None,
    );
    assert_eq!(ins_out.data["roofUnderwriting"]["hasVerifiedReroofPermit"], json!(false));
    assert!(ins_out.data["roofUnderwriting"]["latestReroofPermitYear"].is_null());
    assert_eq!(ins_out.data["roofUnderwriting"]["estimatedAgeFromYearBuilt"], json!(true));
    assert!(
        ins_out.data["perilBreakdown"]["floodRequiredOrRecommended"]["femaZone"].is_null(),
        "missing flood data must not default femaZone to X"
    );
    assert_eq!(
        ins_out.data["perilBreakdown"]["floodRequiredOrRecommended"]["femaZoneVerified"],
        json!(false)
    );
    assert!(
        ins_out.data["perilBreakdown"]["floodRequiredOrRecommended"]["pricingBand"]
            .as_str()
            .unwrap_or("")
            .contains("Unverified")
    );

    let quotes = build_insurance_quotes(&rep);
    assert_eq!(quotes["underwritingProfile"]["roofPermitVerified"], json!(false));
    assert!(quotes["underwritingProfile"]["latestReroofYear"].is_null());
    assert_eq!(
        quotes["confidence"],
        json!("Medium"),
        "empty wildfire wrapper without flood/PGA must not emit High confidence"
    );
    let drivers_txt = quotes["riskDrivers"].to_string();
    assert!(
        !drivers_txt.contains("Verified recent reroof permit"),
        "Solar PV permit must never earn verified reroof credit: {drivers_txt}"
    );

    // Stage-2 `new_construction` contract with permitVerified=false must stay unverified for reroof
    let cond_new_const = json!({
        "systems": {
            "roof": {
                "verificationStatus": "new_construction",
                "reroofVerified": false,
                "replacementDateVerified": false,
                "newConstructionPermitYear": 2022,
                "effectiveYear": 2022,
                "effectiveYearSource": "new_construction_permit",
                "permitVerified": false
            }
        }
    });
    let ins_nc = run_insurance_with_condition(
        &rep["facts"],
        &rep["modules"]["hazards"]["data"],
        &Value::Null,
        &rep["geo"],
        Some(&cond_new_const),
        None,
    );
    assert_eq!(ins_nc.data["roofUnderwriting"]["hasVerifiedReroofPermit"], json!(false));
    assert_eq!(ins_nc.data["roofUnderwriting"]["verificationStatus"], json!("new_construction"));

    // Actual finaled reroof permit MUST verify and earn reroof credit
    let mut rep_reroof = rep.clone();
    rep_reroof["facts"]["permits"] = json!([
        {
            "category": "reroof",
            "type": "Reroof",
            "description": "Tear off and reroof composition shingle 22 squares",
            "status": "Finaled",
            "finalDate": "2024-04-10",
            "finaled": true
        }
    ]);
    let quotes_reroof = build_insurance_quotes(&rep_reroof);
    assert_eq!(quotes_reroof["underwritingProfile"]["roofPermitVerified"], json!(true));
    assert_eq!(quotes_reroof["underwritingProfile"]["latestReroofYear"], json!(2024));
    assert!(
        quotes_reroof["riskDrivers"]
            .to_string()
            .contains("Verified recent reroof permit"),
        "actual finaled reroof permit must earn reroof credit"
    );
}
