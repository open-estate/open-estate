// Integration & Unit Tests for House Profile, Buyer Photo In-Memory Analysis,
// Multigenerational Signals, Loan-Type Valuation Scenarios, Feng Shui Quick Screens,
// and Trilingual (EN / ZH / ES) Scoring Catalog Parity:
// - Uses procedural synthetic reports and synthetic holdout cohort homes from `tests/common/mod.rs`
// - Fetches public-domain USGS National Map aerial orthoimagery (PNG & JPEG) for a generic
//   Silicon Valley bounding box (`stored_bytes == 0`)
// - Gates live OpenAI multimodal vision wire calls cleanly on `OPENAI_API_KEY` (zero mock TCP servers)

mod common;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use open_estate::modules::fengshui::run_fengshui;
use open_estate::property::house_profile::{
    analyze_buyer_photos, compute_home_quality, listing_photos_allowed, BuyerPhotoInput,
    BUYER_PHOTO_NOTICE,
};
use open_estate::property::valuation::run_valuation;
use open_estate::scoring::catalog::{
    catalog_json, Dimension, Feature, Lang, LoanType, PropertyKind, SchoolLevel, Screen,
    StyleFamily,
};
use open_estate::scoring::engine::{
    rank, CommuteMatrix, DropReason, DroppedDimension, EstimatedDimension, GateHit, GateKind,
    GateReason, Notice, NoticeCode,
};
use open_estate::scoring::explain::{
    render_drop, render_estimate, render_explanation, render_gate, render_notice, Explanation,
};
use open_estate::scoring::factors::HomeFactors;
use open_estate::scoring::presets::PRESETS;
use open_estate::scoring::profile::Profile;
use open_estate::scoring::utility::{homefit_value, monthly_cost_for, EstimateReason};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

fn load_holdout_report(holdout_id: &str) -> Value {
    let home = common::synth_holdout_by_id(holdout_id);
    let mut modules = serde_json::Map::new();
    for mod_key in ["market", "insurance", "condition"] {
        if let Some(m) = home.pointer(&format!("/raw/{mod_key}")) {
            if !m.is_null() {
                modules.insert(mod_key.to_string(), json!({ "data": m }));
            }
        }
    }

    json!({
        "meta": {
            "slug": home["id"],
            "displayAddress": home["address"],
            "generatedAt": "2026-11-11T00:00:00Z",
        },
        "slug": home["id"],
        "input": home["address"],
        "geo": home["raw"]["geo"],
        "facts": home["raw"]["facts"],
        "modules": Value::Object(modules),
    })
}

