// Integration & Playwright Headless Chromium regression tests for:
// 1. Open Estate (`open-estate.local`) crate/brand rename + header/cookie resolution
// 2. Backend Mozilla `fluent-templates` i18n (`locales/en-US/main.ftl`, `locales/zh-CN/main.ftl`, `locales/es/main.ftl`)
// 3. Vendored MIT `i18next` (`assets/vendor/i18next.min.js`) + `Intl` formatting + zero-leak HTML sanitizer
// 4. Strictly Simplified Chinese-only `#fengshui` (堪舆风水) UI + AI synthesis on synthetic property reports
// 5. Real Headless Chromium (`light` and `dark` `prefers-color-scheme`) verification of interactive UI modules
//    and synchronous `window.setLang('zh' | 'es' | 'en')` switching without `/api/translate` network round-trips.

mod common;

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
};
use open_estate::ai::build_deterministic_analyst_fallback;
use open_estate::cre::build_cre_report_section;
use open_estate::db::DbStore;
use open_estate::guardrails::{extract_browser_client_id, HttpDefenseShield};
use open_estate::modules::{
    buyer_diligence::build_buyer_diligence_and_hoa_deterministic,
    deed_title_marketplace::build_deed_title_and_marketplace,
    engagement::build_engagement,
    insurance::run_insurance_with_condition,
    xuankong::{
        annual_chart, ba_zhai, chart_to_json, flying_star_chart, mountain_of, period_range,
    },
};
use open_estate::property::{evaluate_condition_and_capex, score_property_condition_from_photos};
use open_estate::search::{build_match_alert_email_for_lang, AlertMatchedItem};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::{
    i18n::{
        is_es, is_zh, resolve_lang, short_code, t, LANG_COOKIE_NAME, LANG_EN, LANG_ES, LANG_ZH,
    },
    render::{
        normalize_stored_report, render_report_html, sanitize_report_for_html, I18NEXT_JS,
        I18N_EN_JSON, I18N_ES_JSON, I18N_ZH_JSON,
    },
    server::{build_router, AppState},
    summary::{apply_native_translations, build_summary_translations, summarize_report},
    ui::{
        render_home_html_for_lang, render_job_html_for_lang, render_unsub_html_for_lang,
    },
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tower::ServiceExt;
use uuid::Uuid;

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

fn load_synth_report(slug: &str) -> Value {
    common::synth_report_for_slug(slug)
}

fn enrich_synth_report_for_ui(slug: &str) -> Value {
    let mut rep = load_synth_report(slug);
    let env = EnvConfig::from_map(HashMap::new());
    let display_address = rep["meta"]["displayAddress"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let diligence = build_buyer_diligence_and_hoa_deterministic(&rep);
    rep["hoaProfile"] = diligence.hoa_profile;
    rep["buyerDiligence"] = diligence.buyer_diligence;

    let mut cond = evaluate_condition_and_capex(
        &rep["facts"],
        &[],
        rep.pointer("/modules/hazards"),
        &rep["geo"],
        rep.pointer("/modules/rooftop/data"),
    );
    let photo_cond = score_property_condition_from_photos(&json!({
        "yearBuilt": rep["facts"]["yearBuilt"],
        "sqft": rep["facts"]["sqft"],
        "remarks": rep["facts"]["renovations"],
        "features": rep["facts"]["features"],
    }));
    cond["photoCondition"] = photo_cond.clone();
    cond["conditionGrade"] = photo_cond["grade"].clone();
    cond["conditionScore"] = photo_cond["conditionScore"].clone();
    cond["valuationMultiplier"] = photo_cond["valuationMultiplier"].clone();
    rep["condition"] = cond;

    let est_val = rep
        .pointer("/modules/valuation/data/estimate/value")
        .and_then(Value::as_i64)
        .or_else(|| rep.pointer("/facts/lastSale/price").and_then(Value::as_i64));
    let ins_out = run_insurance_with_condition(
        &rep["facts"],
        rep.pointer("/modules/hazards").unwrap_or(&Value::Null),
        rep.pointer("/modules/environment").unwrap_or(&Value::Null),
        &rep["geo"],
        rep.get("condition"),
        est_val,
    );
    rep["insurance"] = ins_out.data;

    let dtm = build_deed_title_and_marketplace(&rep, &env);
    rep["titleInsuranceAndDeed"] = dtm.title_insurance_and_deed;
    rep["marketplace"] = dtm.marketplace;
    rep["engagement"] = build_engagement(slug, &display_address, &rep["facts"], None, &[], None, 3);
    rep["cre"] = build_cre_report_section(&rep, None, true).data;
    rep["summary"] = summarize_report(&rep);

    let fb = build_deterministic_analyst_fallback(&rep);
    apply_native_translations(&mut rep, fb.get("translations"), true);
    normalize_stored_report(&mut rep);
    rep
}

fn assert_no_leaks(html: &str, context: &str) {
    assert!(
        !open_estate::shared::has_retired_brand(html),
        "{context} leaked the retired pre-rebrand product name"
    );
    for tok in FORBIDDEN_HTML_TOKENS {
        assert!(
            !html.contains(tok),
            "{context} leaked forbidden token `{tok}`"
        );
    }
}

fn check_inline_scripts_with_node(html: &str, label: &str) {
    let Ok(node_ver) = std::process::Command::new("node").arg("--version").output() else {
        return;
    };
    if !node_ver.status.success() {
        return;
    }

    let mut pos = 0usize;
    let mut idx = 0usize;
    while let Some(open_rel) = html[pos..].find("<script") {
        let open_start = pos + open_rel;
        let Some(tag_end_rel) = html[open_start..].find('>') else {
            break;
        };
        let attrs = &html[open_start..open_start + tag_end_rel];
        let body_start = open_start + tag_end_rel + 1;
        let Some(close_rel) = html[body_start..].find("</script>") else {
            break;
        };
        let body = &html[body_start..body_start + close_rel];
        pos = body_start + close_rel + "</script>".len();

        let attrs_lc = attrs.to_ascii_lowercase();
        if attrs_lc.contains("application/json")
            || attrs_lc.contains("src=")
            || body.trim().is_empty()
        {
            continue;
        }
        let tmp = std::env::temp_dir().join(format!(
            "openestate-js-chk-{label}-{idx}-{}.js",
            Uuid::new_v4()
        ));
        std::fs::write(&tmp, body).expect("write temp JS");
        let out = std::process::Command::new("node")
            .arg("--check")
            .arg(&tmp)
            .output()
            .expect("run node --check");
        let _ = std::fs::remove_file(&tmp);
        assert!(
            out.status.success(),
            "node --check failed for {label} <script> #{idx}:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        idx += 1;
    }
    assert!(idx > 0, "expected at least one inline <script> in {label}");
}

fn discover_playwright_module() -> Option<String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(custom) = std::env::var("PLAYWRIGHT_MODULE_PATH") {
        if !custom.trim().is_empty() {
            candidates.push(custom);
        }
    }
    candidates.push("playwright".into());
    candidates.push(
        manifest_dir
            .join("node_modules/playwright")
            .to_string_lossy()
            .into_owned(),
    );
    candidates.push(
        manifest_dir
            .join("e2e/node_modules/playwright")
            .to_string_lossy()
            .into_owned(),
    );
    if let Ok(home) = std::env::var("HOME") {
        if let Ok(entries) = std::fs::read_dir(&home) {
            for e1 in entries.flatten() {
                let n1 = e1.file_name();
                if n1.to_string_lossy().starts_with('.') {
                    continue;
                }
                let p1 = e1.path();
                if !p1.is_dir() {
                    continue;
                }
                candidates.push(p1.join("node_modules/playwright").to_string_lossy().into_owned());
                if let Ok(sub) = std::fs::read_dir(&p1) {
                    for e2 in sub.flatten() {
                        let n2 = e2.file_name();
                        if n2.to_string_lossy().starts_with('.') {
                            continue;
                        }
                        let p2 = e2.path();
                        if p2.is_dir() {
                            candidates.push(p2.join("node_modules/playwright").to_string_lossy().into_owned());
                            candidates.push(p2.join("playwright").to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }
    }

    for cand in candidates {
        if cand != "playwright" && !Path::new(&cand).exists() {
            continue;
        }
        let probe = format!(
            "const {{ chromium }} = require({}); const fs = require('fs'); const p = chromium.executablePath(); if (!p || !fs.existsSync(p)) process.exit(2);",
            serde_json::to_string(&cand).unwrap()
        );
        if let Ok(out) = std::process::Command::new("node")
            .arg("-e")
            .arg(&probe)
            .output()
        {
            if out.status.success() {
                return Some(cand);
            }
        }
    }
    None
}

#[test]
fn test_1_brand_rename_headers_cookies_and_fluent_bundles() {
    let env = EnvConfig::from_map(HashMap::new());
    assert_eq!(env.brand_name(), "Open Estate");
    assert_eq!(env.brand_domain(), "open-estate.local");

    // Client ID is read from X-OpenEstate-Client-Id
    let mut h_new = HeaderMap::new();
    h_new.insert(
        "x-openestate-client-id",
        HeaderValue::from_static("client-abc-12345"),
    );
    assert_eq!(
        extract_browser_client_id(&h_new).as_deref(),
        Some("client-abc-12345")
    );
    assert_eq!(extract_browser_client_id(&HeaderMap::new()), None);

    // Data persisted before the rebrand is normalized on read/render, keeping casing and separators
    assert_eq!(
        open_estate::shared::normalize_retired_brand("Home Lens Report"),
        "Open Estate Report"
    );
    assert_eq!(
        open_estate::shared::normalize_retired_brand("home-lens light / HOME_LENS_TOKEN"),
        "open-estate light / OPEN_ESTATE_TOKEN"
    );
    assert!(!open_estate::shared::has_retired_brand("Open Estate Report"));

    // Fluent language resolution (?lang=, open_estate_lang cookie, Accept-Language, default en-US)
    assert_eq!(resolve_lang(Some("zh"), &HeaderMap::new()), LANG_ZH);
    assert_eq!(resolve_lang(Some("es"), &HeaderMap::new()), LANG_ES);
    assert_eq!(resolve_lang(Some("es-419"), &HeaderMap::new()), LANG_ES);
    assert_eq!(resolve_lang(Some("en-US"), &HeaderMap::new()), LANG_EN);
    assert_eq!(short_code(&LANG_EN), "en");
    assert_eq!(short_code(&LANG_ZH), "zh");
    assert_eq!(short_code(&LANG_ES), "es");
    let mut h_ck = HeaderMap::new();
    h_ck.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{LANG_COOKIE_NAME}=zh-CN")).unwrap(),
    );
    assert!(is_zh(&resolve_lang(None, &h_ck)));

    let mut h_ck_es = HeaderMap::new();
    h_ck_es.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{LANG_COOKIE_NAME}=es")).unwrap(),
    );
    assert!(is_es(&resolve_lang(None, &h_ck_es)));

    let mut h_acc = HeaderMap::new();
    h_acc.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US;q=0.5, zh-CN;q=0.95"),
    );
    assert_eq!(resolve_lang(None, &h_acc), LANG_ZH);

    let mut h_acc_es = HeaderMap::new();
    h_acc_es.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US;q=0.4, es-MX;q=0.9"),
    );
    assert_eq!(resolve_lang(None, &h_acc_es), LANG_ES);
    assert_eq!(resolve_lang(None, &HeaderMap::new()), LANG_EN);

    assert_eq!(t(&LANG_EN, "brand-name"), "Open Estate");
    assert_eq!(t(&LANG_ZH, "brand-name"), "Open Estate");
    assert_eq!(t(&LANG_ES, "brand-name"), "Open Estate");
    assert!(t(&LANG_ZH, "home-hero-heading").contains("美国房产"));
    assert!(!t(&LANG_ES, "home-hero-heading").is_empty());

    // Localized home / job / unsub pages & Fluent email alerts
    let home_en = render_home_html_for_lang(true, "sqlite", "sqlx", true, &[], "en-US");
    let home_zh = render_home_html_for_lang(true, "sqlite", "sqlx", true, &[], "zh-CN");
    let home_es = render_home_html_for_lang(true, "sqlite", "sqlx", true, &[], "es");
    assert!(home_en.contains("Open <b>Estate</b>"));

    // One brand mark everywhere: favicon, page-header logo and the offline report template share FAVICON_SVG
    let mark_uri = open_estate::web::favicon::LOGO_MARK_DATA_URI.as_str();
    assert!(open_estate::web::favicon::FAVICON_HEAD_TAGS.contains(mark_uri));
    assert!(home_en.contains(mark_uri) && home_en.contains(r#"class="logo-mark""#));
    assert!(
        !home_en.contains("<i>宅</i>"),
        "home header must use the brand mark"
    );
    let tpl = open_estate::web::render::REPORT_TEMPLATE_HTML;
    assert_eq!(
        tpl.matches(mark_uri).count(),
        2,
        "template.html favicon + header logo drifted from FAVICON_SVG"
    );
    assert!(open_estate::web::favicon::FAVICON_ICO.starts_with(&[0, 0, 1, 0]));
    assert!(open_estate::web::favicon::APPLE_TOUCH_ICON_PNG.starts_with(b"\x89PNG"));
    assert!(home_en.contains("Comprehensive US Property Due-Diligence"));
    assert!(home_en.contains("Instant valuation, natural hazard"));
    assert!(home_en.contains("Click or Enter to select"));
    assert!(home_en.contains("US Street Addresses"));
    assert!(home_zh.contains("美国房产全维度智能尽职调查"));
    assert!(home_zh.contains("lang=\"zh-CN\""));
    assert!(home_es.contains("lang=\"es\""));
    assert_no_leaks(&home_en, "home_en");
    assert_no_leaks(&home_zh, "home_zh");
    assert_no_leaks(&home_es, "home_es");
    check_inline_scripts_with_node(&home_en, "home_en");
    check_inline_scripts_with_node(&home_es, "home_es");

    let primary = load_synth_report(common::SYNTH_PRIMARY_SLUG);
    let primary_addr = primary["meta"]["displayAddress"]
        .as_str()
        .expect("synthetic displayAddress");
    let primary_slug = primary["meta"]["slug"].as_str().expect("synthetic slug");
    let primary_zip = primary["geo"]["zip"]
        .as_str()
        .expect("synthetic zip")
        .to_string();
    let primary_sale_price = primary["facts"]["lastSale"]["price"]
        .as_i64()
        .expect("synthetic lastSale.price");

    let job_zh = render_job_html_for_lang(
        "job-95118-1",
        primary_addr,
        primary_slug,
        primary_slug,
        "geocoding",
        "zh-CN",
    );
    assert!(job_zh.contains("Open Estate"));
    assert!(job_zh.contains("正在深度分析"));
    assert_no_leaks(&job_zh, "job_zh");
    check_inline_scripts_with_node(&job_zh, "job_zh");

    let job_es = render_job_html_for_lang(
        "job-95118-2",
        primary_addr,
        primary_slug,
        primary_slug,
        "geocoding",
        "es",
    );
    assert!(job_es.contains("lang=\"es\""));
    assert_no_leaks(&job_es, "job_es");
    check_inline_scripts_with_node(&job_es, "job_es");

    let unsub_zh = render_unsub_html_for_lang(
        true,
        "alerts@open-estate.local",
        std::slice::from_ref(&primary_zip),
        "zh-CN",
    );
    assert!(unsub_zh.contains("已成功退订"));
    assert_no_leaks(&unsub_zh, "unsub_zh");

    let unsub_es = render_unsub_html_for_lang(
        true,
        "alerts@open-estate.local",
        std::slice::from_ref(&primary_zip),
        "es",
    );
    assert!(unsub_es.contains("lang=\"es\""));
    assert_no_leaks(&unsub_es, "unsub_es");

    let synth_listing = json!({
        "address": primary_addr,
        "street": "100 Synthparcelalpha Ln",
        "city": primary["geo"]["city"],
        "state": primary["geo"]["state"],
        "zip": primary["geo"]["zip"],
        "beds": primary["facts"]["beds"],
        "baths": primary["facts"]["baths"],
        "sqft": primary["facts"]["sqft"],
        "status": "for_sale",
        "price": primary_sale_price,
    });

    let alert_zh = build_match_alert_email_for_lang(
        None,
        "4 bed in 95118",
        std::slice::from_ref(&primary_zip),
        &[AlertMatchedItem {
            event_type: "new".into(),
            old_price: None,
            event_key: "evt-95118-new".into(),
            listing: synth_listing.clone(),
            match_score: 94,
            match_tier: "Strong Match".into(),
            why_it_matches: "Matches 4 bedrooms and school district".into(),
            tradeoffs: None,
        }],
        "",
        "unsub-tok-95118-zh",
        "zh-CN",
    );
    assert!(alert_zh.subject.contains("Open Estate"));
    assert!(alert_zh
        .html
        .contains("Open <b style=\"color:#0f766e\">Estate</b>"));
    assert!(alert_zh.text.contains("https://open-estate.local/"));

    let alert_es = build_match_alert_email_for_lang(
        None,
        "4 hab en 95118",
        std::slice::from_ref(&primary_zip),
        &[AlertMatchedItem {
            event_type: "price_drop".into(),
            old_price: Some(primary_sale_price + 100_000),
            event_key: "evt-95118-drop".into(),
            listing: synth_listing,
            match_score: 94,
            match_tier: "Strong Match".into(),
            why_it_matches: "Matches 4 bedrooms and school district".into(),
            tradeoffs: None,
        }],
        "",
        "unsub-tok-95118-es",
        "es",
    );
    assert!(alert_es.subject.contains("Open Estate"));
    assert!(alert_es
        .html
        .contains("Open <b style=\"color:#0f766e\">Estate</b>"));
    assert!(alert_es.html.contains("lang=\"es\""));
    assert!(
        alert_es.text.contains("Ver propiedad")
            && alert_es.text.contains("Administrar alertas")
            && alert_es.html.contains("NUEVO LISTADO")
            && alert_es.html.contains("Cancelar suscripción")
            && !alert_es.html.contains("Compatibilidad")
            && !alert_es.html.contains("Why it matches")
    );
}

fn collect_leaf_keys(prefix: &str, v: &Value, out: &mut std::collections::BTreeSet<String>) {
    if let Some(obj) = v.as_object() {
        for (k, child) in obj {
            let next = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            collect_leaf_keys(&next, child, out);
        }
    } else {
        out.insert(prefix.to_string());
    }
}

#[test]
fn test_2_strictly_chinese_fengshui_and_template_contracts_on_real_fixtures() {
    // 1. Vendored MIT i18next + en/zh-CN/es JSON bundles are valid, aligned, and clean
    assert!(I18NEXT_JS.len() > 20_000, "vendored i18next.min.js missing");
    assert_no_leaks(I18NEXT_JS, "i18next.min.js");
    let en_json: Value = serde_json::from_str(I18N_EN_JSON).expect("en.json valid JSON");
    let zh_json: Value = serde_json::from_str(I18N_ZH_JSON).expect("zh-CN.json valid JSON");
    let es_json: Value = serde_json::from_str(I18N_ES_JSON).expect("es.json valid JSON");
    assert_eq!(en_json["brand"]["name"], "Open Estate");
    assert_eq!(zh_json["brand"]["domain"], "open-estate.local");
    assert_eq!(es_json["brand"]["domain"], "open-estate.local");
    assert_eq!(es_json["nav"]["fengshui"], "堪舆风水");
    assert_no_leaks(I18N_EN_JSON, "en.json");
    assert_no_leaks(I18N_ZH_JSON, "zh-CN.json");
    assert_no_leaks(I18N_ES_JSON, "es.json");

    let mut all_en = std::collections::BTreeSet::new();
    let mut all_zh = std::collections::BTreeSet::new();
    let mut all_es = std::collections::BTreeSet::new();
    collect_leaf_keys("", &en_json, &mut all_en);
    collect_leaf_keys("", &zh_json, &mut all_zh);
    collect_leaf_keys("", &es_json, &mut all_es);
    assert_eq!(all_en, all_zh, "full assets/locales key parity en vs zh-CN");
    assert_eq!(all_en, all_es, "full assets/locales key parity en vs es");

    let env_ex = include_str!("../.env.example");
    assert!(
        !env_ex.contains("GOOGLE_TRANSLATE_API_KEY"),
        ".env.example must not retain removed GOOGLE_TRANSLATE_API_KEY"
    );

    // 2. Xuan Kong & Ba Zhai computations grounded on the two synthetic reports:
    let secondary_raw = load_synth_report(common::SYNTH_SECONDARY_SLUG);
    let primary_raw = load_synth_report(common::SYNTH_PRIMARY_SLUG);

    let secondary_sit = secondary_raw["modules"]["fengshui"]["data"]["orientation"]
        ["sittingMountain"]["palace"]
        .as_str()
        .unwrap();
    let secondary_face = secondary_raw["modules"]["fengshui"]["data"]["orientation"]
        ["facingMountain"]["palace"]
        .as_str()
        .unwrap();
    let bz_secondary = ba_zhai(secondary_sit, Some(secondary_face));
    assert!(bz_secondary["group"].as_str().unwrap_or("").contains("West"));
    assert_eq!(bz_secondary["groupZh"], "西四宅");

    let primary_mag = primary_raw["modules"]["fengshui"]["data"]["orientation"]["facingMag"]
        .as_f64()
        .unwrap();
    let primary_period = primary_raw["modules"]["fengshui"]["data"]["period"]["natalPeriod"]
        .as_u64()
        .unwrap() as u8;
    let mtn_primary = mountain_of(primary_mag);
    assert!(!mtn_primary.zh.is_empty());
    let mtn_boundary = mountain_of(37.4);
    assert_eq!(mtn_boundary.zh, "丑");
    assert!(
        mtn_boundary.void_line.is_some(),
        "37.4° lies near Chou/Gen void boundary"
    );
    let chart_primary = flying_star_chart(primary_period, primary_mag);
    let ann2026 = annual_chart(2026);
    let chart_json = chart_to_json(&chart_primary, 9, &ann2026);
    assert!(chart_json.is_object());
    assert_eq!(period_range(9), [2024, 2043]);

    // 3. Deterministic AI analyst on synthetic primary report emits Simplified Chinese fengShui.zh and empty fengShui.en
    let fb_primary = build_deterministic_analyst_fallback(&primary_raw);
    assert_eq!(fb_primary["fengShui"]["en"], "");
    let fs_zh = fb_primary["fengShui"]["zh"].as_str().expect("fengShui.zh");
    assert!(
        fs_zh.contains('坐') && fs_zh.contains('朝'),
        "expected Chinese sitting/facing orientation in fengShui.zh, got: {fs_zh}"
    );
    assert!(fb_primary["translations"]["zh"]["headline"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));
    assert!(fb_primary["translations"]["es"]["headline"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));

    // Verify `sanitize_report_for_html` blanks legacy `ai.fengShui.en` while preserving Chinese `ai.fengShui.zh`.
    let mut legacy_en_rep = primary_raw.clone();
    legacy_en_rep["ai"]["fengShui"]["en"] =
        json!("Culturally, the home is read with a legacy English site note.");
    let sanitized_primary = sanitize_report_for_html(&legacy_en_rep);
    assert_eq!(sanitized_primary["ai"]["fengShui"]["en"], "");
    assert!(sanitized_primary["ai"]["fengShui"]["zh"]
        .as_str()
        .unwrap_or("")
        .contains('坐'));

    // 4. Enrich synthetic primary report with all deterministic backend modules and verify HTML contracts
    let mut enriched_primary = enrich_synth_report_for_ui(common::SYNTH_PRIMARY_SLUG);
    assert_eq!(
        sanitize_report_for_html(&enriched_primary)["ai"]["fengShui"]["en"],
        ""
    );
    assert_eq!(enriched_primary["translations"]["zh"]["native"], true);
    assert_eq!(enriched_primary["translations"]["es"]["native"], true);
    assert!(enriched_primary["translations"]["zh"]["data"]["verdict"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));
    assert!(enriched_primary["translations"]["es"]["data"]["verdict"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));
    let sum_tr = build_summary_translations(&enriched_primary);
    assert!(sum_tr["zh"]["verdict"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));
    assert!(sum_tr["es"]["verdict"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));

    // Also attach a legacy `sectionNotes.fengshui` English note to verify `renderFengShui()` never renders it
    if let Some(sn) = enriched_primary
        .pointer_mut("/ai/sectionNotes")
        .and_then(Value::as_object_mut)
    {
        sn.insert(
            "fengshui".into(),
            json!("LEGACY_SECTION_NOTE_FENGSHUI_ENGLISH"),
        );
    }

    // 5. Template inspection: nav & #fengshui block contain strictly Chinese Feng Shui UI
    let tpl = include_str!("../assets/template.html");
    assert!(tpl.contains("<a href=\"#fengshui\">堪舆风水</a>"));
    assert!(!tpl.contains("data-en=\"Feng shui\""));
    let fs_start = tpl
        .find("function renderFengShui()")
        .expect("renderFengShui");
    let fs_end = tpl[fs_start..]
        .find("function renderInspection()")
        .expect("renderInspection")
        + fs_start;
    let fs_slice = &tpl[fs_start..fs_end];
    for forbidden_fs in [
        "AI.fengShui.en",
        "Classical site reading",
        "Eight Mansions",
        "Personal Kua",
        "Four Pillars",
        "North up",
        "South up",
        "Form school",
        "Annual flying stars",
        "East group",
        "West group",
    ] {
        assert!(
            !fs_slice.contains(forbidden_fs),
            "renderFengShui() still contains English label `{forbidden_fs}`"
        );
    }
    assert!(fs_slice.contains("堪舆综合解读"));
    assert!(fs_slice.contains("罗盘二十四山立极图"));
    assert!(fs_slice.contains("玄空飞星盘"));
    assert!(fs_slice.contains("八宅明镜"));
    assert!(fs_slice.contains("四柱五行命理与宅气生克"));

    enriched_primary["ai"]["fengShui"]["en"] =
        json!("Culturally, the home is read with a legacy English site note.");
    let rendered = render_report_html(&enriched_primary);
    assert_no_leaks(&rendered, "rendered synthetic primary HTML");
    assert!(rendered.contains("Equal Housing Opportunity.</p>"));
    assert!(rendered.contains("Open Estate"));
    assert!(rendered.contains("window.i18next.init"));
    assert!(
        !rendered.contains("Culturally, the home is read with a legacy English site note."),
        "sanitized HTML must never embed legacy English fengShui.en"
    );
    for marker in [
        "section id=\"diligence\"",
        "section id=\"cre\"",
        "hoaReview",
        "packetReview",
        "repairAddendum",
        "dilRefreshBtn",
        "condRunBtn",
        "creRunBtn",
    ] {
        assert!(rendered.contains(marker), "missing UI marker `{marker}`");
    }
    check_inline_scripts_with_node(&rendered, "primary_full_report");
}

#[tokio::test]
async fn test_3_router_i18n_deal_room_and_playwright_chromium_lang_switching() {
    let temp_dir = std::env::temp_dir().join(format!("openestate-i18n-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("test.sqlite");
    let env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_DB_PATH".into(),
            db_path.to_string_lossy().into_owned(),
        ),
        ("SQLITE_PATH".into(), db_path.to_string_lossy().into_owned()),
    ]));
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    let http = HttpClient::from_env(&env, None);
    let state = AppState::with_components(
        env.clone(),
        http,
        db.clone(),
        HttpDefenseShield::default(),
        PathBuf::from(&temp_dir),
        None,
    );

    let mut primary = enrich_synth_report_for_ui(common::SYNTH_PRIMARY_SLUG);
    if let Some(sn) = primary
        .pointer_mut("/ai/sectionNotes")
        .and_then(Value::as_object_mut)
    {
        sn.insert(
            "fengshui".into(),
            json!("LEGACY_SECTION_NOTE_FENGSHUI_ENGLISH"),
        );
    }
    let secondary = enrich_synth_report_for_ui(common::SYNTH_SECONDARY_SLUG);

    db.put_cached_report(common::SYNTH_PRIMARY_SLUG, &primary, None)
        .await
        .unwrap();
    db.put_cached_report(common::SYNTH_SECONDARY_SLUG, &secondary, None)
        .await
        .unwrap();

    let app = build_router(state);

    // 1. GET /vendor/i18next.min.js
    let r_js = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vendor/i18next.min.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_js.status(), StatusCode::OK);
    let js_ct = r_js
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(js_ct.contains("javascript"));

    // 2. GET /api/i18n/zh-CN.json & GET /api/i18n/es.json
    let r_i18n = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/i18n/zh-CN.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_i18n.status(), StatusCode::OK);
    let b_i18n = axum::body::to_bytes(r_i18n.into_body(), 1_000_000)
        .await
        .unwrap();
    let j_i18n: Value = serde_json::from_slice(&b_i18n).unwrap();
    assert_eq!(j_i18n["nav"]["fengshui"], "堪舆风水");

    let r_i18n_es = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/i18n/es.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_i18n_es.status(), StatusCode::OK);
    let b_i18n_es = axum::body::to_bytes(r_i18n_es.into_body(), 1_000_000)
        .await
        .unwrap();
    let j_i18n_es: Value = serde_json::from_slice(&b_i18n_es).unwrap();
    assert_eq!(j_i18n_es["nav"]["fengshui"], "堪舆风水");

    // 3. GET / with Accept-Language: zh-CN & es -> renders Simplified Chinese & Spanish SSR
    let r_home = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_home.status(), StatusCode::OK);
    let b_home = String::from_utf8_lossy(
        &axum::body::to_bytes(r_home.into_body(), 2_000_000)
            .await
            .unwrap(),
    )
    .to_string();
    assert!(b_home.contains("lang=\"zh-CN\""));
    assert!(b_home.contains("美国房产全维度智能尽职调查"));
    assert_no_leaks(&b_home, "GET / zh-CN");

    let r_home_es = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::ACCEPT_LANGUAGE, "es-ES,es;q=0.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_home_es.status(), StatusCode::OK);
    let b_home_es = String::from_utf8_lossy(
        &axum::body::to_bytes(r_home_es.into_body(), 2_000_000)
            .await
            .unwrap(),
    )
    .to_string();
    assert!(b_home_es.contains("lang=\"es\""));
    assert_no_leaks(&b_home_es, "GET / es");

    // 4. GET /deal-room?slugs=... comparing both synthetic reports in EN and ES
    let deal_uri = format!(
        "/deal-room?slugs={},{}",
        common::SYNTH_PRIMARY_SLUG,
        common::SYNTH_SECONDARY_SLUG
    );
    let r_dr = app
        .clone()
        .oneshot(Request::builder().uri(&deal_uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r_dr.status(), StatusCode::OK);
    let b_dr = String::from_utf8_lossy(
        &axum::body::to_bytes(r_dr.into_body(), 2_000_000)
            .await
            .unwrap(),
    )
    .to_string();
    assert!(b_dr.contains("Open <b style=\"color:var(--brand)\">Estate</b>"));
    assert!(b_dr.contains("堪舆风水 · 坐向元运"));
    assert!(!b_dr.contains("Faces S"));
    assert!(!b_dr.contains("Faces E"));
    assert_no_leaks(&b_dr, "GET /deal-room");
    check_inline_scripts_with_node(&b_dr, "deal_room");

    let deal_es_uri = format!("{deal_uri}&lang=es");
    let r_dr_es = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&deal_es_uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_dr_es.status(), StatusCode::OK);
    let b_dr_es = String::from_utf8_lossy(
        &axum::body::to_bytes(r_dr_es.into_body(), 2_000_000)
            .await
            .unwrap(),
    )
    .to_string();
    assert!(b_dr_es.contains("lang=\"es\""));
    assert!(b_dr_es.contains("堪舆风水 · 坐向元运"));
    assert!(!b_dr_es.contains("Faces S"));
    assert_no_leaks(&b_dr_es, "GET /deal-room es");
    check_inline_scripts_with_node(&b_dr_es, "deal_room_es");

    // 5. POST /api/translate short-circuits from native embedded translations with zero external API calls
    for (req_lang, exp_code) in [("es", "es"), ("zh-CN", "zh")] {
        let r_tr = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/translate")
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "slug": common::SYNTH_PRIMARY_SLUG,
                            "lang": req_lang
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_tr.status(), StatusCode::OK);
        let tr_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(r_tr.into_body(), 1_000_000)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tr_json["ok"], true);
        assert_eq!(tr_json["lang"], exp_code);
        assert_eq!(tr_json["cached"], true);
        assert_eq!(tr_json["record"]["native"], true);
        assert!(tr_json["data"]["verdict"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
    }

    // 6. Real Playwright Headless Chromium browser verification across `light` and `dark` color schemes
    if let Some(pw_mod) = discover_playwright_module() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });

        let pw_script = format!(
            r#"
const {{ chromium }} = require({pw_mod_lit});
const BASE = 'http://{addr}';

function assert(cond, msg) {{
  if (!cond) {{
    console.error('PLAYWRIGHT_I18N_FAIL:', msg);
    process.exit(2);
  }}
}}

(async () => {{
  const browser = await chromium.launch({{ headless: true }});
  try {{
    for (const scheme of ['light', 'dark']) {{
      const ctx = await browser.newContext({{
        colorScheme: scheme,
        viewport: {{ width: 1360, height: 900 }},
      }});
      const page = await ctx.newPage();
      const pageErrors = [];
      page.on('pageerror', (err) => pageErrors.push(String(err)));
      page.on('console', (msg) => {{
        if (msg.type() === 'error') pageErrors.push('console.error: ' + msg.text());
      }});
      const translateCalls = [];
      page.on('request', (req) => {{
        const u = req.url();
        if (u.includes('/api/translate')) translateCalls.push(u);
      }});
      // Block external CDN map tiles so headless page settles instantaneously without internet dependency
      await page.route('https://**/*', (route) => route.abort());

      const navResp = await page.goto(`${{BASE}}/r/{primary_slug}/?lang=en`, {{
        waitUntil: 'domcontentloaded',
      }});
      try {{
        await page.waitForFunction(() => {{
          const fs = document.getElementById('fengshui');
          const dil = document.getElementById('diligence');
          return fs && fs.innerHTML.includes('堪舆风水') && dil && dil.innerHTML.length > 200;
        }}, null, {{ timeout: 15000 }});
      }} catch (waitErr) {{
        const diag = await page.evaluate(() => ({{
          title: document.title,
          fsLen: document.getElementById('fengshui')?.innerHTML.length || 0,
          fsHead: (document.getElementById('fengshui')?.innerHTML || '').slice(0, 200),
          dilLen: document.getElementById('diligence')?.innerHTML.length || 0,
          dilHead: (document.getElementById('diligence')?.innerHTML || '').slice(0, 200),
        }})).catch((e) => ({{ evalErr: String(e) }}));
        console.error('PLAYWRIGHT_WAIT_DIAG:', JSON.stringify({{ scheme, status: navResp?.status(), pageErrors, diag }}));
        throw waitErr;
      }}

      const snapEn = await page.evaluate(() => {{
        const htmlOf = (id) => document.getElementById(id)?.innerHTML || '';
        return {{
          lang: document.documentElement.lang,
          prefersDark: window.matchMedia('(prefers-color-scheme: dark)').matches,
          payment: htmlOf('payment'),
          diligence: htmlOf('diligence'),
          more: htmlOf('more'),
          cre: htmlOf('cre'),
          fengshui: htmlOf('fengshui'),
          ai: htmlOf('ai'),
        }};
      }});

      assert(snapEn.prefersDark === (scheme === 'dark'), `expected colorScheme=${{scheme}}`);
      for (const [sec, html] of Object.entries({{
        payment: snapEn.payment,
        diligence: snapEn.diligence,
        more: snapEn.more,
        cre: snapEn.cre,
        fengshui: snapEn.fengshui,
        ai: snapEn.ai,
      }})) {{
        assert(html.length > 50, `${{scheme}} ${{sec}} rendered empty`);
        assert(!html.includes('[object Object]'), `${{scheme}} ${{sec}} leaked [object Object]`);
        assert(!html.includes('undefined'), `${{scheme}} ${{sec}} leaked undefined`);
        assert(!html.includes('NaN'), `${{scheme}} ${{sec}} leaked NaN`);
      }}

      assert(
        snapEn.diligence.includes('id="dilPaneCash"') && snapEn.diligence.includes('$') && !snapEn.diligence.includes('—/mo'),
        `${{scheme}} dilPaneCash missing dollar PITI`
      );
      assert(
        !snapEn.diligence.includes('permits · true') && !snapEn.diligence.includes('permits · false'),
        `${{scheme}} diligence leaked raw boolean`
      );
      assert(
        snapEn.diligence.includes('[Day 1') && snapEn.diligence.includes('Re-Key'),
        `${{scheme}} dilPanePost calendar missing [Day 1 Re-Key`
      );
      assert(
        snapEn.more.includes('Roof') && snapEn.more.includes('yrs'),
        `${{scheme}} #more missing Roof lifecycle table`
      );
      assert(
        snapEn.fengshui.includes('堪舆风水') &&
        snapEn.fengshui.includes('罗盘二十四山立极图') &&
        snapEn.fengshui.includes('玄空飞星盘') &&
        snapEn.fengshui.includes('八宅明镜'),
        `${{scheme}} #fengshui missing core Chinese sections`
      );
      assert(
        !snapEn.fengshui.includes('LEGACY_SECTION_NOTE_FENGSHUI_ENGLISH') &&
        !snapEn.fengshui.includes('legacy English site note'),
        `${{scheme}} #fengshui leaked English text when lang=en`
      );

      // Switch languages live via window.setLang('zh') -> 'es' -> 'en'
      const afterZh = await page.evaluate(async () => {{
        await window.setLang('zh');
        return {{
          lang: document.documentElement.lang,
          fengshui: document.getElementById('fengshui')?.innerHTML || '',
          ai: document.getElementById('ai')?.innerHTML || '',
          payment: document.getElementById('payment')?.innerHTML || '',
          diligence: document.getElementById('diligence')?.innerHTML || '',
        }};
      }});
      assert(afterZh.lang === 'zh-CN', `${{scheme}} expected lang=zh-CN after setLang('zh'), got ${{afterZh.lang}}`);
      assert(!afterZh.ai.includes('undefined') && !afterZh.ai.includes('[object Object]'), `${{scheme}} zh ai invalid`);
      assert(!afterZh.payment.includes('undefined') && !afterZh.diligence.includes('undefined'), `${{scheme}} zh payment/diligence invalid`);
      assert(
        afterZh.fengshui.includes('堪舆风水') && !afterZh.fengshui.includes('LEGACY_SECTION_NOTE_FENGSHUI_ENGLISH'),
        `${{scheme}} zh #fengshui invariant failed`
      );

      const afterEs = await page.evaluate(async () => {{
        await window.setLang('es');
        return {{
          lang: document.documentElement.lang,
          fengshui: document.getElementById('fengshui')?.innerHTML || '',
          ai: document.getElementById('ai')?.innerHTML || '',
          payment: document.getElementById('payment')?.innerHTML || '',
          diligence: document.getElementById('diligence')?.innerHTML || '',
        }};
      }});
      assert(afterEs.lang === 'es', `${{scheme}} expected lang=es after setLang('es'), got ${{afterEs.lang}}`);
      assert(!afterEs.ai.includes('undefined') && !afterEs.ai.includes('[object Object]'), `${{scheme}} es ai invalid`);
      assert(!afterEs.payment.includes('undefined') && !afterEs.diligence.includes('undefined'), `${{scheme}} es payment/diligence invalid`);
      assert(
        afterEs.fengshui.includes('堪舆风水') && !afterEs.fengshui.includes('LEGACY_SECTION_NOTE_FENGSHUI_ENGLISH'),
        `${{scheme}} es #fengshui invariant failed`
      );

      const afterEn = await page.evaluate(async () => {{
        await window.setLang('en');
        return document.documentElement.lang;
      }});
      assert(afterEn === 'en', `${{scheme}} expected lang=en after setLang('en'), got ${{afterEn}}`);
      assert(
        translateCalls.length === 0,
        `${{scheme}} embedded native translations must switch synchronously without /api/translate, got: ${{translateCalls.join(',')}}`
      );

      await ctx.close();
    }}
  }} finally {{
    await browser.close();
  }}
}})().catch((err) => {{
  console.error(err);
  process.exit(3);
}});
"#,
            pw_mod_lit = serde_json::to_string(&pw_mod).unwrap(),
            primary_slug = common::SYNTH_PRIMARY_SLUG,
        );

        let script_path = temp_dir.join("pw_i18n_check.js");
        std::fs::write(&script_path, &pw_script).unwrap();
        let pw_out = tokio::task::spawn_blocking(move || {
            std::process::Command::new("node")
                .arg(&script_path)
                .output()
                .expect("spawn node playwright i18n check")
        })
        .await
        .unwrap();
        server_handle.abort();

        assert!(
            pw_out.status.success(),
            "Playwright Headless Chromium i18n/fengshui UI verification failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&pw_out.stdout),
            String::from_utf8_lossy(&pw_out.stderr),
        );
    }

    let _ = std::fs::remove_dir_all(&temp_dir);
}

