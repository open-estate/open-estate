// Integration tests for `src/modules/buyer_diligence.rs` and `GET /api/buyer-diligence/:slug`
// using procedural synthetic reports (`common::synth_report_*`, `common::synth_holdout_*`)
// and synthetic jurisdiction parcels with zero mock HTTP servers.

mod common;

use open_estate::db::DbStore;
use open_estate::guardrails::{HttpDefenseShield, SECURITY_RESPONSE_HEADERS};
use open_estate::modules::buyer_diligence::{
    build_buyer_diligence_and_hoa_deterministic, run_buyer_diligence_and_hoa,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

fn ptr<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path)
        .unwrap_or_else(|| panic!("missing JSON pointer {path} in {v}"))
}

fn load_synth_holdout_home_report(holdout_id: &str) -> Value {
    let home = common::synth_holdout_by_id(holdout_id);
    let price = home["targetSalePrice"].as_i64().unwrap_or(950_000);
    let mut rep = common::synth_report_primary();
    rep["meta"]["displayAddress"] = home["address"].clone();
    rep["geo"] = home["raw"]["geo"].clone();
    rep["facts"] = home["raw"]["facts"].clone();
    rep["modules"]["valuation"]["data"]["estimate"]["value"] = json!(price);
    rep["modules"]["valuation"]["data"]["ownership"]["price"] = json!(price);
    rep
}

/// Adapts a procedural synthetic report to a US jurisdiction (state/county/city) so statutory
/// transfer-tax, homestead, and point-of-sale ordinance schedules can be tested across states.
#[allow(clippy::too_many_arguments)]
fn synth_jurisdiction_report(
    display_address: &str,
    slug: &str,
    state: &str,
    county: &str,
    county_fips: &str,
    city: &str,
    zip: &str,
    lat: f64,
    lon: f64,
    price: i64,
) -> Value {
    let mut rep = common::synth_report_secondary();
    rep["meta"]["slug"] = json!(slug);
    rep["meta"]["displayAddress"] = json!(display_address);
    rep["geo"] = json!({
        "matched": true,
        "displayAddress": display_address,
        "state": state,
        "county": county,
        "countyFips": county_fips,
        "city": city,
        "place": city,
        "zip": zip,
        "lat": lat,
        "lon": lon,
        "msa": { "cbsa": "00000", "name": city }
    });
    rep["facts"]["address"] = json!(display_address);
    rep["facts"]["hoaMonthly"] = json!(0);
    rep["modules"]["valuation"]["data"]["estimate"]["value"] = json!(price);
    rep["modules"]["valuation"]["data"]["ownership"] = json!({
        "price": price,
        "rate30": 6.65,
        "rentEstimate": (price as f64 * 0.0045).round() as i64,
        "monthly": {
            "principalInterest": ((price as f64 * 0.80) * 0.00642).round() as i64,
            "propertyTax": ((price as f64 * 0.012) / 12.0).round() as i64,
            "insurance": ((price as f64 * 0.0036) / 12.0).round() as i64,
            "maintenance": 350,
            "hoa": 0
        }
    });
    rep["modules"]["market"]["data"]["zhvi"]["latest"] = json!(price);
    rep
}