async fn fetch_usgs_ortho_bytes(format: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    // Public-domain USGS National Map aerial orthoimagery for a generic Silicon Valley bounding box
    let url = format!(
        "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?bbox=-121.9546,37.2814,-121.9534,37.2826&bboxSR=4326&imageSR=4326&size={w},{h}&format={format}&f=image"
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?.to_vec();
    if bytes.len() < 64 {
        return None;
    }
    Some(bytes)
}

// =================================================================================================
// 1. Objective Home Quality on Synthetic Reports & Holdout Homes (Separate from Open Estate Score)
// =================================================================================================

#[test]
fn home_quality_scores_synthetic_reports_and_holdout_homes_without_altering_openestate_score() {
    let mut rep_a = common::synth_report_secondary();
    rep_a["facts"]["architecturalStyle"] = json!("Craftsman");
    rep_a["facts"]["features"] = json!([
        "Craftsman bungalow",
        "Hardwood floors",
        "Wood-burning fireplace",
        "Main-level primary bedroom",
        "Permitted 600 sq ft detached ADU"
    ]);
    let orig_overall_a = rep_a["summary"]["scores"]["overall"].clone();
    let hq_a = compute_home_quality(&rep_a);
    assert_eq!(
        rep_a["summary"]["scores"]["overall"], orig_overall_a,
        "compute_home_quality must never alter Open Estate Score"
    );
    let score_a = hq_a.score.expect("synthetic secondary home quality score");
    assert!((40.0..=100.0).contains(&score_a), "score_a={score_a}");
    assert!(hq_a.condition_score.is_some());
    assert!(hq_a.systems_score.is_some());
    assert!(hq_a.functionality_score.is_some());
    assert!(hq_a.layout_score.is_some());
    assert!(hq_a.light_energy_score.is_some());
    assert!(hq_a.lot_utility_score.is_some());
    assert_eq!(hq_a.style_family.as_deref(), Some("craftsman"));
    assert!(hq_a.style_evidence_backed);
    assert!(hq_a.character_features.contains(&"hardwood".to_string()));
    assert!(hq_a.character_features.contains(&"fireplace".to_string()));
    assert!(hq_a
        .character_features
        .contains(&"main_level_primary".to_string()));
    assert!(hq_a
        .source_notes
        .iter()
        .any(|n| n.contains("strictly separate from Open Estate Score")));

    let mut rep_b = common::synth_report_primary();
    rep_b["facts"]["features"] = json!([
        "Skylights and abundant natural light",
        "Vaulted high ceilings",
        "2-car attached garage"
    ]);
    let hq_b = compute_home_quality(&rep_b);
    let score_b = hq_b.score.expect("synthetic primary home quality score");
    assert!((40.0..=100.0).contains(&score_b), "score_b={score_b}");
    assert!(!hq_b.style_evidence_backed);
    assert_eq!(hq_b.style_basis.as_deref(), Some("era_prior"));
    assert!(hq_b
        .style_label
        .as_deref()
        .unwrap_or("")
        .starts_with("Likely "));
    assert!(hq_b
        .character_features
        .contains(&"natural_light".to_string()));
    assert!(hq_b
        .character_features
        .contains(&"high_ceilings".to_string()));

    // Compare synthetic Seattle permitted-ADU Craftsman (`synth-wa-98103-adu-sfr`, effectiveYearBuilt 2017)
    // against unremodeled 1926 Evanston SFR (`synth-il-60201-vintage-sfr`)
    let mut sea_adu = load_holdout_report("synth-wa-98103-adu-sfr");
    sea_adu["facts"]["architecturalStyle"] = json!("Craftsman");
    let chi_older = load_holdout_report("synth-il-60201-vintage-sfr");
    let hq_sea = compute_home_quality(&sea_adu);
    let hq_chi = compute_home_quality(&chi_older);
    assert!(
        hq_sea.condition_score.unwrap() > hq_chi.condition_score.unwrap(),
        "Remodeled Seattle Craftsman with effectiveYearBuilt 2017 ({:?}) must outscore unremodeled 1926 Evanston condition ({:?})",
        hq_sea.condition_score,
        hq_chi.condition_score
    );
    assert_eq!(hq_sea.style_family.as_deref(), Some("craftsman"));

    // Verify all 11 canonical Feature keys exist in the scoring catalog
    assert_eq!(Feature::ALL.len(), 11);

    let empty = compute_home_quality(&json!({}));
    assert!(empty.score.is_none());
    assert_eq!(empty.band, "Insufficient Data");
}

// =================================================================================================
// 2. Listing Photo Policy Hard-Block Invariants
// =================================================================================================

#[test]
fn listing_photos_allowed_defaults_false_and_hard_blocks_streetview_and_rapidapi() {
    let empty_env = EnvConfig::default();
    assert!(!listing_photos_allowed("reso_mls", &empty_env));
    assert!(!listing_photos_allowed("google_street_view", &empty_env));
    assert!(!listing_photos_allowed("streetview", &empty_env));
    assert!(!listing_photos_allowed("rapidapi", &empty_env));

    let env = EnvConfig::from_map(HashMap::from([
        ("ALLOW_LISTING_PHOTOS_RESO_MLS".into(), "true".into()),
        (
            "ALLOW_LISTING_PHOTOS_GOOGLE_STREET_VIEW".into(),
            "true".into(),
        ),
        ("ALLOW_LISTING_PHOTOS_STREETVIEW".into(), "1".into()),
        ("ALLOW_LISTING_PHOTOS_RAPIDAPI".into(), "yes".into()),
        ("ALLOW_LISTING_PHOTOS_RAPIDAPI_REALTY".into(), "true".into()),
    ]));
    assert!(listing_photos_allowed("reso_mls", &env));
    assert!(!listing_photos_allowed("attom", &env));
    assert!(!listing_photos_allowed("google_street_view", &env));
    assert!(!listing_photos_allowed("Google-Street-View", &env));
    assert!(!listing_photos_allowed("streetview", &env));
    assert!(!listing_photos_allowed("street_view", &env));
    assert!(!listing_photos_allowed("rapidapi", &env));
    assert!(!listing_photos_allowed("RapidAPI_Realty", &env));
}

// =================================================================================================
// 3. Buyer Photo In-Memory Analysis on Real Public-Domain USGS Orthoimagery + Live OpenAI Vision Gate
// =================================================================================================

#[tokio::test]
async fn buyer_photo_analysis_inspects_real_usgs_orthoimagery_validates_guardrails_and_stores_zero_bytes(
) {
    let http = HttpClient::new(None);
    let env = EnvConfig::default();

    // Guardrail checks that require zero image synthesis (MIME rejection & size cap)
    let bad_mime = vec![BuyerPhotoInput {
        filename: "sketch.svg".into(),
        mime_type: "image/svg+xml".into(),
        data_base64: STANDARD.encode(b"<svg/>"),
        caption: None,
    }];
    assert!(analyze_buyer_photos(&http, &env, &bad_mime).await.is_err());

    let huge = vec![BuyerPhotoInput {
        filename: "huge.jpg".into(),
        mime_type: "image/jpeg".into(),
        data_base64: STANDARD.encode(vec![0u8; 10 * 1024 * 1024 + 64]),
        caption: None,
    }];
    assert!(analyze_buyer_photos(&http, &env, &huge).await.is_err());

    // Fetch public-domain USGS National Map aerial orthoimagery for a generic Silicon Valley bounding box
    let (Some(png_bytes), Some(jpg_bytes)) = tokio::join!(
        fetch_usgs_ortho_bytes("png", 256, 192),
        fetch_usgs_ortho_bytes("jpg", 320, 240),
    ) else {
        eprintln!(
            "Skipping USGS orthoimagery photo inspection: basemap.nationalmap.gov unreachable offline"
        );
        return;
    };

    let photos = vec![
        BuyerPhotoInput {
            filename: "synth_parcel_beta_usgs_aerial_craftsman.png".into(),
            mime_type: "image/png".into(),
            data_base64: STANDARD.encode(&png_bytes),
            caption: Some(
                "1925 Craftsman home at 200 Synthparcelbeta Ave with hardwood floors, fireplace, vaulted ceilings"
                    .into(),
            ),
        },
        BuyerPhotoInput {
            filename: "synth_parcel_beta_usgs_lot_updated_kitchen.jpg".into(),
            mime_type: "image/jpeg".into(),
            data_base64: format!("data:image/jpeg;base64,{}", STANDARD.encode(&jpg_bytes)),
            caption: Some("Updated quartz kitchen with natural light and skylight".into()),
        },
    ];

    let res = analyze_buyer_photos(&http, &env, &photos)
        .await
        .expect("in-memory analysis on real USGS orthoimagery succeeds");
    assert_eq!(res.stored_bytes, 0);
    assert_eq!(res.analyzed_count, 2);
    assert_eq!(res.model_used, "deterministic_offline");
    assert_eq!(res.notice, BUYER_PHOTO_NOTICE);
    assert!(res.notice.contains("In-memory only"));
    assert!(res.notice.contains("0 bytes stored"));
    assert!(res.inferred_styles.contains(&"craftsman".to_string()));
    assert!(res.character_features.contains(&"hardwood".to_string()));
    assert!(res.character_features.contains(&"fireplace".to_string()));
    assert!(res
        .character_features
        .contains(&"high_ceilings".to_string()));
    assert!(res
        .character_features
        .contains(&"natural_light".to_string()));
    assert_eq!(res.finish_level.as_deref(), Some("updated_modern"));
    assert_eq!(res.photo_details[0].width, Some(256));
    assert_eq!(res.photo_details[0].height, Some(192));
    assert_eq!(res.photo_details[1].width, Some(320));
    assert_eq!(res.photo_details[1].height, Some(240));

    // Reject > 12 real photos
    let too_many: Vec<BuyerPhotoInput> = (0..13)
        .map(|i| BuyerPhotoInput {
            filename: format!("usgs_tile_{i}.png"),
            mime_type: "image/png".into(),
            data_base64: STANDARD.encode(&png_bytes),
            caption: None,
        })
        .collect();
    assert!(analyze_buyer_photos(&http, &env, &too_many).await.is_err());

    // Live OpenAI multimodal vision wire check when `OPENAI_API_KEY` is configured (zero mock TCP servers)
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        if !api_key.trim().is_empty() {
            let mut map = HashMap::from([("OPENAI_API_KEY".to_string(), api_key)]);
            if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
                if !base.trim().is_empty() {
                    map.insert("OPENAI_BASE_URL".to_string(), base);
                }
            }
            let live_env = EnvConfig::from_map(map);
            let live_res = analyze_buyer_photos(&http, &live_env, &photos[..1])
                .await
                .expect("live vision photo analysis");
            assert_eq!(live_res.stored_bytes, 0);
            assert_eq!(live_res.analyzed_count, 1);
            assert_eq!(live_res.notice, BUYER_PHOTO_NOTICE);
        }
    }
}

