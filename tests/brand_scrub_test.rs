// Verification suite enforcing the server-side zero-competitor-brand rule across EN, 中文, and ES:
// - Unit tests for `has_competitor_brand`, `is_competitor_portal_url`, and `scrub_competitor_brands`
//   (including CJK adjacency, honesty rules, navigation `compass` preservation, and sentence dropping).
// - End-to-end checks over both procedural synthetic reports (`common::synth_report_secondary` and
//   `common::synth_report_primary`) across `normalize_stored_report`, `sanitize_report_for_html`,
//   `run_property_valuation`, `build_deterministic_analyst_fallback`, `build_deterministic_walkthrough_slides`,
//   `extract_deal_room_item` + `render_deal_room_html`, `ask_property_question`, and `build_match_alert_email_for_lang`.

mod common;

use open_estate::ai::analyst::build_deterministic_analyst_fallback;
use open_estate::ai::ask::ask_property_question;
use open_estate::ai::slides::build_deterministic_walkthrough_slides;
use open_estate::property::valuation::run_valuation;
use open_estate::search::alerts::{build_match_alert_email_for_lang, AlertMatchedItem};
use open_estate::shared::{
    has_competitor_brand, is_competitor_portal_url, scrub_competitor_brands, EnvConfig, HttpClient,
};
use open_estate::web::deal_room::{extract_deal_room_item, render_deal_room_html};
use open_estate::web::render::{normalize_stored_report, sanitize_report_for_html};
use serde_json::{json, Value};

fn load_synth_report(slug: &str) -> Value {
    let mut rep = if slug == common::SYNTH_SECONDARY_SLUG {
        common::synth_report_secondary()
    } else {
        common::synth_report_primary()
    };
    rep["walkthroughSlides"] = build_deterministic_walkthrough_slides(&rep);
    rep["propertyMedia"] = json!({
        "externalGalleryLinks": [
            { "label": "Street View", "url": "https://maps.google.com/?q=37.258000,-121.898000" },
            { "label": "Portal Index", "url": "https://www.movoto.com/" }
        ]
    });
    rep
}

/// Recursively collects any user-visible string in `val` that still triggers `has_competitor_brand`,
/// excluding the exempted bottom Sources arrays (`report.sources` and `facts.sources`), raw URLs,
/// and internal identifier keys (`key`, `id`, `visualKind`, `targetSection`).
fn collect_brand_leaks(val: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match val {
        Value::String(s) => {
            if s.starts_with("http://") || s.starts_with("https://") {
                return;
            }
            if has_competitor_brand(s) {
                out.push((path.to_string(), s.clone()));
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let child_path = format!("{path}[{i}]");
                collect_brand_leaks(item, &child_path, out);
            }
        }
        Value::Object(map) => {
            for (k, child) in map {
                // Exempted bottom Data Sources lists per product rule:
                if (path.is_empty() && k == "sources")
                    || (path == "/facts" && k == "sources")
                    || (path == "/modules/facts/data" && k == "sources")
                    || (path == "/modules/facts" && k == "sources")
                {
                    continue;
                }
                // Internal identifier/lookup keys read by JS, never shown as prose:
                if matches!(
                    k.as_str(),
                    "key" | "id" | "visualKind" | "targetSection" | "slug" | "url" | "thumbUrl"
                ) || k.ends_with("Url")
                    || k.ends_with("Id")
                {
                    continue;
                }
                let child_path = format!("{path}/{k}");
                collect_brand_leaks(child, &child_path, out);
            }
        }
        _ => {}
    }
}