fn strip_script_and_style_blocks(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt..];
        let lower_head: String = after
            .chars()
            .take(12)
            .collect::<String>()
            .to_ascii_lowercase();
        if lower_head.starts_with("<script") {
            if let Some(end_idx) = after.to_ascii_lowercase().find("</script>") {
                rest = &after[end_idx + "</script>".len()..];
                continue;
            }
        } else if lower_head.starts_with("<style") {
            if let Some(end_idx) = after.to_ascii_lowercase().find("</style>") {
                rest = &after[end_idx + "</style>".len()..];
                continue;
            }
        }
        if let Some(gt) = after.find('>') {
            out.push(' ');
            rest = &after[gt + 1..];
        } else {
            break;
        }
    }
    out.push_str(rest);
    out
}

#[tokio::test]
async fn test_4_home_area_onsale_showcase_and_no_ops_or_portal_leaks() {
    let temp_dir = std::env::temp_dir().join(format!("openestate-home-showcase-{}", Uuid::new_v4()));
    let reports_dir = temp_dir.join("reports");
    std::fs::create_dir_all(&reports_dir).unwrap();
    let db_path = temp_dir.join("test.sqlite");
    let env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_DB_PATH".into(),
            db_path.to_string_lossy().into_owned(),
        ),
        ("SQLITE_PATH".into(), db_path.to_string_lossy().into_owned()),
    ]));
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    let http = HttpClient::from_env(&env, None);
    let state = AppState::with_components(
        env.clone(),
        http,
        db.clone(),
        HttpDefenseShield::default(),
        reports_dir.clone(),
        None,
    );
    let app = build_router(state);

    let primary_href = format!("/r/{}/", common::SYNTH_PRIMARY_SLUG);
    let secondary_href = format!("/r/{}/", common::SYNTH_SECONDARY_SLUG);

    // Case 1: Empty server -> renders area onsale discovery + report anatomy card, zero sample bars
    let r_empty = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r_empty.status(), StatusCode::OK);
    let html_empty = String::from_utf8_lossy(
        &axum::body::to_bytes(r_empty.into_body(), 2_000_000)
            .await
            .unwrap(),
    )
    .to_string();
    assert!(!html_empty.contains("id=\"sampleReportsBar\""));
    assert!(!html_empty.contains("id=\"samplePreviewCard\""));
    assert!(!html_empty.contains("sample-pill"));
    assert!(html_empty.contains("id=\"areaOnsaleSection\""));
    assert!(html_empty.contains("id=\"reportAnatomyCard\""));
    assert!(html_empty.contains("id=\"buyerQuestionsCard\""));
    assert!(
        !html_empty.contains("homeOpsLink"),
        "public footer must not include homeOpsLink"
    );
    assert!(
        !html_empty.contains("Platform Ops"),
        "public footer must not include Platform Ops"
    );
    assert!(
        !html_empty.contains("href=\"/ops\""),
        "home page must not link to /ops"
    );
    let form_pos = html_empty
        .find("id=\"homeForm\"")
        .expect("homeForm present");
    let showcase_pos = html_empty
        .find("id=\"homeShowcase\"")
        .expect("homeShowcase present");
    assert!(
        form_pos < showcase_pos,
        "search box must remain ahead of homeShowcase"
    );
    assert_no_leaks(&html_empty, "GET / (empty reports)");
    check_inline_scripts_with_node(&html_empty, "home_empty");

    // Case 2: Even when cached reports exist on disk/DB, home page never exposes sample report links
    let primary_rep = common::synth_report_primary();
    let secondary_rep = common::synth_report_secondary();
    for (slug, rep) in [
        (common::SYNTH_PRIMARY_SLUG, &primary_rep),
        (common::SYNTH_SECONDARY_SLUG, &secondary_rep),
    ] {
        let dir = reports_dir.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.json"), serde_json::to_string(rep).unwrap()).unwrap();
        db.put_cached_report(slug, rep, None).await.unwrap();
    }

    let banned_portals = [
        concat!("zil", "low"),
        concat!("zes", "timate"),
        "zhvi",
        "zori",
        concat!("red", "fin"),
        "walk score",
        "walkscore",
        concat!("real", "tor.com"),
        concat!("ho", "mes.com"),
        concat!("tru", "lia"),
        concat!("mov", "oto"),
        "compass",
        concat!("open", "door"),
        "greatschools",
        "niche",
        "attom",
        "simplyrets",
        "homelens",
    ];

    for (uri, label, expected_vocab) in [
        ("/", "home_en", "True Estimate"),
        ("/?lang=zh", "home_zh", "真实估值"),
        ("/?lang=es", "home_es", "Estimación Real"),
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_html = String::from_utf8_lossy(
            &axum::body::to_bytes(resp.into_body(), 2_000_000)
                .await
                .unwrap(),
        )
        .to_string();
        assert!(!body_html.contains(&primary_href));
        assert!(!body_html.contains(&secondary_href));
        assert!(!body_html.contains("id=\"sampleReportsBar\""));
        assert!(!body_html.contains("id=\"samplePreviewCard\""));
        assert!(body_html.contains("id=\"areaOnsaleSection\""));
        assert!(body_html.contains("id=\"reportAnatomyCard\""));
        assert!(
            body_html.contains(expected_vocab),
            "{uri} must use shared vocabulary '{expected_vocab}'"
        );
        assert!(!body_html.contains("homeOpsLink"));
        assert!(!body_html.contains("Platform Ops"));
        assert_no_leaks(&body_html, label);
        check_inline_scripts_with_node(&body_html, label);

        let visible_text = strip_script_and_style_blocks(&body_html).to_ascii_lowercase();
        for portal in banned_portals {
            assert!(
                !visible_text.contains(portal),
                "Home visible HTML at {uri} leaked banned brand/trademark '{portal}'"
            );
        }
    }

    let rendered_es = render_home_html_for_lang(true, "sqlite", "sqlx", false, &[], "es");
    assert!(
        rendered_es.contains("expViewMap: '🗺 Mapa'"),
        "ES map toggle should say Mapa"
    );
    assert!(
        !strip_script_and_style_blocks(&rendered_es).contains("堪舆风水"),
        "Spanish home page must not display Chinese 堪舆风水 in visible copy"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}