#[test]
fn test_1_hoa_profile_super_priority_noda_liens_reserves_and_resale_cert() {
    // 1A: Synthetic Coral Gables FL holdout property configured as condo HOA scenario
    let mut fl_rep = load_synth_holdout_home_report("synth-fl-33134-remod-sfr");
    fl_rep["facts"]["propertyType"] = json!("Condo");
    fl_rep["facts"]["hoaMonthly"] = json!(720);
    fl_rep["facts"]["features_text"] = json!(
        "20% rental cap with waitlist; no short-term rentals 30-day minimum lease; max 2 pets 25 lb weight limit; board approval and $250 move-in fee required; active $12,000 roof special assessment."
    );
    fl_rep["facts"]["titleAndLiens"] = json!({
        "involuntaryLiens": [
            { "type": "HOA Assessment Lien", "amount": 4850, "note": "Recorded condominium association assessment lien for unpaid dues" }
        ]
    });
    fl_rep["facts"]["hoa"] = json!({
        "specialAssessmentUsd": 12000,
        "specialAssessmentStatus": "Approved / Active"
    });
    fl_rep["hoaReview"] = json!({
        "financialHealth": {
            "monthlyDuesUsd": 740.0,
            "percentFunded": 24.5,
            "reserveAllocationPctOfBudget": 7.5,
            "delinquencyRatePct": 18.2,
            "specialAssessments": [
                { "amountUsd": 12000, "purpose": "Milestone Concrete & Roof", "boardVoteStatus": "Approved / Active" }
            ]
        },
        "governanceAndLifestyleRules": {
            "rentalCap": { "hasRentalCap": true, "capPercent": 20.0, "hasWaitlist": true, "waitlistCount": 4, "summary": "20% cap with 4-unit waitlist" },
            "shortTermRental": { "strProhibited": true, "minimumLeaseDays": 180, "summary": "180-day minimum lease" },
            "petRestrictions": { "restricted": true, "petsAllowed": true, "maxCount": 2, "maxWeightLbs": 25 },
            "masterInsurance": { "coverageType": "Bare Walls", "deductiblePercent": 5.0, "deductibleShockWarning": true }
        },
        "warrantability": { "status": "Non-Warrantable", "isWarrantable": false }
    });

    let fl_out = build_buyer_diligence_and_hoa_deterministic(&fl_rep);
    let hp = &fl_out.hoa_profile;

    assert_eq!(hp["hasHoa"], json!(true));
    assert_eq!(hp["hoaMonthly"], json!(740.0));
    assert_eq!(hp["hoaAnnual"], json!(8880));
    assert_eq!(hp["consumedHoaReview"], json!(true));
    assert_eq!(
        ptr(
            hp,
            "/debtAndLienStatus/superPriorityLienState/isSuperPriorityState"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(
            hp,
            "/debtAndLienStatus/superPriorityLienState/monthsPriority"
        ),
        &json!(12)
    );
    assert_eq!(ptr(hp, "/debtAndLienStatus/hasActiveHoaLien"), &json!(true));
    assert_eq!(
        ptr(hp, "/debtAndLienStatus/hasSpecialAssessmentSignal"),
        &json!(true)
    );
    assert_eq!(ptr(hp, "/debtAndLienStatus/percentFunded"), &json!(24.5));
    assert!(ptr(hp, "/coverageBreakdown/reservesContributionRisk/level")
        .as_str()
        .unwrap()
        .contains("Critical"));
    assert_eq!(
        ptr(hp, "/requirements/rentalCapOrWaitlist/restricted"),
        &json!(true)
    );
    assert_eq!(
        ptr(hp, "/requirements/shortTermRentalRestriction/prohibited"),
        &json!(true)
    );
    assert_eq!(
        ptr(
            hp,
            "/requirements/boardApprovalOrMoveInFee/requiresBoardApproval"
        ),
        &json!(true)
    );
    assert!(ptr(hp, "/requirements/fhaVaCondoApprovalAlert/riskLevel")
        .as_str()
        .unwrap()
        .contains("High"));

    let cert_req = ptr(hp, "/debtAndLienStatus/resaleCertificateRequirement")
        .as_str()
        .unwrap();
    assert!(
        cert_req.contains("Seller-Paid HOA Resale Certificate / Demand Statement")
            && cert_req.contains("$0.00")
    );
    assert!(hp["disclaimer"].as_str().unwrap().contains("HOA"));

    // 1B: Nevada 9-month super-priority state + age-restricted HOA detection
    let mut nv_rep = synth_jurisdiction_report(
        "100 Synthdesert01 Blvd, Las Vegas, NV 89101",
        "100-synthdesert01-blvd-las-vegas-nv-89101",
        "NV",
        "Clark County",
        "32003",
        "Las Vegas",
        "89101",
        36.1672,
        -115.1485,
        420_000,
    );
    nv_rep["facts"] = json!({
        "propertyType": "Townhouse",
        "sqft": 1500,
        "hoaMonthly": 210,
        "features_text": "55+ active adult community; gated HOA includes water and trash."
    });
    let nv_out = build_buyer_diligence_and_hoa_deterministic(&nv_rep);
    let nv_hp = &nv_out.hoa_profile;
    assert_eq!(nv_hp["consumedHoaReview"], json!(false));
    assert_eq!(nv_hp["hoaMonthly"], json!(210.0));
    assert_eq!(
        ptr(
            nv_hp,
            "/debtAndLienStatus/superPriorityLienState/isSuperPriorityState"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(
            nv_hp,
            "/debtAndLienStatus/superPriorityLienState/monthsPriority"
        ),
        &json!(9)
    );
    assert_eq!(
        ptr(
            nv_hp,
            "/requirements/occupancyOrAgeRestriction/isAgeRestricted"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(nv_hp, "/requirements/occupancyOrAgeRestriction/minAge"),
        &json!(55)
    );

    // 1C: California NODA detection on synthetic San Jose report
    let mut ca_rep = common::synth_report_primary();
    ca_rep["facts"]["propertyType"] = json!("Condo");
    ca_rep["facts"]["hoaMonthly"] = json!(480);
    ca_rep["facts"]["titleAndLiens"] = json!({
        "involuntaryLiens": [
            { "type": "Notice of Delinquent Assessment (NODA)", "amount": 3950, "note": "Recorded NODA under CA Civ. Code 5675" }
        ]
    });
    let ca_out = build_buyer_diligence_and_hoa_deterministic(&ca_rep);
    let ca_hp = &ca_out.hoa_profile;
    assert_eq!(
        ptr(
            ca_hp,
            "/debtAndLienStatus/superPriorityLienState/isSuperPriorityState"
        ),
        &json!(false)
    );
    assert_eq!(ptr(ca_hp, "/debtAndLienStatus/nodaDetected"), &json!(true));
    assert!(ptr(ca_hp, "/debtAndLienStatus/overallRisk")
        .as_str()
        .unwrap()
        .contains("Critical"));
}

#[test]
fn test_2_cash_to_close_high_transfer_tax_and_2_1_buydown_math_on_synth_locations() {
    let price = 1_500_000;
    let sf_rep = synth_jurisdiction_report(
        "100 Synthcivic02 Pl, San Francisco, CA 94102",
        "100-synthcivic02-pl-san-francisco-ca-94102",
        "CA",
        "San Francisco County",
        "06075",
        "San Francisco",
        "94102",
        37.7793,
        -122.4193,
        price,
    );
    let mut sea_rep = load_synth_holdout_home_report("synth-wa-98103-adu-sfr");
    sea_rep["modules"]["valuation"]["data"]["estimate"]["value"] = json!(price);
    sea_rep["modules"]["valuation"]["data"]["ownership"]["price"] = json!(price);

    let nyc_rep = synth_jurisdiction_report(
        "100 Synthmidtown03 Ave, New York, NY 10118",
        "100-synthmidtown03-ave-new-york-ny-10118",
        "NY",
        "New York County",
        "36061",
        "New York",
        "10118",
        40.7484,
        -73.9857,
        price,
    );
    let tx_rep = synth_jurisdiction_report(
        "100 Synthcapitol04 St, Austin, TX 78701",
        "100-synthcapitol04-st-austin-tx-78701",
        "TX",
        "Travis County",
        "48453",
        "Austin",
        "78701",
        30.2650,
        -97.7466,
        price,
    );

    let sf_bd = build_buyer_diligence_and_hoa_deterministic(&sf_rep).buyer_diligence;
    let sea_bd = build_buyer_diligence_and_hoa_deterministic(&sea_rep).buyer_diligence;
    let nyc_bd = build_buyer_diligence_and_hoa_deterministic(&nyc_rep).buyer_diligence;
    let tx_bd = build_buyer_diligence_and_hoa_deterministic(&tx_rep).buyer_diligence;

    let sf_tt = sf_bd
        .pointer("/cashToClose/transferTax/totalTransferTaxUsd")
        .and_then(Value::as_i64)
        .unwrap();
    let sea_tt = sea_bd
        .pointer("/cashToClose/transferTax/totalTransferTaxUsd")
        .and_then(Value::as_i64)
        .unwrap();
    let nyc_tt = nyc_bd
        .pointer("/cashToClose/transferTax/totalTransferTaxUsd")
        .and_then(Value::as_i64)
        .unwrap();
    let tx_tt = tx_bd
        .pointer("/cashToClose/transferTax/totalTransferTaxUsd")
        .and_then(Value::as_i64)
        .unwrap();

    assert_eq!(sf_tt, 11_250); // $1.5M * 0.75% SF tier
    assert!(sea_tt > 22_000); // WA graduated REET + 0.50% Seattle local REET
    assert!(nyc_tt > 35_000); // NYS transfer + NYC RPTT + NYS Mansion Tax
    assert_eq!(tx_tt, 0); // TX $0 state transfer tax baseline

    assert_eq!(
        ptr(
            &sf_bd,
            "/cashToClose/transferTax/isHighTransferTaxJurisdiction"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(
            &sea_bd,
            "/cashToClose/transferTax/isHighTransferTaxJurisdiction"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(
            &nyc_bd,
            "/cashToClose/transferTax/isHighTransferTaxJurisdiction"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(
            &tx_bd,
            "/cashToClose/transferTax/isHighTransferTaxJurisdiction"
        ),
        &json!(false)
    );

    let nyc_c2c_20 = nyc_bd
        .pointer("/cashToClose/scenarios/0/totalCashToCloseUsd")
        .and_then(Value::as_i64)
        .unwrap();
    let tx_c2c_20 = tx_bd
        .pointer("/cashToClose/scenarios/0/totalCashToCloseUsd")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(nyc_c2c_20 > tx_c2c_20 + 25_000);

    // Check San Jose (Measure E >$2M), Philadelphia PA, Coral Gables FL, Cambridge MA
    let mut sj_primary = common::synth_report_primary();
    sj_primary["modules"]["valuation"]["data"]["estimate"]["value"] = json!(2_150_000);
    sj_primary["modules"]["valuation"]["data"]["ownership"]["price"] = json!(2_150_000);
    let phila_rep = load_synth_holdout_home_report("synth-pa-19147-rowhouse");
    let miami_rep = load_synth_holdout_home_report("synth-fl-33134-remod-sfr");
    let ma_rep = load_synth_holdout_home_report("synth-ma-02139-condo");

    for (label, rep) in [
        ("San Jose Measure E", sj_primary),
        ("Philadelphia PA", phila_rep),
        ("Miami-Dade FL", miami_rep),
        ("Cambridge MA", ma_rep),
    ] {
        let bd = build_buyer_diligence_and_hoa_deterministic(&rep).buyer_diligence;
        assert_eq!(
            ptr(
                &bd,
                "/cashToClose/transferTax/isHighTransferTaxJurisdiction"
            ),
            &json!(true),
            "expected high transfer tax flag for {label}"
        );
        assert!(
            ptr(&bd, "/cashToClose/transferTax/totalTransferTaxUsd")
                .as_i64()
                .unwrap()
                > 2_500,
            "expected substantial statutory transfer tax for {label}"
        );
    }

    // 2-1 Temporary Rate Buydown vs Direct Price Reduction
    let bd21 = &sf_bd["cashToClose"]["buydownVsPriceCut"];
    assert_eq!(bd21["noteRatePct"], json!(6.65));
    assert_eq!(bd21["year1RatePct"], json!(4.65));
    assert_eq!(bd21["year2RatePct"], json!(5.65));
    assert!(bd21["buydownEscrowCostUsd"].as_i64().unwrap() > 20_000);
    assert!(
        bd21["year1MonthlySavingsUsd"].as_i64().unwrap()
            > bd21["year2MonthlySavingsUsd"].as_i64().unwrap()
    );
    assert!(bd21["breakEvenMonths"].as_i64().unwrap() >= 24);
}

#[test]
fn test_3_special_tax_districts_recorded_levies_and_zero_invention_when_unrecorded() {
    // 3A: CA Mello-Roos CFD with recorded parcel `melloRoosAnnual`
    let mut ca_recorded = common::synth_report_primary();
    ca_recorded["facts"]["melloRoosAnnual"] = json!(2_880);
    ca_recorded["facts"]["features_text"] =
        json!("Includes swimming pool & spa; subject to Mello-Roos CFD special tax.");
    let ca_bd = build_buyer_diligence_and_hoa_deterministic(&ca_recorded).buyer_diligence;
    assert_eq!(
        ptr(
            &ca_bd,
            "/ongoingCosts/specialTaxDistricts/melloRoosCfd/flagged"
        ),
        &json!(true)
    );
    assert_eq!(
        ca_bd
            .pointer("/ongoingCosts/specialTaxDistricts/melloRoosCfd/estimatedAnnualUsd")
            .and_then(Value::as_i64),
        Some(2_880)
    );
    let ca_burn = ca_bd
        .pointer("/ongoingCosts/allInMonthlyBurn/allInMonthlyBurnUsd")
        .and_then(Value::as_i64)
        .unwrap();
    let ca_piti = ca_bd
        .pointer("/ongoingCosts/allInMonthlyBurn/pitiUsd")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(ca_burn > ca_piti);

    // 3B: TX MUD with recorded parcel `mudTaxAnnual`
    let mut tx_rep = synth_jurisdiction_report(
        "100 Synthbayou05 Rd, Pasadena, TX 77504",
        "100-synthbayou05-rd-pasadena-tx-77504",
        "TX",
        "Harris County",
        "48201",
        "Pasadena",
        "77504",
        29.6611,
        -95.1769,
        550_000,
    );
    tx_rep["facts"]["yearBuilt"] = json!(2021);
    tx_rep["facts"]["mudTaxAnnual"] = json!(3_300);
    tx_rep["facts"]["features_text"] =
        json!("Located in Harris County Municipal Utility District (MUD).");
    let tx_bd = build_buyer_diligence_and_hoa_deterministic(&tx_rep).buyer_diligence;
    assert_eq!(
        ptr(
            &tx_bd,
            "/ongoingCosts/specialTaxDistricts/mudDistrictTx/flagged"
        ),
        &json!(true)
    );
    assert_eq!(
        tx_bd
            .pointer("/ongoingCosts/specialTaxDistricts/mudDistrictTx/estimatedAnnualUsd")
            .and_then(Value::as_i64),
        Some(3_300)
    );

    // 3C: FL CDD on synthetic Coral Gables holdout home with recorded `cddAnnual`
    let mut fl_rep = load_synth_holdout_home_report("synth-fl-33134-remod-sfr");
    fl_rep["facts"]["cddAnnual"] = json!(2_400);
    fl_rep["facts"]["features_text"] = json!("Master-planned CDD community with clubhouse.");
    let fl_bd = build_buyer_diligence_and_hoa_deterministic(&fl_rep).buyer_diligence;
    assert_eq!(
        ptr(
            &fl_bd,
            "/ongoingCosts/specialTaxDistricts/cddDistrictFl/flagged"
        ),
        &json!(true)
    );
    assert_eq!(
        fl_bd
            .pointer("/ongoingCosts/specialTaxDistricts/cddDistrictFl/estimatedAnnualUsd")
            .and_then(Value::as_i64),
        Some(2_400)
    );
}

#[tokio::test]
async fn test_4_pos_mandates_nj_oil_tank_east_bay_sewer_rural_septic_well_and_fcc_resilience() {
    // 4A: Pre-1985 NJ parcel -> buried oil tank sweep + federal lead paint
    let mut nj_rep = synth_jurisdiction_report(
        "100 Synthbergen06 Sq, Paramus, NJ 07652",
        "100-synthbergen06-sq-paramus-nj-07652",
        "NJ",
        "Bergen County",
        "34003",
        "Paramus",
        "07652",
        40.9445,
        -74.0754,
        780_000,
    );
    nj_rep["facts"]["yearBuilt"] = json!(1964);
    let nj_bd = build_buyer_diligence_and_hoa_deterministic(&nj_rep).buyer_diligence;
    let nj_flags: Vec<String> = nj_bd
        .pointer("/utilitiesAndMandates/pointOfSaleMandates/jurisdictionFlags")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    assert!(nj_flags.contains(&"PRE_1985_OIL_TANK_SWEEP".to_string()));
    assert!(nj_flags.contains(&"FEDERAL_PRE_1978_LEAD_PAINT".to_string()));

    // 4B: East Bay (Oakland, CA) -> EBMUD sewer lateral + CA seismic water-heater strap + historic zoning flag
    let mut oak_rep = synth_jurisdiction_report(
        "100 Syntheastbay07 Plz, Oakland, CA 94612",
        "100-syntheastbay07-plz-oakland-ca-94612",
        "CA",
        "Alameda County",
        "06001",
        "Oakland",
        "94612",
        37.8053,
        -122.2725,
        1_050_000,
    );
    oak_rep["facts"]["yearBuilt"] = json!(1928);
    oak_rep["facts"]["zoning"] = json!("RM-2 (HPOZ Historic Sensitivity)");
    let oak_bd = build_buyer_diligence_and_hoa_deterministic(&oak_rep).buyer_diligence;
    let oak_flags: Vec<String> = oak_bd
        .pointer("/utilitiesAndMandates/pointOfSaleMandates/jurisdictionFlags")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    assert!(oak_flags.contains(&"EAST_BAY_EBMUD_PSL".to_string()));
    assert!(oak_flags.contains(&"CA_SEISMIC_STRAP_AND_LOW_FLOW".to_string()));
    assert_eq!(
        ptr(
            &oak_bd,
            "/neighborhoodPipeline/historicRestrictions/isHistoricOrSensitive"
        ),
        &json!(true)
    );

    // 4C: Rural large-lot septic & private well + low broadband adoption
    let mut rural_rep = synth_jurisdiction_report(
        "100 Synthrural08 Rd, Pittsboro, NC 27312",
        "100-synthrural08-rd-pittsboro-nc-27312",
        "NC",
        "Chatham County",
        "37037",
        "Pittsboro",
        "27312",
        35.7201,
        -79.1772,
        620_000,
    );
    rural_rep["geo"]["msa"] = Value::Null;
    rural_rep["facts"]["lotSqft"] = json!(95_000);
    rural_rep["modules"]["environment"]["data"]["water"] = json!({ "primary": Value::Null });
    rural_rep["modules"]["market"]["data"]["acs"]["tract"] = json!({ "broadbandPct": 71.0 });
    rural_rep["modules"]["civic"]["data"]["walkability"] = json!({ "index": 3.0 });
    rural_rep["modules"]["amenities"]["data"]["counts"] =
        json!({ "restaurant": { "oneMile": 0 } });

    let rural_bd = build_buyer_diligence_and_hoa_deterministic(&rural_rep).buyer_diligence;
    assert_eq!(
        ptr(
            &rural_bd,
            "/utilitiesAndMandates/sewerAndSeptic/isLikelyPrivateSeptic"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(
            &rural_bd,
            "/utilitiesAndMandates/waterSource/isLikelyPrivateWell"
        ),
        &json!(true)
    );
    assert_eq!(
        ptr(&rural_bd, "/connectivity/ruralSatelliteDslRiskWarning"),
        &json!(true)
    );
    assert!(rural_bd
        .pointer("/connectivity/fccLookupUrl")
        .and_then(Value::as_str)
        .unwrap()
        .starts_with("https://broadbandmap.fcc.gov/"));

    // 4D: Zero-mock transport resilience (closed local TCP port -> graceful non-panicking fallback)
    let closed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);

    let env_dead = EnvConfig::from_map(std::collections::HashMap::from([(
        "FCC_BDC_API_URL".to_string(),
        format!("http://127.0.0.1:{closed_port}/bdc"),
    )]));
    let http_dead = HttpClient::new(None);
    let fb_out = run_buyer_diligence_and_hoa(&http_dead, &env_dead, &oak_rep).await;
    let fcc_status = fb_out
        .buyer_diligence
        .pointer("/connectivity/fcc/status")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !fcc_status.is_empty() && fcc_status != "live_fcc",
        "unreachable FCC endpoint must not claim live_fcc: {fcc_status}"
    );

    let proc_env = EnvConfig::from_process_and_dotenv(Some(Path::new(env!("CARGO_MANIFEST_DIR"))));
    if proc_env.get("FCC_BDC_API_TOKEN").is_some() || proc_env.get("FCC_BDC_USERNAME").is_some() {
        let http_live = HttpClient::from_env(&proc_env, None);
        let primary = common::synth_report_primary();
        let live_out = run_buyer_diligence_and_hoa(&http_live, &proc_env, &primary).await;
        assert!(live_out.buyer_diligence.get("connectivity").is_some());
    } else {
        eprintln!(
            "Skipping live keyed FCC BDC query: FCC_BDC_API_TOKEN / FCC_BDC_USERNAME not set"
        );
    }
}

#[test]
fn test_5_rent_vs_buy_1bath_obsolescence_and_homestead_calendar_on_synth_fixtures() {
    let mut weak_rep = synth_jurisdiction_report(
        "100 Synthmetro09 St, Dallas, TX 75201",
        "100-synthmetro09-st-dallas-tx-75201",
        "TX",
        "Dallas County",
        "48113",
        "Dallas",
        "75201",
        32.7763,
        -96.7969,
        1_200_000,
    );
    weak_rep["facts"]["baths"] = json!(1.0);
    weak_rep["facts"]["beds"] = json!(2);
    weak_rep["modules"]["market"]["data"]["zhvi"]["latest"] = json!(450_000);
    weak_rep["modules"]["amenities"]["data"]["proximity"] =
        json!({ "freeway": { "distM": 150 } });
    weak_rep["modules"]["valuation"]["data"]["ownership"]["rentEstimate"] = json!(3200);

    let strong_rep = synth_jurisdiction_report(
        "100 Synthmetro09 St, Dallas, TX 75201",
        "100-synthmetro09-st-dallas-tx-75201",
        "TX",
        "Dallas County",
        "48113",
        "Dallas",
        "75201",
        32.7763,
        -96.7969,
        450_000,
    );

    let weak_bd = build_buyer_diligence_and_hoa_deterministic(&weak_rep).buyer_diligence;
    let strong_bd = build_buyer_diligence_and_hoa_deterministic(&strong_rep).buyer_diligence;

    let weak_score = weak_bd
        .pointer("/exitAndBreakEven/exitLiquidity/score")
        .and_then(Value::as_i64)
        .unwrap();
    let strong_score = strong_bd
        .pointer("/exitAndBreakEven/exitLiquidity/score")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(weak_score < strong_score);
    assert!((0..=100).contains(&weak_score));

    let obs_keys: Vec<String> = weak_bd
        .pointer("/exitAndBreakEven/exitLiquidity/obsolescenceFlags")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|x| x.get("key").and_then(Value::as_str).map(String::from))
        .collect();
    assert!(obs_keys.contains(&"single_bath_obsolescence".to_string()));
    assert!(obs_keys.contains(&"over_improved_price_ceiling".to_string()));
    assert!(obs_keys.contains(&"freeway_backing".to_string()));

    // Statutory homestead exemption checks on TX, FL, and CA
    assert!(strong_bd
        .pointer("/postClosingAndClimate/homesteadExemption/programName")
        .and_then(Value::as_str)
        .unwrap()
        .contains("$100,000"));
    assert!(strong_bd
        .pointer("/postClosingAndClimate/homesteadExemption/filingDeadline")
        .and_then(Value::as_str)
        .unwrap()
        .contains("Apr 30"));

    let fl_bd = build_buyer_diligence_and_hoa_deterministic(&load_synth_holdout_home_report(
        "synth-fl-33134-remod-sfr",
    ))
    .buyer_diligence;
    assert!(fl_bd
        .pointer("/postClosingAndClimate/homesteadExemption/programName")
        .and_then(Value::as_str)
        .unwrap()
        .contains("$50,000"));
    assert!(fl_bd
        .pointer("/postClosingAndClimate/homesteadExemption/filingDeadline")
        .and_then(Value::as_str)
        .unwrap()
        .contains("March 1"));

    let ca_bd =
        build_buyer_diligence_and_hoa_deterministic(&common::synth_report_primary()).buyer_diligence;
    assert!(ca_bd
        .pointer("/postClosingAndClimate/homesteadExemption/programName")
        .and_then(Value::as_str)
        .unwrap()
        .contains("Prop 19"));
    assert!(ca_bd
        .pointer("/postClosingAndClimate/homesteadExemption/filingDeadline")
        .and_then(Value::as_str)
        .unwrap()
        .contains("February 15"));
}

#[tokio::test]
async fn test_6_http_api_buyer_diligence_route_and_client_hoa_review_merge_on_synth_fixture() {
    let temp_dir = std::env::temp_dir().join(format!("openestate-bd-test-{}", Uuid::new_v4()));
    let reports_dir = temp_dir.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("create reports dir");
    let db_path = temp_dir.join("bd.sqlite");

    let env = EnvConfig::from_map(std::collections::HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            reports_dir.to_string_lossy().into_owned(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
    ]));
    let http = HttpClient::new(None);
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");

    // Seed synthetic secondary report configured with HOA dues to verify per-client review merge
    let slug = common::SYNTH_SECONDARY_SLUG;
    let mut rep = common::synth_report_secondary();
    if let Some(obj) = rep.as_object_mut() {
        obj.remove("hoaProfile");
        obj.remove("buyerDiligence");
    }
    rep["facts"]["hoaMonthly"] = json!(650);
    db.put_cached_report(slug, &rep, None)
        .await
        .expect("seed cached report");

    let client_id = "browser-client-12345";
    db.put_client_doc_review(
        "hoa",
        slug,
        client_id,
        &json!({
            "financialHealth": {
                "monthlyDuesUsd": 715.0,
                "percentFunded": 28.0,
                "specialAssessments": [{ "amountUsd": 8500, "purpose": "Roof & Exterior Reserve", "boardVoteStatus": "Approved / Active" }]
            }
        }),
        3600,
    )
    .await
    .expect("seed client hoa review");

    let state = AppState::with_components(
        env,
        http,
        db,
        HttpDefenseShield::default(),
        reports_dir,
        None::<PathBuf>,
    );
    let router = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await;
    });

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .unwrap();

    // 1. Unknown slug -> 404
    let r404 = client
        .get(format!(
            "http://{addr}/api/buyer-diligence/nonexistent-slug-999"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r404.status(), reqwest::StatusCode::NOT_FOUND);

    // 2. Invalid slug -> 400
    let r400 = client
        .get(format!("http://{addr}/api/buyer-diligence/bad_slug!!"))
        .send()
        .await
        .unwrap();
    assert_eq!(r400.status(), reqwest::StatusCode::BAD_REQUEST);

    // 3. Cached report without client header -> uses baseline report HOA ($650/mo)
    let r_base = client
        .get(format!("http://{addr}/api/buyer-diligence/{slug}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r_base.status(), reqwest::StatusCode::OK);
    for &(name, expected) in SECURITY_RESPONSE_HEADERS {
        assert_eq!(
            r_base.headers().get(name).and_then(|v| v.to_str().ok()),
            Some(expected)
        );
    }
    let j_base: Value = r_base.json().await.unwrap();
    assert_eq!(j_base["ok"], json!(true));
    assert_eq!(ptr(&j_base, "/hoaProfile/hoaMonthly"), &json!(650.0));
    assert!(j_base
        .pointer("/buyerDiligence/disclaimer")
        .and_then(Value::as_str)
        .unwrap()
        .contains("Estimates for planning only"));

    // 4. With X-OpenEstate-Client-Id -> consumes saved client hoaReview ($715/mo, 28% funded, $8.5k special assessment)
    let r_cli = client
        .get(format!("http://{addr}/api/buyer-diligence/{slug}"))
        .header("X-OpenEstate-Client-Id", client_id)
        .send()
        .await
        .unwrap();
    assert_eq!(r_cli.status(), reqwest::StatusCode::OK);
    let j_cli: Value = r_cli.json().await.unwrap();
    assert_eq!(ptr(&j_cli, "/hoaProfile/hoaMonthly"), &json!(715.0));
    assert_eq!(ptr(&j_cli, "/hoaProfile/consumedHoaReview"), &json!(true));
    assert_eq!(
        ptr(&j_cli, "/hoaProfile/debtAndLienStatus/percentFunded"),
        &json!(28.0)
    );
    assert_eq!(
        ptr(
            &j_cli,
            "/hoaProfile/debtAndLienStatus/hasSpecialAssessmentSignal"
        ),
        &json!(true)
    );

    let _ = shutdown_tx.send(());
    let _ = std::fs::remove_dir_all(&temp_dir);
}