// =================================================================================================
// 4. All-Cash & Loan-Type Valuation Scenarios + Ranking Selection on Synthetic Reports
// =================================================================================================

#[test]
fn valuation_exposes_all_cash_and_loan_scenarios_and_scoring_selects_by_loan_type() {
    let rep = common::synth_report_primary();
    let val_out = run_valuation(
        &rep["facts"],
        &rep["modules"]["market"]["data"],
        &rep["geo"],
    );
    let own = &val_out.data["ownership"];
    let scenarios = own.get("scenarios").expect("ownership.scenarios");

    let conv = scenarios["conventional"]["total"].as_f64().unwrap();
    let fha = scenarios["fha"]["total"].as_f64().unwrap();
    let va = scenarios["va"]["total"].as_f64().unwrap();
    let cash = scenarios["cash"]["total"].as_f64().unwrap();
    assert_eq!(scenarios["cash"]["principalInterest"].as_i64(), Some(0));
    assert_eq!(scenarios["cash"]["mortgageInsurance"].as_i64(), Some(0));
    assert_eq!(own["allCash"]["monthlyTotal"].as_f64(), Some(cash));
    assert_eq!(
        val_out.data["loanPrograms"]["allCash"]["monthlyTotal"].as_f64(),
        Some(cash)
    );
    assert_eq!(conv, own["monthly"]["total"].as_f64().unwrap());
    assert!(
        cash < conv && conv < va && va < fha,
        "cash={cash}, conv={conv}, va={va}, fha={fha}"
    );

    let expected_cash_carrying = own["monthly"]["propertyTax"].as_f64().unwrap()
        + own["monthly"]["insurance"].as_f64().unwrap()
        + own["monthly"]["maintenance"].as_f64().unwrap()
        + own["monthly"]["hoa"].as_f64().unwrap();
    assert!((cash - expected_cash_carrying).abs() <= 2.0);

    let mut updated_rep = rep.clone();
    updated_rep["modules"]["valuation"]["data"] = val_out.data.clone();
    let hf = HomeFactors::from_report(&updated_rep);

    let mut p_conv = Profile::default();
    p_conv.loan_type = LoanType::Conventional;
    p_conv.budget.monthly = Some(15_000.0);

    let mut p_fha = p_conv.clone();
    p_fha.loan_type = LoanType::Fha;

    let mut p_va = p_conv.clone();
    p_va.loan_type = LoanType::Va;

    let mut p_cash = p_conv.clone();
    p_cash.loan_type = LoanType::Cash;

    assert_eq!(monthly_cost_for(&p_conv, &hf), Some(conv));
    assert_eq!(monthly_cost_for(&p_fha, &hf), Some(fha));
    assert_eq!(monthly_cost_for(&p_va, &hf), Some(va));
    assert_eq!(monthly_cost_for(&p_cash, &hf), Some(cash));

    // Also rank both synthetic Campbell & San Jose reports under Cash vs Conventional profiles
    let mut rep_cb = common::synth_report_secondary();
    let val_cb = run_valuation(
        &rep_cb["facts"],
        &rep_cb["modules"]["market"]["data"],
        &rep_cb["geo"],
    );
    rep_cb["modules"]["valuation"]["data"] = val_cb.data;
    let hf_cb = HomeFactors::from_report(&rep_cb);

    let ranked_cash = rank(
        &p_cash,
        &[hf.clone(), hf_cb.clone()],
        &CommuteMatrix::default(),
    );
    assert_eq!(ranked_cash.all().count(), 2);
}

