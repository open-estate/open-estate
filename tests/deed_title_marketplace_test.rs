// Integration and unit tests for Deed/Title/Title-Insurance analysis and the
// compliant 3-vertical Marketplace (`/api/marketplace/quotes`, `/api/marketplace/click`)
// using procedural synthetic property reports and multi-state synthetic cohort records.

mod common;

use axum::http::StatusCode;
use chrono::NaiveDate;
use open_estate::db::DbStore;
use open_estate::guardrails::{HttpDefenseShield, SECURITY_RESPONSE_HEADERS};
use open_estate::modules::deed_title_marketplace::{
    analyze_from_parts, build_deed_title_and_marketplace,
    build_deed_title_and_marketplace_with_options, lookup_who_pays, price_tier_for_price,
    MarketplaceClickStore, MarketplaceQueryOptions, COMPENSATION_MODEL, SPONSORED_DISCLOSURE_TEXT,
    WHO_PAYS_LABEL,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use uuid::Uuid;

fn report_from_holdout_home(home: &Value) -> Value {
    let raw = &home["raw"];
    let mut rep = json!({
        "meta": {
            "version": "0.3.0",
            "input": home["address"],
            "displayAddress": home["address"],
            "slug": home["id"],
            "aiPending": false
        },
        "geo": raw["geo"],
        "facts": raw["facts"],
        "modules": {
            "market": { "data": raw["market"], "warnings": [] },
            "valuation": {
                "data": {
                    "estimate": {
                        "value": home["targetSalePrice"]
                    }
                },
                "warnings": []
            },
            "hazards": { "data": {}, "warnings": [] }
        }
    });
    if raw.get("insurance").map(|v| v.is_object()).unwrap_or(false) {
        rep["insurance"] = raw["insurance"].clone();
    }
    rep
}

#[test]
fn test_1_fha_antiflipping_90d_and_91_180d_plus_fraud_guidance() {
    // Primary synthetic Santa Clara County report (lastSale: 2019-06-15 at $1,480,000)
    let primary = common::synth_report_primary();
    assert_eq!(primary["facts"]["lastSale"]["date"], json!("2019-06-15"));
    assert_eq!(primary["facts"]["lastSale"]["price"], json!(1_480_000));

    let env = EnvConfig::default();

    // A) Evaluate 41 days after recorded 2019-06-15 acquisition (<= 90 days) -> FHA ineligible
    let out_41d = build_deed_title_and_marketplace_with_options(
        &primary,
        &env,
        &MarketplaceQueryOptions {
            as_of_date: Some(NaiveDate::from_ymd_opt(2019, 7, 26).unwrap()),
            ..Default::default()
        },
    );
    let deed_41d = &out_41d.title_insurance_and_deed["deedAnalysis"];
    assert_eq!(deed_41d["fhaAntiFlipping"]["fhaEligible"], json!(false));
    let flags_41d = deed_41d["flags"].as_array().expect("flags array");
    assert!(
        flags_41d
            .iter()
            .any(|f| f["code"] == "FHA_FLIP_UNDER_90_DAYS"),
        "expected FHA_FLIP_UNDER_90_DAYS flag on 41-day hold, got {flags_41d:?}"
    );
    let exceptions = deed_41d["fhaAntiFlipping"]["exceptions"]
        .as_array()
        .expect("statutory exceptions list");
    let exc_blob = serde_json::to_string(exceptions).unwrap().to_lowercase();
    assert!(exc_blob.contains("inherited") && exc_blob.contains("hud reo"));

    let mort_benches_41d = out_41d.marketplace["verticals"]["mortgage"]["benchmarks"]
        .as_array()
        .expect("mortgage benchmarks");
    let fha_bench_41d = mort_benches_41d
        .iter()
        .find(|b| b["id"] == "benchmark-fha-35")
        .expect("FHA 3.5% benchmark row");
    assert_eq!(fha_bench_41d["eligible"], json!(false));

    // B) Evaluate 122 days after 2019-06-15 (91..=180 days) at $3,100,000 (>= 100% above $1,480,000)
    //    -> 24 CFR 203.37a(b)(3) second independent FHA appraisal required
    let out_122d = build_deed_title_and_marketplace_with_options(
        &primary,
        &env,
        &MarketplaceQueryOptions {
            as_of_date: Some(NaiveDate::from_ymd_opt(2019, 10, 15).unwrap()),
            price_override: Some(3_100_000.0),
            ..Default::default()
        },
    );
    let deed_122d = &out_122d.title_insurance_and_deed["deedAnalysis"];
    assert_eq!(deed_122d["fhaAntiFlipping"]["fhaEligible"], json!(true));
    assert_eq!(
        deed_122d["fhaAntiFlipping"]["secondAppraisalRequired"],
        json!(true)
    );
    let flags_122d = deed_122d["flags"].as_array().expect("flags array");
    assert!(
        flags_122d
            .iter()
            .any(|f| f["code"] == "FHA_FLIP_SECOND_APPRAISAL"),
        "expected FHA_FLIP_SECOND_APPRAISAL flag, got {flags_122d:?}"
    );

    // C) Evaluate at report generation window (2026-11-10, > 180 days) -> FHA anti-flip clear
    let out_seasoned = build_deed_title_and_marketplace_with_options(
        &primary,
        &env,
        &MarketplaceQueryOptions {
            as_of_date: Some(NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()),
            ..Default::default()
        },
    );
    let deed_seasoned = &out_seasoned.title_insurance_and_deed["deedAnalysis"];
    assert_eq!(deed_seasoned["fhaAntiFlipping"]["fhaEligible"], json!(true));
    assert_eq!(
        deed_seasoned["fhaAntiFlipping"]["secondAppraisalRequired"],
        json!(false)
    );

    // Wire-fraud & deed-theft guidance must always be populated with actionable steps
    let fraud_blob = serde_json::to_string(&out_seasoned.title_insurance_and_deed)
        .unwrap()
        .to_lowercase();
    assert!(
        fraud_blob.contains("wire") && fraud_blob.contains("escrow"),
        "expected closing wire-fraud verification guidance"
    );
    assert!(
        fraud_blob.contains("recorder") || fraud_blob.contains("property fraud"),
        "expected county recorder deed-alert guidance"
    );
}

#[test]
fn test_2_multi_region_title_insurance_who_pays_alta_and_reissue_discount() {
    let env = EnvConfig::default();

    // 1. Santa Clara County CA (06085): primary report (2019-06-15 sale -> 1.68 yrs at 2021-02-18)
    let primary = common::synth_report_primary();
    let out_primary = build_deed_title_and_marketplace_with_options(
        &primary,
        &env,
        &MarketplaceQueryOptions {
            as_of_date: Some(NaiveDate::from_ymd_opt(2021, 2, 18).unwrap()),
            ..Default::default()
        },
    );
    let ti_primary = &out_primary.title_insurance_and_deed["titleInsurance"];
    assert_eq!(ti_primary["customaryWhoPays"]["label"], json!(WHO_PAYS_LABEL));
    assert_eq!(
        ti_primary["customaryWhoPays"]["matchedRegion"]["ownersPolicyPayer"],
        json!("buyer"),
        "Northern CA (Santa Clara County 06085) custom assigns owner's policy to buyer"
    );
    assert_eq!(ti_primary["reissueDiscount"]["eligible"], json!(true));
    assert_eq!(
        ti_primary["reissueDiscount"]["estimatedDiscountPct"],
        json!(30.0)
    );

    // Policy comparison: Lender's vs Owner's Standard vs ALTA Homeowner's (10-15% markup)
    let pol = &ti_primary["policyComparison"];
    assert_eq!(
        pol["lendersPolicy"]["buyerEquityProtected"],
        json!(false),
        "Lender's policy protects lender only, not buyer equity"
    );
    assert_eq!(
        pol["ownersStandardPolicy"]["buyerEquityProtected"],
        json!(true)
    );
    assert_eq!(
        pol["altaHomeownersPolicy"]["buyerEquityProtected"],
        json!(true)
    );
    let std_prem = pol["ownersStandardPolicy"]["estimatedPremiumUsd"]
        .as_f64()
        .expect("standard owner premium");
    let enh_prem = pol["altaHomeownersPolicy"]["estimatedPremiumUsd"]
        .as_f64()
        .expect("enhanced owner premium");
    let ratio = enh_prem / std_prem;
    assert!(
        (1.10..=1.15).contains(&ratio),
        "ALTA Homeowner's policy premium ({enh_prem}) must be 10-15% above standard ({std_prem}), got ratio {ratio:.4}"
    );
    assert_eq!(
        ti_primary["respaSection9"]["sellerCannotRequireTitleInsurer"],
        json!(true)
    );
    assert!(
        ti_primary["sectionCShoppableServices"]["estimatedSavingsUsd"]
            .as_i64()
            .unwrap_or(0)
            >= 250
    );

    // 2. Santa Clara County CA (06085): secondary Campbell report (2021-05-20 sale)
    let campbell = common::synth_report_secondary();
    let out_campbell_4yr = build_deed_title_and_marketplace_with_options(
        &campbell,
        &env,
        &MarketplaceQueryOptions {
            // ~4.41 yrs since 2021-05-20 -> 3-5 yr 15% bracket
            as_of_date: Some(NaiveDate::from_ymd_opt(2025, 10, 18).unwrap()),
            ..Default::default()
        },
    );
    assert_eq!(
        out_campbell_4yr.title_insurance_and_deed["titleInsurance"]["reissueDiscount"]["eligible"],
        json!(true)
    );
    assert_eq!(
        out_campbell_4yr.title_insurance_and_deed["titleInsurance"]["reissueDiscount"]
            ["estimatedDiscountPct"],
        json!(15.0)
    );

    let out_campbell_5yr_plus = build_deed_title_and_marketplace_with_options(
        &campbell,
        &env,
        &MarketplaceQueryOptions {
            as_of_date: Some(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()), // > 5.1 yrs -> expired
            ..Default::default()
        },
    );
    assert_eq!(
        out_campbell_5yr_plus.title_insurance_and_deed["titleInsurance"]["reissueDiscount"]
            ["eligible"],
        json!(false)
    );

    // 3. Multi-state synthetic cohort records from common::synth_holdout_by_id
    let evanston = common::synth_holdout_by_id("synth-il-60201-vintage-sfr"); // Cook County IL 17031
    let philly = common::synth_holdout_by_id("synth-pa-19147-rowhouse"); // Philadelphia PA 42101
    let cambridge = common::synth_holdout_by_id("synth-ma-02139-condo"); // Middlesex MA 25017
    let coral_gables = common::synth_holdout_by_id("synth-fl-33134-remod-sfr"); // Miami-Dade FL 12086
    let seattle = common::synth_holdout_by_id("synth-wa-98103-adu-sfr"); // King County WA 53033

    let il_rule = lookup_who_pays(&evanston["raw"]["geo"]);
    assert_eq!(il_rule.state, "IL");
    assert_eq!(il_rule.owners_policy_payer, "seller");

    let pa_rule = lookup_who_pays(&philly["raw"]["geo"]);
    assert_eq!(pa_rule.state, "PA");
    assert_eq!(pa_rule.owners_policy_payer, "buyer");

    let ma_rule = lookup_who_pays(&cambridge["raw"]["geo"]);
    assert_eq!(ma_rule.state, "MA");
    assert_eq!(ma_rule.closing_model, "Attorney-closing state");

    let fl_rule = lookup_who_pays(&coral_gables["raw"]["geo"]);
    assert_eq!(fl_rule.state, "FL");
    assert_eq!(
        fl_rule.owners_policy_payer, "buyer",
        "Miami-Dade County FL (12086) custom assigns owner's policy to buyer"
    );

    let wa_rule = lookup_who_pays(&seattle["raw"]["geo"]);
    assert_eq!(wa_rule.state, "WA");
    assert_eq!(wa_rule.owners_policy_payer, "seller");
}

#[test]
fn test_3_3_vertical_benchmarks_tila_apr_respa_and_zero_partner_mode() {
    let env = EnvConfig::default(); // Zero MARKETPLACE_PARTNERS_JSON configured -> pure benchmark mode

    // Conforming Midwest SFR ($920,000 in Evanston, IL 60201)
    let evanston_rep =
        report_from_holdout_home(&common::synth_holdout_by_id("synth-il-60201-vintage-sfr"));
    let out_il = build_deed_title_and_marketplace(&evanston_rep, &env);
    assert_eq!(out_il.marketplace["propertyPrice"], json!(920_000));
    assert_eq!(out_il.marketplace["zip"], json!("60201"));
    assert_eq!(out_il.marketplace["partnersConfigured"], json!(false));
    assert_eq!(out_il.marketplace["hasSponsoredOffers"], json!(false));
    assert_eq!(
        out_il.marketplace["compensationModel"],
        json!(COMPENSATION_MODEL)
    );
    assert_eq!(
        out_il.marketplace["compliance"]["sponsoredDisclosureText"],
        json!(SPONSORED_DISCLOSURE_TEXT)
    );
    assert_eq!(
        out_il.marketplace["compliance"]["equalHousingOpportunity"],
        json!(true)
    );

    for vert in ["mortgage", "insurance", "title"] {
        let vobj = &out_il.marketplace["verticals"][vert];
        assert!(
            vobj.is_object(),
            "expected vertical `{vert}` object in marketplace"
        );
        let sponsored = vobj["sponsoredOffers"]
            .as_array()
            .expect("sponsoredOffers array");
        assert!(
            sponsored.is_empty(),
            "zero-partner benchmark mode must emit empty sponsoredOffers for `{vert}`"
        );
    }

    // TILA / Reg Z (12 CFR 1026.24): every mortgage benchmark displays APR >= note rate
    let mort_benches = out_il.marketplace["verticals"]["mortgage"]["benchmarks"]
        .as_array()
        .expect("mortgage benchmarks");
    assert!(!mort_benches.is_empty());
    for b in mort_benches {
        let rate = b["ratePct"].as_f64().expect("ratePct");
        let apr = b["aprPct"].as_f64().expect("aprPct");
        assert!(
            apr >= rate,
            "TILA Reg Z violation on {}: aprPct ({apr}) < ratePct ({rate})",
            b["id"]
        );
        assert!(
            b["fiveYearCostUsd"].as_f64().unwrap_or(0.0) > 0.0,
            "expected positive 5-year total cost on {}",
            b["id"]
        );
    }

    // Investment occupancy on synthetic Seattle ADU property ($1,460,000) includes DSCR investor benchmark
    let seattle_home = common::synth_holdout_by_id("synth-wa-98103-adu-sfr");
    let out_sea_inv = analyze_from_parts(
        &seattle_home["raw"]["facts"],
        &json!({ "estimate": { "value": seattle_home["targetSalePrice"] } }),
        &json!({}),
        &seattle_home["raw"]["market"],
        &seattle_home["raw"]["geo"],
        seattle_home["raw"].get("insurance"),
        &env,
        &MarketplaceQueryOptions {
            occupancy: Some("investment".to_string()),
            credit_score: Some(760),
            ..Default::default()
        },
    );
    let sea_benches = out_sea_inv.marketplace["verticals"]["mortgage"]["benchmarks"]
        .as_array()
        .expect("seattle mortgage benchmarks");
    assert!(
        sea_benches
            .iter()
            .any(|b| b["id"] == "benchmark-dscr-investor"),
        "investment occupancy should surface benchmark-dscr-investor"
    );
}

#[tokio::test]
async fn test_4_privacy_safe_click_store_capacity_and_http_marketplace_endpoints() {
    // 1. Bounded MarketplaceClickStore unit check: zero PII, strict key cap against cardinality exhaustion
    let bounded_store = MarketplaceClickStore::with_max_keys(3);
    let tier_2m = price_tier_for_price(2_320_000.0);
    assert!(bounded_store
        .try_record_event("mortgage", "benchmark-30yr-fixed", "95118", tier_2m, false)
        .is_some());
    assert!(bounded_store
        .try_record_event("mortgage", "benchmark-30yr-fixed", "95118", tier_2m, true)
        .is_some());
    assert!(bounded_store
        .try_record_event("title", "benchmark-title-enhanced", "95008", tier_2m, true)
        .is_some());
    assert!(bounded_store
        .try_record_event("insurance", "benchmark-ho3-admitted", "60201", "400k_800k", true)
        .is_some());
    assert_eq!(bounded_store.len(), 3);
    // 4th distinct key must be rejected once max_keys=3 is reached, while existing keys still increment
    assert!(bounded_store
        .try_record_event("mortgage", "benchmark-15yr-fixed", "98103", tier_2m, true)
        .is_none());
    assert!(bounded_store
        .try_record_event("mortgage", "benchmark-30yr-fixed", "95118", tier_2m, true)
        .is_some());
    assert_eq!(
        bounded_store.get_counts("mortgage", "benchmark-30yr-fixed", "95118", tier_2m),
        (1, 2)
    );

    // 2. Live Axum server seeded with procedural synthetic Santa Clara County reports
    let temp_dir = std::env::temp_dir().join(format!("openestate-mkt-test-{}", Uuid::new_v4()));
    let reports_dir = temp_dir.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
    let db_path = temp_dir.join("test.sqlite");
    let env = EnvConfig::from_process_and_dotenv(None).with_overrides([
        ("SQLITE_DB_PATH", db_path.to_string_lossy().into_owned()),
        ("SQLITE_PATH", db_path.to_string_lossy().into_owned()),
        ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
        ("SYNC_INTERVAL_MIN", "0".to_string()),
    ]);
    let http = HttpClient::from_env(&env, Some(temp_dir.join("http_cache")));
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");

    for (slug, mut rep) in [
        (common::SYNTH_PRIMARY_SLUG, common::synth_report_primary()),
        (common::SYNTH_SECONDARY_SLUG, common::synth_report_secondary()),
    ] {
        normalize_stored_report(&mut rep);
        let html = render_report_html(&rep);
        db.put_cached_report(slug, &rep, Some(&html))
            .await
            .expect("seed synthetic report");
    }

    let state = AppState::with_components(
        env,
        http,
        db,
        HttpDefenseShield::default(),
        reports_dir,
        Some(temp_dir.join("datasets")),
    );
    let router = build_router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");
    let host_hdr = addr.to_string();

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

    let client = Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client");

    // A) Marketplace brokerage routes are unmounted (404) in Personal Edition
    let q_res = client
        .get(format!(
            "{base_url}/api/marketplace/quotes?slug={}",
            common::SYNTH_PRIMARY_SLUG
        ))
        .send()
        .await
        .expect("GET /api/marketplace/quotes");
    assert_eq!(q_res.status(), StatusCode::NOT_FOUND);
    for &(hname, hexp) in SECURITY_RESPONSE_HEADERS {
        assert_eq!(
            q_res.headers().get(hname).and_then(|v| v.to_str().ok()),
            Some(hexp)
        );
    }

    let pq_res = client
        .post(format!("{base_url}/api/marketplace/quotes"))
        .header("Host", &host_hdr)
        .header("Origin", &base_url)
        .json(&json!({
            "slug": common::SYNTH_SECONDARY_SLUG,
            "vertical": "title"
        }))
        .send()
        .await
        .expect("POST /api/marketplace/quotes");
    assert_eq!(pq_res.status(), StatusCode::NOT_FOUND);

    let clk_res = client
        .post(format!("{base_url}/api/marketplace/click"))
        .header("Host", &host_hdr)
        .header("Origin", &base_url)
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "vertical": "mortgage",
            "partnerId": "benchmark-30yr-fixed",
            "event": "click"
        }))
        .send()
        .await
        .expect("POST /api/marketplace/click");
    assert_eq!(clk_res.status(), StatusCode::NOT_FOUND);

    let _ = shutdown_tx.send(());
    let _ = std::fs::remove_dir_all(&temp_dir);
}