#[test]
fn test_scrub_competitor_brands_en_zh_es_and_honesty_invariants() {
    // 1. Fixture prose: multi-AVM clauses in EN must become "third-party estimates", NEVER "True Estimate"
    let s1 = "The $2.114M medium-confidence blend sits below the $2.32M last sale and near the $2.093M Redfin and $2.135M Movoto estimates.";
    let r1 = scrub_competitor_brands(s1);
    assert!(!has_competitor_brand(&r1), "residual brand in: {r1}");
    assert!(
        r1.contains("$2.093M") && r1.contains("$2.135M"),
        "numbers must stay intact: {r1}"
    );
    assert!(
        r1.to_ascii_lowercase().contains("third-party estimates"),
        "expected third-party estimates vocabulary: {r1}"
    );
    assert!(
        !r1.to_ascii_lowercase().contains("true estimate"),
        "honesty violation: must never label third-party figures True Estimate: {r1}"
    );

    let s2 = "Blended value is $2.114M with a $1.886M–$2.342M range, while Redfin and Movoto estimate $2.093M and $2.135M.";
    let r2 = scrub_competitor_brands(s2);
    assert!(!has_competitor_brand(&r2), "residual brand in: {r2}");
    assert!(
        r2.contains("third-party estimates are $2.093M and $2.135M"),
        "got: {r2}"
    );

    // 2. Lot note & school wrapper rewrites
    let lot = "MLS / Redfin / Zillow 10,018 sq ft (0.2300 ac); Santa Clara County Assessor roll 9,583 sq ft.";
    let r_lot = scrub_competitor_brands(lot);
    assert!(!has_competitor_brand(&r_lot), "residual brand in: {r_lot}");
    assert!(r_lot.contains("10,018 sq ft") && r_lot.contains("9,583 sq ft"));

    let sch = "GreatSchools 8/10 (via Movoto); Walk Score 74 via Redfin.";
    let r_sch = scrub_competitor_brands(sch);
    assert!(!has_competitor_brand(&r_sch), "residual brand in: {r_sch}");
    assert!(r_sch.contains("GreatSchools 8/10"));
    assert!(r_sch.contains("Walkability score 74"));

    // 3. Index & trademark vocabulary in EN, 中文 (including ASCII adjacent to CJK), and ES
    let en_idx = "Zillow Zestimate $2,400,000, ZHVI appreciation, ZORI rent, and Zillow 3D Home.";
    let r_en_idx = scrub_competitor_brands(en_idx);
    assert!(
        !has_competitor_brand(&r_en_idx),
        "residual brand in: {r_en_idx}"
    );
    assert!(r_en_idx.contains("Third-party estimate $2,400,000"));
    assert!(r_en_idx.contains("ZIP home-value index"));
    assert!(r_en_idx.contains("ZIP rent index"));
    assert!(!r_en_idx.contains("True Estimate"));

    let zh = "周边可比成交及同邮编ZHVI指数共同界定合理出价区间，参考Zillow邮编预测、ZORI租金与Walk Score 74，而Redfin与Movoto估值分别为 $2.093M 与 $2.135M。";
    assert!(
        has_competitor_brand(zh),
        "must detect ASCII brand adjacent to CJK"
    );
    let r_zh = scrub_competitor_brands(zh);
    assert!(!has_competitor_brand(&r_zh), "residual brand in ZH: {r_zh}");
    assert!(r_zh.contains("邮编房价指数"), "got: {r_zh}");
    assert!(r_zh.contains("邮编租金指数"), "got: {r_zh}");
    assert!(r_zh.contains("步行便利指数"), "got: {r_zh}");
    assert!(r_zh.contains("第三方估值"), "got: {r_zh}");
    assert!(!r_zh.contains("真实估值"));

    let es = "Las ventas comparables y la tendencia ZHVI del código postal responden al pronóstico Zillow del código postal, ZORI y Walk Score 74; estimaciones de Redfin y Movoto de $2.093M y $2.135M.";
    let r_es = scrub_competitor_brands(es);
    assert!(!has_competitor_brand(&r_es), "residual brand in ES: {r_es}");
    assert!(
        r_es.to_lowercase()
            .contains("índice de valor del código postal"),
        "got: {r_es}"
    );
    assert!(
        r_es.to_lowercase()
            .contains("índice de renta del código postal"),
        "got: {r_es}"
    );
    assert!(
        r_es.to_lowercase().contains("índice de caminabilidad"),
        "got: {r_es}"
    );
    assert!(
        r_es.to_lowercase().contains("estimaciones de terceros"),
        "got: {r_es}"
    );

    // 4. Navigation `compass` must NOT be treated as Compass brokerage
    let nav = "Cultural site reading: verify exact door Compass bearing and compass direction (180° S) with a luopan.";
    assert!(!has_competitor_brand(nav));
    assert_eq!(scrub_competitor_brands(nav), nav);

    let brok = "Listed by Compass brokerage in 2024. Verified parcel lot is 6,300 sq ft.";
    assert!(has_competitor_brand(brok));
    let r_brok = scrub_competitor_brands(brok);
    assert!(
        !has_competitor_brand(&r_brok),
        "residual brand in: {r_brok}"
    );
    assert!(r_brok.contains("Verified parcel lot is 6,300 sq ft."));

    // 5. Multi-sentence safety net drops un-rewritable brand prose cleanly while keeping factual sentences
    let mixed = "Opendoor made an instant cash offer on a nearby home. County Assessor records confirm 1,956 sq ft.";
    let r_mixed = scrub_competitor_brands(mixed);
    assert_eq!(r_mixed, "County Assessor records confirm 1,956 sq ft.");
}