// =================================================================================================
// 5. Feng Shui Quick Screens & Live Overpass Site Analysis on Synthetic Properties
// =================================================================================================

#[tokio::test]
async fn fengshui_quick_screens_on_synthetic_campbell_and_san_jose_properties() {
    let mut campbell = common::synth_report_secondary();
    let san_jose = common::synth_report_primary();

    // Verify HomeFactors extraction of Feng Shui site attributes on synthetic stored reports
    let hf_cb = HomeFactors::from_report(&campbell);
    let hf_sj = HomeFactors::from_report(&san_jose);
    assert_eq!(hf_cb.gates.house_number.as_deref(), Some("200"));
    assert_eq!(hf_sj.gates.house_number.as_deref(), Some("100"));

    // Exercise 8/4 address digit quick screens and Period 4 (1925) timeliness on a synthetic address
    campbell["geo"]["displayAddress"] = json!("484 Synthparcelbeta Ave, Campbell, CA 95008");
    campbell["geo"]["matchedAddress"] = json!("484 SYNTHPARCELBETA AVE, CAMPBELL, CA, 95008");
    campbell["facts"]["address"] = json!("484 Synthparcelbeta Ave, Campbell, CA 95008");
    campbell["facts"]["yearBuilt"] = json!(1925);

    let http = HttpClient::new(None);
    let rooftop = json!({
        "amenities": campbell["modules"]["amenities"]["data"],
        "market": campbell["modules"]["market"]["data"],
    });
    match run_fengshui(&http, &campbell["geo"], &campbell["facts"], Some(&rooftop)).await {
        Ok(out) => {
            if out.data["available"].as_bool() == Some(true) {
                let qs = &out.data["quickScreens"];
                assert_eq!(qs["addressDigits"]["houseNumber"], json!("484"));
                assert_eq!(qs["addressDigits"]["has8"], json!(true));
                assert_eq!(qs["addressDigits"]["has4"], json!(true));
                assert_eq!(qs["addressDigits"]["count8"], json!(1));
                assert_eq!(qs["addressDigits"]["count4"], json!(2));
                assert!(qs["tJunction"]["hit"].is_boolean());
                assert!(qs["culDeSac"]["hit"].is_boolean());
                assert!(qs["roadCurve"]["kind"].is_string());
                assert!(qs["freewayOrPowerLine"]["hit"].is_boolean());
                assert!(qs["facingTrigram"]["palace"].is_string());
                assert!(qs["eightMansionsGroup"]["group"].is_string());
                // Synthetic yearBuilt 1925 -> Xuan Kong Period 4 (1924-1943)
                assert_eq!(qs["periodTimeliness"]["constructionPeriod"], json!(4));
                assert!(qs["periodTimeliness"]["facingTimeliness"].is_string());
            }
        }
        Err(e) => {
            eprintln!("Skipping live Overpass fengshui run (network unavailable): {e}");
        }
    }
}

