// Comprehensive Zero-Mock Integration, Document-Reconciliation, and Ephemeral Axum HTTP Suite
// for Commercial Real Estate (CRE) Mode (`feat/cre-mode`) using procedural synthetic data (`common::*`).

#![recursion_limit = "512"]

mod common;

use open_estate::ai::llm::{validate_json_schema, MemoryLlmCache};
use open_estate::ai::pdf::UploadedDocInput;
use open_estate::cre::{
    analyze_cre_documents, analyze_cre_trade_area, attach_cre_section_if_applicable,
    build_cre_report_section, classify_cre_asset, documents::cre_review_schema,
    parse_cre_documents_deterministic, redact_cre_document_pii, run_cre_underwriting,
    CreAssetClass, CreUnderwritingInput, CreUnderwritingOverrides,
};
use open_estate::db::DbStore;
use open_estate::guardrails::{HttpDefenseShield, SECURITY_RESPONSE_HEADERS};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

fn offline_env() -> EnvConfig {
    EnvConfig::default().with_overrides([
        ("OPENAI_API_KEY", String::new()),
        ("OPENAI_BASE_URL", String::new()),
        ("ATTOM_API_KEY", String::new()),
        ("RESO_BEARER_TOKEN", String::new()),
    ])
}

/// Constructs a synthetic Campbell, CA retail center report (`200 Synthcreplaza Ave, Campbell, CA 95008`)
/// backed by procedural synthetic Santa Clara County modules + an active LUST environmental record.
fn synth_campbell_retail_center_report() -> Value {
    let campbell = common::synth_report_secondary();
    let san_jose = common::synth_report_primary();

    let mut market_data = san_jose["modules"]["market"]["data"].clone();
    if let Some(geos) = market_data
        .pointer("/acs/geos")
        .and_then(Value::as_array)
        .cloned()
    {
        let find_metrics = |lvl: &str| -> Value {
            geos.iter()
                .find(|g| g["level"] == lvl)
                .map(|g| g.get("metrics").cloned().unwrap_or_else(|| g.clone()))
                .unwrap_or_else(|| json!({}))
        };
        market_data["acs"]["tract"] = find_metrics("tract");
        market_data["acs"]["zip"] = find_metrics("zip");
        market_data["acs"]["place"] = find_metrics("zip");
        market_data["acs"]["county"] = find_metrics("zip");
        market_data["acs"]["metro"] = find_metrics("zip");
    }

    let mut amen_data = campbell["modules"]["amenities"]["data"].clone();
    amen_data["lines"] = json!([
        { "kind": "arterial", "highway": "primary", "name": "Syntharterial Blvd", "distM": 16 }
    ]);
    amen_data["counts"] = json!({
        "restaurant": { "oneMile": 34 },
        "grocery": { "oneMile": 6 },
        "shopping": { "oneMile": 24 },
        "transit": { "halfMile": 8 },
        "evCharging": { "oneMile": 6 }
    });
    amen_data["proximity"] = json!({
        "freeway": { "distanceMi": 0.9 },
        "arterial": { "distanceMi": 0.01 },
        "rail": { "distanceMi": 1.2 },
        "majorAirport": { "distanceMi": 7.5 }
    });

    let mut civic_data = campbell["modules"]["civic"]["data"].clone();
    civic_data["walkability"] = json!({ "index": 12.83 });

    let mut env_data = campbell["modules"]["environment"]["data"].clone();
    env_data["sites"] = json!({
        "insideNplBoundary": false,
        "items": [
            {
                "program": "GeoTracker LUST",
                "name": "Synthetic Corner Station Case (SYNTH-LUST-001)",
                "status": "Open - Site Assessment",
                "distanceMi": 0.18,
                "chlorinatedSolvents": true
            }
        ],
        "nearestByProgram": {
            "lust": {
                "programLabel": "GeoTracker LUST",
                "name": "Synthetic Corner Station Case (SYNTH-LUST-001)",
                "status": "Open - Site Assessment",
                "distanceMi": 0.18
            }
        }
    });

    json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": "2026-11-10T19:12:29Z",
            "displayAddress": "200 Synthcreplaza Ave, Campbell, CA 95008",
            "slug": "200-synthcreplaza-ave-campbell-ca-95008",
            "timingsMs": {}
        },
        "geo": {
            "matched": true,
            "displayAddress": "200 Synthcreplaza Ave, Campbell, CA 95008",
            "lat": 37.2886,
            "lon": -121.9334,
            "city": "Campbell",
            "county": "Santa Clara County",
            "countyFips": "06085",
            "state": "CA",
            "zip": "95008",
            "msa": { "cbsa": "41940", "name": "San Jose-Sunnyvale-Santa Clara, CA" }
        },
        "facts": {
            "propertyType": "Retail Strip Center",
            "useCode": "RET-12",
            "zoning": "CG",
            "sqft": 12_000,
            "lotSqft": 36_000,
            "stories": 1,
            "yearBuilt": 1986,
            "listPrice": 6_800_000,
            "officialAssessment": {
                "assessedTotal": 3_200_000,
                "annualTax": 38_400,
                "statutoryTaxRatePct": 1.18
            },
            "taxHistory": [
                { "year": 2023, "assessed": 3_080_000, "tax": 36_500 },
                { "year": 2025, "assessed": 3_200_000, "tax": 38_400 }
            ]
        },
        "modules": {
            "market": { "data": market_data },
            "amenities": { "data": amen_data },
            "civic": { "data": civic_data },
            "hazards": campbell["modules"]["hazards"].clone(),
            "environment": { "data": env_data },
            "fengshui": {
                "data": {
                    "house": { "areaSqft": 12_000 },
                    "orientation": {
                        "street": { "name": "Syntharterial Blvd", "distM": 16 }
                    },
                    "streetsNear": [
                        { "name": "Syntharterial Blvd", "highway": "primary", "distM": 16 },
                        { "name": "Synthcross Way", "highway": "secondary", "distM": 38 }
                    ]
                }
            }
        },
        "warnings": [],
        "sources": []
    })
}