#[test]
fn test_fixture_reports_sanitized_html_and_server_outputs_have_zero_competitor_brands() {
    for slug in [common::SYNTH_SECONDARY_SLUG, common::SYNTH_PRIMARY_SLUG] {
        let mut raw_report = load_synth_report(slug);
        let orig_sources_len = raw_report["sources"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        let orig_fact_sources_len = raw_report["facts"]["sources"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        // Normalize stored report (idempotent on second pass)
        let _ = normalize_stored_report(&mut raw_report);
        assert!(
            !normalize_stored_report(&mut raw_report),
            "normalize_stored_report must remain idempotent for {slug}"
        );

        // Sanitize for HTML rendering (`<script id="report-data">`)
        let clean = sanitize_report_for_html(&raw_report);

        // Bottom Sources lists must remain untouched
        assert_eq!(
            clean["sources"].as_array().map(|a| a.len()).unwrap_or(0),
            orig_sources_len,
            "report.sources must be preserved for {slug}"
        );
        assert_eq!(
            clean["facts"]["sources"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            orig_fact_sources_len,
            "facts.sources must be preserved for {slug}"
        );

        // Slide 3 internal ID (`zestimate_comps`) must remain intact for client JS rendering
        assert_eq!(
            clean["walkthroughSlides"]["slides"][2]["id"],
            json!("zestimate_comps")
        );
        assert_eq!(
            clean["walkthroughSlides"]["slides"][2]["visualKind"],
            json!("zestimate_comps")
        );

        // Portal gallery & county dossier links must be stripped from HTML payload while Street View / Bing stay
        if let Some(links) = clean
            .pointer("/propertyMedia/externalGalleryLinks")
            .and_then(Value::as_array)
        {
            assert!(
                !links.is_empty(),
                "Google/Bing curbside links must remain in externalGalleryLinks for {slug}"
            );
            for l in links {
                let u = l.get("url").and_then(Value::as_str).unwrap_or("");
                assert!(
                    !is_competitor_portal_url(u),
                    "competitor portal URL leaked into externalGalleryLinks for {slug}: {u}"
                );
            }
        }
        if let Some(portals) = clean
            .pointer("/facts/countyRecords/officialPortals")
            .and_then(Value::as_array)
        {
            for p in portals {
                let u = p.get("url").and_then(Value::as_str).unwrap_or("");
                assert!(
                    !is_competitor_portal_url(u),
                    "competitor portal URL leaked into countyRecords.officialPortals for {slug}: {u}"
                );
            }
        }

        // Zero competitor brand names across ALL visible strings in sanitized report JSON (EN + zh + es)
        let mut leaks = Vec::new();
        collect_brand_leaks(&clean, "", &mut leaks);
        assert!(
            leaks.is_empty(),
            "competitor brand leaked in sanitized report for {slug}: {leaks:#?}"
        );

        // Re-run fresh Rust server generators on this fixture and verify zero brand leaks:
        // (a) Property valuation module
        let val_out = run_valuation(
            raw_report.get("facts").unwrap_or(&Value::Null),
            raw_report
                .pointer("/modules/market/data")
                .unwrap_or(&Value::Null),
            raw_report.get("geo").unwrap_or(&Value::Null),
        );
        let mut val_leaks = Vec::new();
        collect_brand_leaks(&val_out.data, "/valuation", &mut val_leaks);
        assert!(
            val_leaks.is_empty(),
            "competitor brand leaked in fresh valuation output for {slug}: {val_leaks:#?}"
        );

        // (b) Deterministic AI analyst fallback (EN + translations.zh + translations.es)
        let det_ai = build_deterministic_analyst_fallback(&raw_report);
        let mut ai_leaks = Vec::new();
        collect_brand_leaks(&det_ai, "/ai", &mut ai_leaks);
        assert!(
            ai_leaks.is_empty(),
            "competitor brand leaked in deterministic analyst output for {slug}: {ai_leaks:#?}"
        );

        // (c) Deterministic 8-slide walkthrough deck (EN + zh + es)
        let deck = build_deterministic_walkthrough_slides(&raw_report);
        let mut deck_leaks = Vec::new();
        collect_brand_leaks(&deck, "/walkthroughSlides", &mut deck_leaks);
        assert!(
            deck_leaks.is_empty(),
            "competitor brand leaked in walkthrough deck for {slug}: {deck_leaks:#?}"
        );

        // (d) Deal Room item & SSR HTML across EN, zh, es
        let dr_item = extract_deal_room_item(slug, &raw_report, None, "http://127.0.0.1:48815");
        let dr_json = serde_json::to_value(&dr_item).unwrap();
        let mut dr_leaks = Vec::new();
        collect_brand_leaks(&dr_json, "/dealRoomItem", &mut dr_leaks);
        assert!(
            dr_leaks.is_empty(),
            "competitor brand leaked in DealRoomItem for {slug}: {dr_leaks:#?}"
        );
        let script_re = regex::Regex::new(r"(?is)<script\b[^>]*>.*?</script>").unwrap();
        for lang in ["en", "zh", "es"] {
            let dr_html = render_deal_room_html(&[dr_item.clone()], &[], &[], lang);
            let dr_body_only = script_re.replace_all(&dr_html, " ");
            assert!(
                !has_competitor_brand(&dr_body_only),
                "competitor brand leaked in Deal Room SSR HTML ({lang}) for {slug}"
            );
        }
    }
}

#[tokio::test]
async fn test_qa_and_alert_emails_scrub_competitor_brands_in_en_zh_es() {
    let report = load_synth_report(common::SYNTH_PRIMARY_SLUG);
    let http = HttpClient::new(None);
    let empty_env = EnvConfig::from_map(std::collections::HashMap::new());

    for q in [
        "How does the valuation compare with nearby comps?",
        "这套房子的综合估值和物业费是多少？",
    ] {
        let ans = ask_property_question(&http, &empty_env, &report, q, None, None, None, None)
            .await
            .expect("deterministic QA");
        let answer_str = ans["answer"].as_str().unwrap_or("");
        assert!(
            !has_competitor_brand(answer_str),
            "competitor brand leaked in QA answer for `{q}`: {answer_str}"
        );
    }

    let matched = vec![AlertMatchedItem {
        event_type: "new_listing".to_string(),
        old_price: None,
        event_key: "evt-1".to_string(),
        listing: json!({
            "address": common::SYNTH_PRIMARY_ADDRESS,
            "status": "for_sale",
            "price": 2_040_000,
            "beds": 4.0,
            "baths": 2.5,
            "sqft": 2040,
            "lotSqft": 6600,
            "schoolGrade": "A-",
            "schoolScore": 8.0,
            "reportSlug": common::SYNTH_PRIMARY_SLUG
        }),
        match_score: 91,
        match_tier: "Strong Match".to_string(),
        why_it_matches:
            "Priced near the $2.093M Redfin and $2.135M Movoto estimates with Walk Score 74."
                .to_string(),
        tradeoffs: Some("Above ZIP ZHVI median.".to_string()),
    }];
    let zips = vec!["95118".to_string()];

    for lang in ["en-US", "zh-CN", "es"] {
        let email = build_match_alert_email_for_lang(
            Some("Alex"),
            "4 bed in 95118 below Zillow Zestimate with high Walk Score",
            &zips,
            &matched,
            "https://openestate.org",
            "tok_test_123",
            lang,
        );
        assert!(
            !has_competitor_brand(&email.subject),
            "brand leaked in email subject ({lang}): {}",
            email.subject
        );
        assert!(
            !has_competitor_brand(&email.text),
            "brand leaked in email text ({lang}): {}",
            email.text
        );
        assert!(
            !has_competitor_brand(&email.html),
            "brand leaked in email html ({lang})"
        );
    }
}