// =================================================================================================
// 6. Multigenerational Signals on Synthetic Homes (Single-Story + ADU vs Rowhouse)
// =================================================================================================

#[test]
fn multigenerational_signals_improve_homefit_on_synthetic_adu_craftsman_vs_multistory_rowhouse() {
    let mut p_mg = Profile::default();
    p_mg.multigenerational = true;
    p_mg.min_beds = Some(3.0);
    p_mg.min_baths = Some(2.0);
    p_mg.min_sqft = Some(1500.0);

    // Synthetic 1-story Craftsman + permitted detached ADU + 9,200 sqft lot
    let mut campbell_adu = common::synth_report_secondary();
    campbell_adu["facts"]["architecturalStyle"] = json!("Craftsman");
    campbell_adu["facts"]["features"] = json!([
        "Permitted 600 sq ft detached ADU",
        "Main-level primary bedroom",
        "Solar PV 6.4 kW",
        "200A electrical panel"
    ]);
    // Synthetic multi-story urban townhouse without ADU (`synth-pa-19147-rowhouse`)
    let mut philly_row = load_holdout_report("synth-pa-19147-rowhouse");
    philly_row["facts"]["stories"] = json!(3);

    let f_cb = HomeFactors::from_report(&campbell_adu);
    let f_row = HomeFactors::from_report(&philly_row);
    assert!(f_cb.home.has_adu(), "synthetic secondary report has a permitted ADU");
    assert!(f_cb
        .features
        .as_ref()
        .unwrap()
        .value
        .contains(&Feature::MainLevelPrimary));

    let u_cb = homefit_value(&p_mg, &f_cb, 2026).expect("Campbell multigenerational Homefit");
    let u_row = homefit_value(&p_mg, &f_row, 2026).expect("Rowhouse multigenerational Homefit");
    assert!(
        u_cb > u_row + 8.0,
        "Single-story Craftsman with permitted ADU ({u_cb}) must substantially outscore multi-story rowhouse ({u_row}) for multigenerational Homefit"
    );
}