struct TestServerHandle {
    base_url: String,
    state: AppState,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServerHandle {
    async fn start(env_extra: Vec<(&str, String)>) -> Self {
        let temp_dir = std::env::temp_dir().join(format!("openestate-cre-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
        let db_path = temp_dir.join("cre_test.sqlite");

        let mut overrides = vec![
            ("SQLITE_DB_PATH", db_path.to_string_lossy().into_owned()),
            ("SQLITE_PATH", db_path.to_string_lossy().into_owned()),
            ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
            ("SYNC_INTERVAL_MIN", "0".to_string()),
            ("OPENAI_API_KEY", String::new()),
            ("OPENAI_BASE_URL", String::new()),
        ];
        overrides.extend(env_extra);

        let env = EnvConfig::default().with_overrides(overrides);
        let http = HttpClient::from_env(&env, Some(temp_dir.join("http_cache")));
        let db = DbStore::connect(&env).await.expect("connect test sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let state = AppState::with_components(
            env,
            http,
            db,
            HttpDefenseShield::default(),
            reports_dir,
            None,
        );
        let router = build_router(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        Self {
            base_url,
            state,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

#[test]
fn test_1_cre_classification_all_asset_classes_vs_synth_residential() {
    // 1. 5+ unit multifamily via units >= 5 & county useCode
    let mf_12 = classify_cre_asset(
        &json!({ "propertyType": "Apartment Building", "units": 12, "zoning": "R-4", "sqft": 10_400 }),
        None,
        None,
        false,
    );
    assert!(mf_12.is_cre);
    assert_eq!(mf_12.asset_class, Some(CreAssetClass::Multifamily5Plus));
    assert!(mf_12.agency_multifamily_eligible);
    assert!(!mf_12.sba_owner_user_compatible);

    let mf_code = classify_cre_asset(&json!({ "useCode": "MFR5" }), None, None, false);
    assert!(mf_code.is_cre);
    assert_eq!(mf_code.asset_class, Some(CreAssetClass::Multifamily5Plus));

    // 2. Retail / Strip Center via C-2 / CG zoning
    let retail = classify_cre_asset(
        &json!({ "zoning": "C-2", "useDescription": "Neighborhood Strip Center" }),
        None,
        None,
        false,
    );
    assert!(retail.is_cre);
    assert_eq!(retail.asset_class, Some(CreAssetClass::Retail));
    assert!(retail.sba_owner_user_compatible);
    assert!(!retail.agency_multifamily_eligible);

    // 3. Industrial / Warehouse / Flex via M-1 / M-2 / I-L zoning
    for z in ["M-1", "M-2", "I-L"] {
        let ind = classify_cre_asset(&json!({ "zoning": z, "sqft": 24_000 }), None, None, false);
        assert!(ind.is_cre, "zoning {z} should classify as CRE");
        assert_eq!(ind.asset_class, Some(CreAssetClass::Industrial));
        assert!(ind.sba_owner_user_compatible);
    }

    // 4. Mixed-Use via MU zoning and split commercial/residential units
    let mu = classify_cre_asset(
        &json!({ "zoning": "MU", "residentialUnits": 6, "commercialUnits": 2 }),
        None,
        None,
        false,
    );
    assert!(mu.is_cre);
    assert_eq!(mu.asset_class, Some(CreAssetClass::MixedUse));
    assert!(mu.sba_owner_user_compatible);
    assert!(mu.agency_multifamily_eligible);

    // 5. Office / Medical Condo, Hospitality, Commercial Land, Agricultural / Farmland
    let med = classify_cre_asset(
        &json!({ "propertyType": "Condo", "useDescription": "Medical Condo Suite", "zoning": "C-O" }),
        None,
        None,
        false,
    );
    assert!(med.is_cre);
    assert_eq!(med.asset_class, Some(CreAssetClass::Office));

    let hosp = classify_cre_asset(
        &json!({ "useDescription": "Boutique Hotel", "units": 36 }),
        None,
        None,
        false,
    );
    assert_eq!(hosp.asset_class, Some(CreAssetClass::Hospitality));

    let land = classify_cre_asset(
        &json!({ "zoning": "CG", "sqft": 0, "lotSqft": 43_560, "useDescription": "Vacant Commercial Pad Site" }),
        None,
        None,
        false,
    );
    assert_eq!(land.asset_class, Some(CreAssetClass::CommercialLand));

    let farm = classify_cre_asset(
        &json!({ "zoning": "A-1", "sqft": 0, "lotSqft": 3_484_800, "useDescription": "Irrigated Cropland" }),
        None,
        None,
        false,
    );
    assert_eq!(farm.asset_class, Some(CreAssetClass::AgriculturalFarmLand));

    // 6. 1–4 Unit Residential exclusions (synthetic SFR, PA Condo, Fourplex)
    let san_jose_sfr = common::synth_report_primary();
    let sfr_cls = classify_cre_asset(&san_jose_sfr["facts"], san_jose_sfr.get("geo"), None, false);
    assert!(!sfr_cls.is_cre);
    assert!(sfr_cls.residential_reason.is_some());

    let pa_condo = common::synth_holdout_by_id("synth-pa-19106-condo");
    let condo_cls = classify_cre_asset(
        &pa_condo["raw"]["facts"],
        pa_condo["raw"].get("geo"),
        None,
        false,
    );
    assert!(!condo_cls.is_cre);

    let fourplex = classify_cre_asset(
        &json!({ "propertyType": "Fourplex", "units": 4, "zoning": "R-3" }),
        None,
        None,
        false,
    );
    assert!(!fourplex.is_cre);

    // 7. Explicit mode=cre & explicit asset_class override
    let forced = classify_cre_asset(&san_jose_sfr["facts"], san_jose_sfr.get("geo"), None, true);
    assert!(forced.is_cre);
    assert_eq!(forced.confidence, "explicit_override");

    let explicit_ind = classify_cre_asset(
        &san_jose_sfr["facts"],
        san_jose_sfr.get("geo"),
        Some("industrial"),
        false,
    );
    assert!(explicit_ind.is_cre);
    assert_eq!(explicit_ind.asset_class, Some(CreAssetClass::Industrial));
}

#[test]
fn test_2_cre_underwriting_noi_prop13_sensitivity_dscr_and_multi_state_tax_regimes() {
    let report = synth_campbell_retail_center_report();
    let cls = classify_cre_asset(&report["facts"], report.get("geo"), None, false);
    assert_eq!(cls.asset_class, Some(CreAssetClass::Retail));

    let ov = CreUnderwritingOverrides {
        purchase_price: Some(6_800_000.0),
        gross_sqft: Some(12_000.0),
        lease_structure: Some("nnn".to_string()),
        rent_per_sqft_yr: Some(36.0),
        other_income_annual: Some(8_000.0),
        vacancy_pct: Some(7.0),
        in_place_property_tax_annual: Some(38_400.0),
        property_tax_rate_pct: Some(1.18),
        conventional_rate_pct: Some(7.25),
        conventional_ltv_pct: Some(65.0),
        conventional_amort_years: Some(25),
        conventional_term_years: Some(7),
        min_dscr: Some(1.25),
        owner_occupancy_pct: Some(60.0),
        selected_loan_program: Some("conventional_cre".to_string()),
        ..Default::default()
    };

    let inp = CreUnderwritingInput::from_report_and_overrides(
        &report,
        CreAssetClass::Retail,
        &cls,
        Some(&ov),
    );
    let out = run_cre_underwriting(&inp);
    let uw = &out.data;

    assert_eq!(
        uw.pointer("/income/grossPotentialRent")
            .and_then(Value::as_i64),
        Some(432_000)
    );
    assert_eq!(
        uw.pointer("/income/vacancyAndCreditLossPct")
            .and_then(Value::as_f64),
        Some(7.0)
    );
    let egi = uw
        .pointer("/income/effectiveGrossIncome")
        .and_then(Value::as_i64)
        .unwrap();
    let reimb = uw
        .pointer("/income/expenseReimbursements")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(reimb > 50_000);
    assert!(egi > 440_000);

    // California Prop 13 reassessment shock & Split-Roll note
    let tax = uw.pointer("/operatingExpenses/propertyTaxes").unwrap();
    assert_eq!(
        tax.get("inPlaceAnnual").and_then(Value::as_i64),
        Some(38_400)
    );
    assert_eq!(
        tax.get("postAcquisitionAnnual").and_then(Value::as_i64),
        Some(80_240)
    );
    assert_eq!(
        tax.get("reassessmentShockAnnual").and_then(Value::as_i64),
        Some(41_840)
    );
    assert!(
        tax.get("year1SupplementalBillEstimate")
            .and_then(Value::as_i64)
            .unwrap()
            > 0
    );
    assert_eq!(
        tax.get("annualGrowthCapPct").and_then(Value::as_f64),
        Some(2.0)
    );
    assert!(tax
        .get("splitRollRiskNote")
        .and_then(Value::as_str)
        .unwrap_or("")
        .contains("Split-Roll"));

    // Management fee & replacement CapEx reserves
    let mgmt_pct = uw
        .pointer("/operatingExpenses/propertyManagement/pctOfEgi")
        .and_then(Value::as_f64)
        .unwrap();
    assert!((3.0..=6.0).contains(&mgmt_pct));
    assert!(
        uw.pointer("/operatingExpenses/replacementCapexReserves/perSqftYr")
            .and_then(Value::as_f64)
            .unwrap()
            > 0.0
    );

    // 3-Way Lease Structure comparison (NNN vs MG vs FSG)
    let noi_nnn = uw
        .pointer("/leaseStructureComparison/nnn/noi")
        .and_then(Value::as_i64)
        .unwrap();
    let noi_mg = uw
        .pointer("/leaseStructureComparison/modifiedGross/noi")
        .and_then(Value::as_i64)
        .unwrap();
    let noi_fsg = uw
        .pointer("/leaseStructureComparison/fullServiceGross/noi")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(noi_nnn > noi_mg && noi_mg > noi_fsg);

    // 5-Point Cap Rate Sensitivity Table (-100 / -50 / Base / +50 / +100 bps)
    let sens = uw
        .pointer("/capRateMatrix/sensitivityTable")
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(sens.len(), 5);
    assert_eq!(sens[0].get("spreadBps").and_then(Value::as_i64), Some(-100));
    assert_eq!(sens[2].get("label").and_then(Value::as_str), Some("Base"));
    assert_eq!(sens[4].get("spreadBps").and_then(Value::as_i64), Some(100));
    for i in 0..4 {
        let v_hi = sens[i].get("impliedValue").and_then(Value::as_i64).unwrap();
        let v_lo = sens[i + 1]
            .get("impliedValue")
            .and_then(Value::as_i64)
            .unwrap();
        assert!(v_hi > v_lo);
    }

    // DSCR-constrained Conventional CRE loan + SBA 504 + SBA 7(a) ($5M cap) + Agency Multifamily
    let conv = uw.pointer("/debtComparison/conventionalCre").unwrap();
    let max_ltv = conv.get("maxLoanByLtv").and_then(Value::as_i64).unwrap();
    let max_dscr = conv.get("maxLoanByDscr").and_then(Value::as_i64).unwrap();
    let conv_loan = conv.get("loanAmount").and_then(Value::as_i64).unwrap();
    assert_eq!(max_ltv, 4_420_000);
    assert!(max_dscr < max_ltv);
    assert_eq!(conv_loan, max_dscr);
    assert_eq!(
        conv.get("bindingConstraint").and_then(Value::as_str),
        Some("DSCR")
    );
    assert_eq!(
        conv.get("dscrConstrained").and_then(Value::as_bool),
        Some(true)
    );

    let sba504 = uw.pointer("/debtComparison/sba504").unwrap();
    assert_eq!(
        sba504
            .get("ownerOccupiedQualifiesNow")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        sba504.get("bankFirstLienLoan").and_then(Value::as_i64),
        Some(3_400_000)
    );
    assert_eq!(
        sba504.get("cdcDebentureSecondLoan").and_then(Value::as_i64),
        Some(2_720_000)
    );
    assert_eq!(
        sba504.get("downPayment").and_then(Value::as_i64),
        Some(680_000)
    );

    let sba7a = uw.pointer("/debtComparison/sba7a").unwrap();
    assert_eq!(
        sba7a.get("loanAmount").and_then(Value::as_i64),
        Some(5_000_000)
    );
    assert_eq!(
        sba7a.get("cappedAtStatutoryMax").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        uw.pointer("/debtComparison/agencyMultifamily/eligible")
            .and_then(Value::as_bool),
        Some(false)
    );

    let im = uw.pointer("/investorMetrics").unwrap();
    let em5 = im
        .get("fiveYearEquityMultiple")
        .and_then(Value::as_f64)
        .unwrap();
    let em10 = im
        .get("tenYearEquityMultiple")
        .and_then(Value::as_f64)
        .unwrap();
    assert!(em10 > em5 && em5 > 1.0);
    let uw_disc = uw.get("disclaimer").and_then(Value::as_str).unwrap_or("");
    assert!(
        uw_disc.contains("NOT a commercial appraisal")
            && uw_disc.contains("lending commitment")
            && uw_disc.contains("Phase I ESA")
    );

    // Multi-state commercial tax regimes on synthetic Cook County IL & Miami-Dade FL geo records
    let il_holdout = common::synth_holdout_by_id("synth-il-60201-classic-sfr");
    let il_office = json!({
        "geo": il_holdout["raw"]["geo"],
        "facts": { "propertyType": "Office Building", "zoning": "C-O", "sqft": 18_000, "purchasePrice": 4_200_000 },
        "modules": { "market": { "data": il_holdout["raw"]["market"] } }
    });
    let il_sec = build_cre_report_section(&il_office, None, false);
    assert_eq!(
        il_sec
            .data
            .pointer("/underwriting/income/vacancyAndCreditLossPct")
            .and_then(Value::as_f64),
        Some(12.0)
    );
    assert!(il_sec
        .data
        .pointer("/underwriting/operatingExpenses/propertyTaxes/regime")
        .and_then(Value::as_str)
        .unwrap_or("")
        .contains("Illinois"));

    let fl_holdout = common::synth_holdout_by_id("synth-fl-33134-remod-sfr");
    let fl_ind = json!({
        "geo": fl_holdout["raw"]["geo"],
        "facts": { "propertyType": "Industrial Warehouse", "zoning": "I-L", "sqft": 25_000, "purchasePrice": 5_000_000 },
        "modules": { "market": { "data": fl_holdout["raw"]["market"] } }
    });
    let fl_sec = build_cre_report_section(&fl_ind, None, false);
    assert_eq!(
        fl_sec
            .data
            .pointer("/underwriting/income/vacancyAndCreditLossPct")
            .and_then(Value::as_f64),
        Some(7.0)
    );
    assert!(fl_sec
        .data
        .pointer("/underwriting/operatingExpenses/propertyTaxes/regime")
        .and_then(Value::as_str)
        .unwrap_or("")
        .contains("Florida"));

    // Multifamily 5+ Agency Eligibility (30-year amortization, 5% vacancy)
    let mf_rep = json!({
        "geo": { "state": "TX", "city": "Austin", "county": "Travis County", "msa": { "name": "Austin-Round Rock, TX" } },
        "facts": { "propertyType": "Multifamily Apartment", "units": 24, "sqft": 20_400 },
        "modules": { "market": { "data": { "rates": { "mort30": 6.40 } } } }
    });
    let mf_sec = build_cre_report_section(&mf_rep, None, false);
    assert_eq!(
        mf_sec
            .data
            .pointer("/underwriting/income/vacancyAndCreditLossPct")
            .and_then(Value::as_f64),
        Some(5.0)
    );
    assert_eq!(
        mf_sec
            .data
            .pointer("/underwriting/debtComparison/agencyMultifamily/eligible")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        mf_sec
            .data
            .pointer("/underwriting/debtComparison/agencyMultifamily/amortizationYears")
            .and_then(Value::as_i64),
        Some(30)
    );
}

#[test]
fn test_3_trade_area_frontage_aadt_demographics_and_site_envelope_on_synth_corridor() {
    let report = synth_campbell_retail_center_report();
    let ta = analyze_cre_trade_area(&report, CreAssetClass::Retail).data;

    let ft = ta.get("frontageTraffic").unwrap();
    assert_eq!(
        ft.get("frontingStreetName").and_then(Value::as_str),
        Some("Syntharterial Blvd")
    );
    assert_eq!(
        ft.get("osmHighwayClass").and_then(Value::as_str),
        Some("primary")
    );
    assert_eq!(
        ft.get("cornerLotLikely").and_then(Value::as_bool),
        Some(true)
    );
    let aadt_mid = ft
        .pointer("/modeledAadt/mid")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(
        aadt_mid >= 20_000,
        "primary corridor with POI density should yield >=20k AADT, got {aadt_mid}"
    );
    assert!(ft.get("visibilityScore").and_then(Value::as_i64).unwrap() >= 85);
    assert!(
        ft.get("ingressEgressScore")
            .and_then(Value::as_i64)
            .unwrap()
            >= 85
    );

    let rings = ta.get("tradeAreaRings").and_then(Value::as_array).unwrap();
    assert_eq!(rings.len(), 3);
    let pop1 = rings[0]
        .get("estimatedPopulation")
        .and_then(Value::as_i64)
        .unwrap();
    let pop3 = rings[1]
        .get("estimatedPopulation")
        .and_then(Value::as_i64)
        .unwrap();
    let pop5 = rings[2]
        .get("estimatedPopulation")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(pop1 < pop3 && pop3 < pop5);
    assert_eq!(
        rings[0]
            .get("medianHouseholdIncome")
            .and_then(Value::as_i64),
        Some(172_480),
        "1-mile ring must reflect synthetic Santa Clara County tract MHI ($172,480)"
    );
    assert!(
        rings[0]
            .get("aggregatePurchasingPowerUsd")
            .and_then(Value::as_i64)
            .unwrap()
            > 100_000_000
    );

    let se = ta.get("siteEnvelope").unwrap();
    assert_eq!(se.get("lotSqft").and_then(Value::as_i64), Some(36_000));
    assert_eq!(se.get("far").and_then(Value::as_f64), Some(0.33));
    assert_eq!(se.get("bcrPct").and_then(Value::as_f64), Some(33.3));
    assert_eq!(
        se.pointer("/evAndTransitAccess/epaWalkabilityIndex")
            .and_then(Value::as_f64),
        Some(12.83)
    );
    assert!(
        se.pointer("/freightAndCorridorProximity/freightLogisticsScore")
            .and_then(Value::as_i64)
            .unwrap()
            >= 75
    );
}

#[tokio::test]
async fn test_4_deterministic_cre_document_reconciliation_phase1_geotracker_and_schema() {
    let report = synth_campbell_retail_center_report();
    let env = offline_env();
    let http = HttpClient::from_env(&env, None);

    let docs = vec![
        UploadedDocInput {
            name: "Offering_Memorandum_Synth_Center.txt".into(),
            text: Some(
                "[Page 1]\n\
                 OFFERING MEMORANDUM — 200 SYNTHCREPLAZA AVE, CAMPBELL, CA 95008\n\
                 Property Type: Retail Strip Center (12,000 RSF)\n\
                 Offering Price: $6,800,000\n\
                 Broker Pro-Forma Gross Potential Rent: $460,000\n\
                 Broker Pro-Forma NOI: $435,000\n\
                 Pro-Forma Cap Rate: 6.40%\n\
                 Pro-Forma Property Taxes: $39,500\n\
                 Vesting: Synth Valley Commercial Holdings LLC\n"
                    .into(),
            ),
            base64: None,
        },
        UploadedDocInput {
            name: "Rent_Roll_Schedule.txt".into(),
            text: Some(
                "[Page 1]\n\
                 RENT ROLL — As of 2026-11-01\n\
                 | Suite | Tenant | SqFt | Lease Start | Lease Expiration | Base Rent | Escalations | Lease Type |\n\
                 |---|---|---|---|---|---|---|---|\n\
                 | Suite 101 | Silicon Valley Dental Partners LLC | 4,000 SF | 2021-01-01 | 2027-06-30 | $38.00/SF/yr | 3.0% annual | NNN |\n\
                 | Suite 102 | Campbell Valet Dry Cleaners Inc. | 3,000 SF | 2020-05-01 | 2027-09-30 | $34.00/SF/yr | CPI annual | NNN |\n\
                 | Suite 103 | Synth Physical Therapy PC | 3,000 SF | 2023-03-01 | 2030-03-31 | $32.00/SF/yr | 3.0% annual | NNN |\n\
                 | Suite 104 | Vacant | 2,000 SF | - | - | $0.00/SF/yr | - | Vacant |\n"
                    .into(),
            ),
            base64: None,
        },
        UploadedDocInput {
            name: "Trailing_12_Operating_Statement.txt".into(),
            text: Some(
                "[Page 1]\n\
                 TRAILING-12 (T-12) OPERATING STATEMENT — Statement Date: 2026-10-31\n\
                 T-12 Rental Collections: $312,000\n\
                 T-12 Property Taxes: $38,400\n\
                 T-12 Insurance: $4,200\n\
                 T-12 Total Operating Expenses: $78,000\n\
                 T-12 Net Operating Income: $234,000\n"
                    .into(),
            ),
            base64: None,
        },
        UploadedDocInput {
            name: "NNN_Lease_Abstract_Suite_101.txt".into(),
            text: Some(
                "[Page 1]\n\
                 COMMERCIAL NNN LEASE ABSTRACT\n\
                 Suite: Suite 101, 4,000 SF\n\
                 Tenant: Silicon Valley Dental Partners LLC\n\
                 Lease Start: 2021-01-01\n\
                 Lease Expiration: 2027-06-30\n\
                 Base Rent: $38.00/SF/yr\n\
                 Annual Escalations: 3.0% fixed annual\n\
                 Lease Type: Triple Net (NNN)\n\
                 CAM Cap: 4% non-cumulative cap on controllable CAM\n\
                 Renewal Options: Two (2) 5-year options at 95% FMV\n\
                 Exclusivity Clause: Exclusive dental/orthodontic use in center\n\
                 Co-Tenancy Clause: Requires 75% center occupancy\n\
                 TI Allowance: $25.00/SF landlord turnkey\n\
                 Corporate Guaranty: Corporate guaranty by Pacific Healthcare Partners LLC\n"
                    .into(),
            ),
            base64: None,
        },
        UploadedDocInput {
            name: "Phase_I_ESA_ASTM_E1527_21.txt".into(),
            text: Some(
                "[Page 1]\n\
                 ASTM E1527-21 PHASE I ENVIRONMENTAL SITE ASSESSMENT\n\
                 Subject: 200 Synthcreplaza Ave, Campbell, CA 95008\n\
                 REC: Historical dry cleaning solvent (perchloroethylene / PCE) and nearby leaking underground storage tank (LUST) constitute active Recognized Environmental Conditions.\n\
                 Vapor Intrusion: Sub-slab soil-gas vapor intrusion screen recommended.\n\
                 Phase II subsurface investigation recommended prior to loan closing.\n"
                    .into(),
            ),
            base64: None,
        },
    ];

    let out = analyze_cre_documents(&http, &env, &report, docs, None)
        .await
        .expect("analyze_cre_documents deterministic");
    let d = &out.data;

    assert_eq!(
        d.get("source").and_then(Value::as_str),
        Some("deterministic")
    );
    let serialized = serde_json::to_string(d).unwrap();
    assert!(serialized.contains("Silicon Valley Dental Partners LLC"));
    assert!(serialized.contains("Campbell Valet Dry Cleaners Inc."));

    let recon = d.get("reconciliation").unwrap();
    assert_eq!(
        recon.get("omProFormaNoi").and_then(Value::as_i64),
        Some(435_000)
    );
    assert_eq!(
        recon.get("t12ActualNoi").and_then(Value::as_i64),
        Some(234_000)
    );
    assert!(
        recon
            .get("proFormaVsT12NoiInflationPct")
            .and_then(Value::as_f64)
            .unwrap()
            > 80.0
    );

    let phys_occ = recon
        .get("physicalOccupancyPct")
        .and_then(Value::as_f64)
        .unwrap();
    let econ_occ = recon
        .get("economicOccupancyPct")
        .and_then(Value::as_f64)
        .unwrap();
    assert!((phys_occ - 83.3).abs() < 0.2);
    assert!(econ_occ < phys_occ - 10.0);

    let roll24_pct = recon
        .pointer("/rolloverSchedule/expiring24MonthsSqftPct")
        .and_then(Value::as_f64)
        .unwrap();
    assert!((roll24_pct - 70.0).abs() < 0.2);
    assert_eq!(
        recon
            .pointer("/rolloverSchedule/rolloverCliffRisk")
            .and_then(Value::as_str),
        Some("critical")
    );

    let flags: Vec<&str> = recon
        .get("discrepancyFlags")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|f| f.get("code").and_then(Value::as_str))
        .collect();
    for expected in [
        "PRO_FORMA_NOI_INFLATION",
        "ECONOMIC_OCCUPANCY_GAP",
        "UNDERSTATED_POST_SALE_TAXES",
        "LEASE_ROLLOVER_CLIFF_24M",
        "PHASE_I_ESA_REC_IDENTIFIED",
    ] {
        assert!(
            flags.contains(&expected),
            "missing expected discrepancy flag {expected} in {flags:?}"
        );
    }

    let leases = d.get("leaseAbstracts").and_then(Value::as_array).unwrap();
    assert_eq!(leases.len(), 4);
    let s101 = leases
        .iter()
        .find(|l| l.get("suite").and_then(Value::as_str) == Some("Suite 101"))
        .unwrap();
    assert_eq!(
        s101.get("tenantLabel").and_then(Value::as_str),
        Some("Silicon Valley Dental Partners LLC")
    );
    assert_eq!(s101.get("sqft").and_then(Value::as_i64), Some(4_000));
    assert_eq!(s101.get("leaseType").and_then(Value::as_str), Some("NNN"));
    assert!(s101
        .get("camOpexCap")
        .and_then(Value::as_str)
        .unwrap()
        .contains("4%"));
    assert!(s101
        .get("exclusivityClause")
        .and_then(Value::as_str)
        .unwrap()
        .contains("dental"));
    assert!(s101
        .get("coTenancyClause")
        .and_then(Value::as_str)
        .unwrap()
        .contains("75%"));
    assert!(s101
        .get("tiAllowance")
        .and_then(Value::as_str)
        .unwrap()
        .contains("$25.00"));

    // Phase I ESA corroborated against synthetic GeoTracker LUST record (`SYNTH-LUST-001`)
    let env_xc = d.get("environmentalAndSurveyCrossCheck").unwrap();
    assert_eq!(
        env_xc.get("phaseIEsaDetected").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        env_xc.get("vaporIntrusionFlag").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        env_xc.get("phaseIiRecommended").and_then(Value::as_bool),
        Some(true)
    );
    assert!(!env_xc
        .get("confirmations")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());

    // Contradiction check when a Phase I claims "No RECs" adjacent to the open LUST case
    let contra_out = analyze_cre_documents(
        &http,
        &env,
        &report,
        vec![UploadedDocInput {
            name: "Clean_Phase_I_Summary.txt".into(),
            text: Some(
                "ASTM E1527-21 PHASE I ENVIRONMENTAL SITE ASSESSMENT\n\
                 Executive Summary: This assessment revealed no evidence of Recognized Environmental Conditions (RECs)."
                    .into(),
            ),
            base64: None,
        }],
        None,
    )
    .await
    .expect("contradiction check");
    assert!(!contra_out
        .data
        .pointer("/environmentalAndSurveyCrossCheck/contradictions")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());

    // Corporate vs personal guarantor redaction rules (`redact_cre_document_pii`)
    let pii_check = redact_cre_document_pii(
        "Vesting: Synth Valley Commercial Holdings LLC\nCorporate Guaranty: Pacific Healthcare Partners LLC\nGuarantor: Silicon Valley Dental Partners LLC (Personal Guarantor: Thomas Alan Wright)",
    );
    assert!(pii_check
        .masked_text
        .contains("Synth Valley Commercial Holdings LLC"));
    assert!(pii_check
        .masked_text
        .contains("Pacific Healthcare Partners LLC"));
    assert!(pii_check
        .masked_text
        .contains("Silicon Valley Dental Partners LLC"));
    assert!(!pii_check.masked_text.contains("Thomas Alan Wright"));
    assert!(pii_check
        .masked_text
        .contains("[REDACTED_INDIVIDUAL_GUARANTOR]"));

    // Direct HREC / CREC & negated-REC checks + `cre_review_schema()` validation
    let det_direct = parse_cre_documents_deterministic(
        "Offering Memorandum\nPro-Forma NOI: $500,000\nT-12 NOI: $380,000\n\
         Phase I ESA (ASTM E1527-21)\n\
         HREC: Closed 1998 auto body paint booth and underground tank removal with NFA.\n\
         CREC: Sub-slab vapor barrier maintained under ongoing Activity and Use Limitation.\n",
        None,
    );
    assert_eq!(
        det_direct
            .pointer("/reconciliation/proFormaVsT12NoiDelta")
            .and_then(Value::as_i64),
        Some(120_000)
    );
    assert_eq!(
        det_direct
            .pointer("/environmentalAndSurveyCrossCheck/hrecs/0/type")
            .and_then(Value::as_str),
        Some("HREC")
    );
    assert_eq!(
        det_direct
            .pointer("/environmentalAndSurveyCrossCheck/crecs/0/type")
            .and_then(Value::as_str),
        Some("CREC")
    );

    for clean_phase1 in [
        "Phase I ESA (ASTM E1527-21)\nRecognized Environmental Conditions: None.",
        "Phase I Environmental Site Assessment\nREC: None",
        "ASTM E1527-21 Report\nZero Recognized Environmental Conditions were identified.",
    ] {
        let neg_rec = parse_cre_documents_deterministic(clean_phase1, None);
        assert_eq!(
            neg_rec
                .pointer("/environmentalAndSurveyCrossCheck/claimsNoRecs")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(neg_rec
            .pointer("/environmentalAndSurveyCrossCheck/recs")
            .and_then(Value::as_array)
            .unwrap()
            .is_empty());
    }

    let schema = cre_review_schema();
    assert_eq!(
        schema.get("additionalProperties").and_then(Value::as_bool),
        Some(false)
    );
    let valid_payload = json!({
        "executiveHeadline": "T-12 NOI trails broker Pro-Forma by $201k with 70% 24-month rollover cliff.",
        "reconciliationSummary": "Re-underwrite acquisition to actual T-12 NOI adjusted for Prop 13 reassessment.",
        "negotiationLevers": ["Anchor offer to T-12 NOI adjusted for post-sale Prop 13 taxes."],
        "sellerAndBrokerQuestions": ["Provide 24 months of bank rent collection statements."],
        "additionalLeaseAbstracts": []
    });
    assert!(validate_json_schema(&schema, &valid_payload).is_empty());
}

#[tokio::test]
async fn test_5_orchestrator_attachment_and_http_endpoints_underwrite_and_cre_review() {
    let mut san_jose_sfr = common::synth_report_primary();
    assert!(!attach_cre_section_if_applicable(
        &mut san_jose_sfr,
        None,
        false
    ));
    assert!(san_jose_sfr.get("cre").is_none());

    let mut mf_report = synth_campbell_retail_center_report();
    mf_report["facts"] = json!({
        "propertyType": "Apartment",
        "units": 8,
        "zoning": "R-4",
        "sqft": 7_200,
        "cre": 1
    });
    assert!(attach_cre_section_if_applicable(
        &mut mf_report,
        None,
        false
    ));
    assert_eq!(
        mf_report.pointer("/cre/enabled").and_then(Value::as_bool),
        Some(true)
    );
    assert!(mf_report
        .pointer("/modules/cre/data/underwriting")
        .is_some());

    let srv = TestServerHandle::start(vec![]).await;
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap();

    let cre_rep = synth_campbell_retail_center_report();
    let cre_slug = cre_rep
        .pointer("/meta/slug")
        .and_then(Value::as_str)
        .unwrap();
    srv.state
        .db
        .put_cached_report(cre_slug, &cre_rep, Some("<html>CRE</html>"))
        .await
        .expect("seed cre report");

    let res_cached = common::synth_report_primary();
    let res_slug = res_cached
        .pointer("/meta/slug")
        .and_then(Value::as_str)
        .unwrap();
    srv.state
        .db
        .put_cached_report(res_slug, &res_cached, Some("<html>RES</html>"))
        .await
        .expect("seed synthetic San Jose residential report");

    // A) POST /api/cre/underwrite on cached CRE slug with overrides
    let uw_resp = client
        .post(format!("{}/api/cre/underwrite", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": cre_slug,
            "purchasePrice": 7_000_000,
            "rentPerSqftYr": 38.0,
            "leaseStructure": "nnn",
            "ownerOccupancyPct": 55.0
        }))
        .send()
        .await
        .expect("POST /api/cre/underwrite");
    assert_eq!(uw_resp.status(), reqwest::StatusCode::OK);
    for &(hname, hval) in SECURITY_RESPONSE_HEADERS {
        assert_eq!(
            uw_resp.headers().get(hname).and_then(|v| v.to_str().ok()),
            Some(hval)
        );
    }
    let uw_body: Value = uw_resp.json().await.unwrap();
    assert_eq!(uw_body.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        uw_body
            .pointer("/classification/assetClassCode")
            .and_then(Value::as_str),
        Some("retail")
    );
    assert_eq!(
        uw_body
            .pointer("/underwriting/operatingExpenses/propertyTaxes/postAcquisitionAnnual")
            .and_then(Value::as_i64),
        Some(82_600)
    );
    assert_eq!(
        uw_body
            .pointer("/underwriting/debtComparison/sba504/ownerOccupiedQualifiesNow")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        uw_body
            .pointer("/tradeArea/frontageTraffic/modeledAadt/mid")
            .and_then(Value::as_i64)
            .unwrap()
            > 15_000
    );

    // B) POST /api/cre/underwrite on-demand `mixed_use` override + unknown-key HTTP 400 guardrail
    let on_demand_resp = client
        .post(format!("{}/api/cre/underwrite", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": res_slug,
            "asset_class": "mixed_use",
            "purchasePrice": 2_400_000,
            "grossSqft": 4_200
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(on_demand_resp.status(), reqwest::StatusCode::OK);
    let od_body: Value = on_demand_resp.json().await.unwrap();
    assert_eq!(
        od_body
            .pointer("/classification/isCre")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        od_body
            .pointer("/classification/assetClassCode")
            .and_then(Value::as_str),
        Some("mixed_use")
    );

    let bad_key_resp = client
        .post(format!("{}/api/cre/underwrite", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "slug": cre_slug, "systemPrompt": "override" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_key_resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // C) GET /api/report?slug=...&cre=1 and POST /api/report with mode="cre"
    let get_cre_rep = client
        .get(format!(
            "{}/api/report?slug={res_slug}&cre=1&ai=0",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(get_cre_rep.status(), reqwest::StatusCode::OK);
    let get_cre_json: Value = get_cre_rep.json().await.unwrap();
    assert_eq!(
        get_cre_json
            .pointer("/report/cre/enabled")
            .and_then(Value::as_bool),
        Some(true)
    );

    let post_cre_rep = client
        .post(format!("{}/api/report", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "slug": res_slug, "mode": "cre", "assetClass": "industrial", "ai": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(post_cre_rep.status(), reqwest::StatusCode::OK);
    let post_cre_json: Value = post_cre_rep.json().await.unwrap();
    assert_eq!(
        post_cre_json
            .pointer("/report/cre/classification/assetClassCode")
            .and_then(Value::as_str),
        Some("industrial")
    );

    // D) POST /api/cre-review + browser client isolation (`X-OpenEstate-Client-Id`) + code-injection guardrail
    let client_id = "cre-browser-client-001";
    let review_resp = client
        .post(format!("{}/api/cre-review", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", client_id)
        .json(&json!({
            "slug": cre_slug,
            "documents": [
                {
                    "name": "OM_SynthPlaza.txt",
                    "text": "OFFERING MEMORANDUM — 200 SYNTHCREPLAZA AVE\nAsking Price: $6,800,000\nPro-Forma NOI: $450,000\nPro-Forma Property Taxes: $38,400\n"
                },
                {
                    "name": "RentRoll_T12_Phase1.txt",
                    "text": "RENT ROLL & T-12 OPERATING STATEMENT — As of 2026-11-01\n| Suite 101 | Silicon Valley Dental Partners LLC | 6,000 SF | 2022-01-01 | 2027-06-30 | $36.00/SF/yr | 3% | NNN |\n| Suite 102 | Pacific Logistics Inc. | 4,000 SF | 2022-01-01 | 2031-01-01 | $34.00/SF/yr | 3% | NNN |\n| Suite 103 | Vacant | 2,000 SF | - | - | $0.00/SF/yr | - | Vacant |\nT-12 Rental Collections: $290,000\nT-12 NOI: $245,000\nPhase I ESA (ASTM E1527-21): REC - Former dry cleaner PCE vapor intrusion flagged.\n"
                }
            ]
        }))
        .send()
        .await
        .expect("POST /api/cre-review");
    assert_eq!(review_resp.status(), reqwest::StatusCode::OK);
    let rev_body: Value = review_resp.json().await.unwrap();
    assert_eq!(rev_body.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        rev_body
            .pointer("/data/reconciliation/omProFormaNoi")
            .and_then(Value::as_i64),
        Some(450_000)
    );
    assert_eq!(
        rev_body
            .pointer("/data/reconciliation/t12ActualNoi")
            .and_then(Value::as_i64),
        Some(245_000)
    );
    assert_eq!(
        rev_body
            .pointer("/data/reconciliation/rolloverSchedule/expiring24MonthsSqftPct")
            .and_then(Value::as_f64),
        Some(60.0)
    );
    assert_eq!(
        rev_body
            .pointer("/data/environmentalAndSurveyCrossCheck/vaporIntrusionFlag")
            .and_then(Value::as_bool),
        Some(true)
    );

    let get_same = client
        .get(format!("{}/api/cre-review?slug={cre_slug}", srv.base_url))
        .header("X-OpenEstate-Client-Id", client_id)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(get_same.get("data").unwrap().is_object());

    let get_other = client
        .get(format!("{}/api/cre-review?slug={cre_slug}", srv.base_url))
        .header("X-OpenEstate-Client-Id", "other-browser-client-999")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(get_other.get("data").unwrap().is_null());

    let code_rej = client
        .post(format!("{}/api/cre-review", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": cre_slug,
            "documents": [{
                "name": "script.txt",
                "text": "def run_exploit():\n    return 1\nasync def main():\n    pass\n```python\nprint('x')\n```"
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(code_rej.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_6_live_llm_cre_document_reconciliation_when_configured() {
    let env =
        EnvConfig::from_process_and_dotenv(Some(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))));
    let api_key = env.get("OPENAI_API_KEY").unwrap_or("");
    if !env.llm_configured()
        || api_key.trim().is_empty()
        || api_key.contains("your-")
        || api_key.starts_with("test")
    {
        eprintln!("Skipping live LLM CRE document check in test_6: OPENAI_BASE_URL / OPENAI_API_KEY not set");
        return;
    }

    let report = synth_campbell_retail_center_report();
    let http = HttpClient::from_env(&env, None);
    let cache = MemoryLlmCache::default();
    let docs = vec![UploadedDocInput {
        name: "OM_and_T12_SynthPlaza.txt".into(),
        text: Some(
            "OFFERING MEMORANDUM & T-12 — 200 SYNTHCREPLAZA AVE, CAMPBELL, CA 95008\n\
             Offering Price: $6,800,000\n\
             Pro-Forma NOI: $435,000\n\
             T-12 NOI: $234,000\n\
             | Suite 101 | Silicon Valley Dental Partners LLC | 4,000 SF | 2021-01-01 | 2027-06-30 | $38.00/SF/yr | 3% | NNN |\n"
                .into(),
        ),
        base64: None,
    }];

    let out = analyze_cre_documents(&http, &env, &report, docs, Some(&cache))
        .await
        .expect("live LLM CRE document review");
    assert!(
        out.data
            .get("executiveHeadline")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
            || out.data.get("reconciliation").is_some(),
        "expected CRE document review output"
    );
    assert!(!out
        .data
        .get("negotiationLevers")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
}