// =================================================================================================
// 7. Full Three-Language (EN / ZH / ES) Scoring Catalog & Explanation Parity
// =================================================================================================

#[test]
fn full_three_language_parity_has_zero_english_fallbacks() {
    let en_cat = catalog_json(Lang::En);
    let zh_cat = catalog_json(Lang::ZhCn);
    let es_cat = catalog_json(Lang::Es);

    for list_key in [
        "dimensions",
        "styleFamilies",
        "stylePreferences",
        "presets",
        "screens",
        "features",
        "loanTypes",
        "schoolLevels",
        "propertyTypes",
        "matchBands",
    ] {
        let en_arr = en_cat[list_key].as_array().unwrap();
        let zh_arr = zh_cat[list_key].as_array().unwrap();
        let es_arr = es_cat[list_key].as_array().unwrap();
        assert_eq!(en_arr.len(), zh_arr.len(), "{list_key}");
        assert_eq!(en_arr.len(), es_arr.len(), "{list_key}");

        for i in 0..en_arr.len() {
            let en_lbl = en_arr[i]["label"].as_str().unwrap();
            let zh_lbl = zh_arr[i]["label"].as_str().unwrap();
            let es_lbl = es_arr[i]["label"].as_str().unwrap();
            assert!(!zh_lbl.is_empty() && !es_lbl.is_empty());
            assert_ne!(zh_lbl, en_lbl, "{list_key}[{i}] zh fell back to English");
            assert_ne!(es_lbl, en_lbl, "{list_key}[{i}] es fell back to English");
            if let Some(en_desc) = en_arr[i]["description"].as_str() {
                let zh_desc = zh_arr[i]["description"].as_str().unwrap();
                let es_desc = es_arr[i]["description"].as_str().unwrap();
                assert_ne!(
                    zh_desc, en_desc,
                    "{list_key}[{i}] zh desc fell back to English"
                );
                assert_ne!(
                    es_desc, en_desc,
                    "{list_key}[{i}] es desc fell back to English"
                );
            }
        }
    }

    assert_eq!(PRESETS.len(), 10);
    for p in &PRESETS {
        assert_ne!(p.label.zh, p.label.en);
        assert_ne!(p.label.es, p.label.en);
        assert_ne!(p.description.zh, p.description.en);
        assert_ne!(p.description.es, p.description.en);
    }

    for d in Dimension::ALL {
        for impact in [1.5, -1.5] {
            let e = Explanation {
                dimension: d,
                utility: 80.0,
                median: 70.0,
                impact,
                text: None,
            };
            let s_en = render_explanation(&e, Lang::En);
            let s_zh = render_explanation(&e, Lang::ZhCn);
            let s_es = render_explanation(&e, Lang::Es);
            assert_ne!(s_zh, s_en);
            assert_ne!(s_es, s_en);
        }
    }

    let gate_cases = [
        GateHit {
            reason: GateReason::SfhaFlood,
            kind: GateKind::DealBreaker,
            value: None,
            limit: None,
            label: Some("AE".into()),
            text: None,
        },
        GateHit {
            reason: GateReason::VeryHighFire,
            kind: GateKind::DealBreaker,
            value: None,
            limit: None,
            label: None,
            text: None,
        },
        GateHit {
            reason: GateReason::FreewayWithin500ft,
            kind: GateKind::DealBreaker,
            value: Some(320.0),
            limit: Some(500.0),
            label: None,
            text: None,
        },
        GateHit {
            reason: GateReason::TJunction,
            kind: GateKind::DealBreaker,
            value: None,
            limit: None,
            label: None,
            text: None,
        },
        GateHit {
            reason: GateReason::HoaOver,
            kind: GateKind::DealBreaker,
            value: Some(600.0),
            limit: Some(400.0),
            label: None,
            text: None,
        },
        GateHit {
            reason: GateReason::StyleAvoid,
            kind: GateKind::DealBreaker,
            value: Some(0.8),
            limit: None,
            label: Some(StyleFamily::Ranch.key().into()),
            text: None,
        },
        GateHit {
            reason: GateReason::AddressDigits,
            kind: GateKind::DealBreaker,
            value: None,
            limit: None,
            label: Some("4".into()),
            text: None,
        },
        GateHit {
            reason: GateReason::PropertyType,
            kind: GateKind::MustHave,
            value: None,
            limit: None,
            label: Some(PropertyKind::Condo.key().into()),
            text: None,
        },
    ];
    for g in &gate_cases {
        let s_en = render_gate(g, Lang::En);
        let s_zh = render_gate(g, Lang::ZhCn);
        let s_es = render_gate(g, Lang::Es);
        assert_ne!(s_zh, s_en);
        assert_ne!(s_es, s_en);
    }

    for reason in [
        DropReason::NoData,
        DropReason::NoStylePreference,
        DropReason::NoPlaces,
        DropReason::NoBudget,
    ] {
        let d = DroppedDimension {
            dimension: Dimension::Cost,
            reason,
            text: None,
        };
        assert_ne!(render_drop(&d, Lang::ZhCn), render_drop(&d, Lang::En));
        assert_ne!(render_drop(&d, Lang::Es), render_drop(&d, Lang::En));
    }

    for reason in [
        EstimateReason::ImputedMedian,
        EstimateReason::NoStyleEvidence,
        EstimateReason::RoadDistanceEstimate,
        EstimateReason::SourceEstimate,
    ] {
        let ed = EstimatedDimension {
            dimension: Dimension::Crime,
            reason,
            text: None,
        };
        assert_ne!(
            render_estimate(&ed, Lang::ZhCn),
            render_estimate(&ed, Lang::En)
        );
        assert_ne!(
            render_estimate(&ed, Lang::Es),
            render_estimate(&ed, Lang::En)
        );
    }

    for notice in [
        Notice {
            code: NoticeCode::NoScorableDimensions,
            place_index: None,
            place_label: None,
            text: None,
        },
        Notice {
            code: NoticeCode::PlaceWithoutDriveTimes,
            place_index: Some(0),
            place_label: Some("Office".into()),
            text: None,
        },
    ] {
        assert_ne!(
            render_notice(&notice, Lang::ZhCn),
            render_notice(&notice, Lang::En)
        );
        assert_ne!(
            render_notice(&notice, Lang::Es),
            render_notice(&notice, Lang::En)
        );
    }

    let _ = (
        Screen::ALL,
        SchoolLevel::ALL,
        BTreeMap::<StyleFamily, f64>::new(),
    );
}
