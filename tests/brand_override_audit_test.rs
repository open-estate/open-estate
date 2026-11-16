//! White-label `BRAND_NAME` / `BRAND_DOMAIN` full-stack override audit & regression guardrails.
//!
//! Verifies two strict invariants without hardcoding any downstream product brand:
//! 1. Standalone Open Estate container mode (`BRAND_NAME` / `BRAND_DOMAIN` unset) continues to
//!    default to `"Open Estate"` and `"open-estate.local"` across `<title>`, i18n bundles, emails,
//!    reports, and ops/admin views.
//! 2. Custom white-label mode (`BRAND_NAME=AcmeHomes`, `BRAND_DOMAIN=acmehomes.example`)
//!    overrides EVERY page `<title>`, i18next bundle (`/api/i18n/:lang` and embedded HTML JSON),
//!    header/footer, `/ops`, `/admin`, `/alerts`, `/deal-room`, `/r/:slug`, `/r/:slug/report.json`,
//!    unsubscribe confirmation, email alerts, AI deterministic fallbacks, scoring factors, and
//!    guardrail error responses across EN / 中文 / ES with zero `"Open Estate"` or
//!    `"open-estate.local"` leaks.

mod common;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use open_estate::ai::analyst::run_ai_analyst_with_refresh;
use open_estate::ai::slides::build_deterministic_walkthrough_slides;
use open_estate::db::DbStore;
use open_estate::guardrails::{validate_slug_param, HttpDefenseShield};
use open_estate::modules::engagement::build_engagement_with_brand;
use open_estate::property::house_profile::compute_home_quality;
use open_estate::scoring::factors::HomeFactors;
use open_estate::search::alerts::{build_match_alert_email_with_brand, AlertMatchedItem};
use open_estate::search::CreateSubscriptionRequest;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tower::ServiceExt;
use uuid::Uuid;

const CUSTOM_BRAND: &str = "AcmeHomes";
const CUSTOM_DOMAIN: &str = "acmehomes.example";
const ADMIN_TOKEN: &str = "test-admin-tok-brand-audit";

async fn build_test_app(extra_env: &[(&str, &str)]) -> (axum::Router, AppState, DbStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!("oe-brand-audit-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("brand_audit.sqlite");
    let mut map = HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
    ]);
    for (k, v) in extra_env {
        map.insert((*k).to_string(), (*v).to_string());
    }
    let env = EnvConfig::from_map(map);
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    let state = AppState::with_components(
        env,
        HttpClient::offline_loopback_only(None),
        db.clone(),
        HttpDefenseShield::default(),
        dir.clone(),
        None,
    );
    let app = build_router(state.clone());
    (app, state, db, dir)
}

async fn http_get(
    app: &axum::Router,
    uri: &str,
    auth_bearer: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(uri);
    if let Some(tok) = auth_bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 16_000_000)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn extract_html_title(html: &str) -> String {
    let re = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| html_escape::decode_html_entities(m.as_str()).into_owned())
        .unwrap_or_default()
}

fn strip_scripts_and_styles(html: &str) -> String {
    let script_re = Regex::new(r"(?is)<script\b[^>]*>.*?</script>").unwrap();
    let style_re = Regex::new(r"(?is)<style\b[^>]*>.*?</style>").unwrap();
    let comment_re = Regex::new(r"(?is)<!--.*?-->").unwrap();
    let no_script = script_re.replace_all(html, " ");
    let no_style = style_re.replace_all(&no_script, " ");
    comment_re.replace_all(&no_style, " ").into_owned()
}

fn strip_js_comments_and_internal_ids(script_src: &str) -> String {
    let block_cmt = Regex::new(r"/\*[\s\S]*?\*/").unwrap();
    let line_cmt = Regex::new(r"(?m)(?:^|\s)//[^\n]*").unwrap();
    let cleaned = block_cmt.replace_all(script_src, " ");
    let cleaned = line_cmt.replace_all(&cleaned, " ");
    let id_re = Regex::new(
        r"(?x)
        __OPEN_ESTATE_[A-Z0-9_]+
        | OpenEstate(?:Core|House3D|Motion|Workspace|CommandCenter|WalkinMedia|ProfileStore|Drawer|Fab|Chat)
        | (?:open|close|toggle|save|load)OpenEstate[A-Za-z0-9_]*
        | aiOpenEstateLauncher
        | openOpenEstateFromAiBtn
        | openestate:[a-z0-9_:-]+
        | open_estate_[a-z0-9_]+
        | x-openestate-[a-z0-9-]+
        | X-OpenEstate-[A-Za-z0-9-]+
        ",
    )
    .unwrap();
    id_re.replace_all(&cleaned, " ").into_owned()
}

fn collect_leaked_strings_in_json(val: &Value, path: &str, leaks: &mut Vec<String>) {
    match val {
        Value::String(s) => {
            if path.ends_with(".id")
                || path.ends_with(".key")
                || path.ends_with(".slug")
                || path.ends_with(".rawSlug")
                || path.ends_with(".visualKind")
                || path.ends_with(".targetSection")
                || path.ends_with(".thumbUrl")
                || path.ends_with(".icon")
            {
                return;
            }
            if (path.ends_with(".url") || path.ends_with(".sourceUrl") || path.ends_with(".href"))
                && !s.contains("open-estate.local")
            {
                return;
            }
            if s.contains("Open Estate") || s.contains("open-estate.local") {
                leaks.push(format!("{path}: {s:?}"));
            }
        }
        Value::Array(arr) => {
            for (idx, item) in arr.iter().enumerate() {
                collect_leaked_strings_in_json(item, &format!("{path}[{idx}]"), leaks);
            }
        }
        Value::Object(map) => {
            for (k, child) in map {
                collect_leaked_strings_in_json(child, &format!("{path}.{k}"), leaks);
            }
        }
        _ => {}
    }
}

fn extract_i18n_bundles_from_html(html: &str, marker: &str) -> Option<HashMap<String, Value>> {
    let pos = html.find(marker)?;
    let block = &html[pos..];
    let block_end = block.find("</script>").unwrap_or(block.len());
    let slice = &block[..block_end];
    let mut out = HashMap::new();
    for (lang_key, patterns) in [
        ("en", ["en: { translation:", "en: {translation:"].as_slice()),
        (
            "zh-CN",
            [
                "\"zh-CN\": { translation:",
                "'zh-CN': { translation:",
                "'zh-CN': {translation:",
            ]
            .as_slice(),
        ),
        ("es", ["es: { translation:", "es: {translation:"].as_slice()),
    ] {
        let found = patterns.iter().find_map(|pat| {
            slice.find(pat).map(|idx| &slice[idx + pat.len()..])
        })?;
        let trimmed = found.trim_start();
        let mut de = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
        let parsed = de.next()?.ok()?;
        out.insert(lang_key.to_string(), parsed);
    }
    Some(out)
}

fn extract_script_tag_json(html: &str, script_id: &str) -> Option<Value> {
    let open = format!("id=\"{script_id}\"");
    let pos = html.find(&open)?;
    let after_open = &html[pos..];
    let gt = after_open.find('>')? + 1;
    let body = &after_open[gt..];
    let close = body.find("</script>")?;
    serde_json::from_str::<Value>(body[..close].trim()).ok()
}

fn seed_report_with_default_brand(slug: &str) -> Value {
    let mut rep = common::synth_report_for_slug(slug);
    if let Some(obj) = rep.as_object_mut() {
        obj.insert(
            "brand".to_string(),
            json!({ "name": "Open Estate", "domain": "open-estate.local" }),
        );
        obj.insert(
            "engagement".to_string(),
            build_engagement_with_brand(
                slug,
                common::SYNTH_PRIMARY_ADDRESS,
                &json!({}),
                None,
                &[],
                None,
                3,
                "Open Estate",
            ),
        );
        let slides = build_deterministic_walkthrough_slides(&json!({
            "meta": obj.get("meta").cloned().unwrap_or(json!({})),
            "facts": obj.get("facts").cloned().unwrap_or(json!({})),
            "modules": obj.get("modules").cloned().unwrap_or(json!({})),
            "summary": obj.get("summary").cloned().unwrap_or(json!({})),
        }));
        obj.insert("walkthroughSlides".to_string(), slides);
    }
    rep
}

#[tokio::test]
async fn standalone_default_mode_preserves_open_estate_branding() {
    let (app, _state, db, dir) = build_test_app(&[]).await;
    let slug = common::SYNTH_PRIMARY_SLUG;
    let seed = seed_report_with_default_brand(slug);
    db.put_cached_report(slug, &seed, None).await.unwrap();

    for path in [
        "/",
        "/alerts",
        "/deal-room",
        "/ops",
        &format!("/r/{slug}"),
    ] {
        let (status, html) = http_get(&app, path, None).await;
        assert_eq!(status, StatusCode::OK, "{path} should return 200");
        let title = extract_html_title(&html);
        assert!(
            title.contains("Open Estate"),
            "standalone default mode {path} <title> should contain 'Open Estate', got {title:?}"
        );
        assert!(
            !title.contains(CUSTOM_BRAND),
            "standalone default mode {path} <title> must not contain {CUSTOM_BRAND:?}"
        );
    }

    for lang_code in ["en", "zh-CN", "es"] {
        let (status, body) = http_get(&app, &format!("/api/i18n/{lang_code}"), None).await;
        assert_eq!(status, StatusCode::OK);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["brand"]["name"], "Open Estate");
        assert_eq!(json["brand"]["domain"], "open-estate.local");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn custom_brand_override_leaves_zero_open_estate_leaks_across_routes_and_langs() {
    let (app, _state, db, dir) = build_test_app(&[
        ("BRAND_NAME", CUSTOM_BRAND),
        ("BRAND_DOMAIN", CUSTOM_DOMAIN),
        ("ADMIN_API_TOKEN", ADMIN_TOKEN),
    ])
    .await;

    let slug = common::SYNTH_PRIMARY_SLUG;
    let seed = seed_report_with_default_brand(slug);
    db.put_cached_report(slug, &seed, None).await.unwrap();

    // 1. Standalone HTML routes with no embedded third-party fallback literals in `<script>`
    let strict_script_paths = [
        "/?lang=en".to_string(),
        "/?lang=zh-CN".to_string(),
        "/?lang=es".to_string(),
        "/shop?lang=en".to_string(),
        "/shop?lang=zh-CN".to_string(),
        "/shop?lang=es".to_string(),
        "/alerts?lang=en".to_string(),
        "/alerts?lang=zh-CN".to_string(),
        "/alerts?lang=es".to_string(),
        "/deal-room?lang=en".to_string(),
        "/deal-room?lang=zh-CN".to_string(),
        "/deal-room?lang=es".to_string(),
        format!("/deal-room?slugs={slug}&lang=en"),
        "/ops".to_string(),
        "/api/subscriptions/unsubscribe?token=missing&lang=en".to_string(),
        "/api/subscriptions/unsubscribe?token=missing&lang=zh-CN".to_string(),
        "/api/subscriptions/unsubscribe?token=missing&lang=es".to_string(),
    ];

    for path in &strict_script_paths {
        let (status, html) = http_get(&app, path, None).await;
        let expected_status = if path.starts_with("/api/subscriptions/unsubscribe?token=missing") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        assert_eq!(status, expected_status, "{path} should return {expected_status}");

        let title = extract_html_title(&html);
        assert!(
            title.contains(CUSTOM_BRAND),
            "{path} <title> must contain {CUSTOM_BRAND:?}, got {title:?}"
        );
        assert!(
            !title.contains("Open Estate") && !title.contains("open-estate.local"),
            "{path} <title> leaked default brand: {title:?}"
        );

        let visible_html = strip_scripts_and_styles(&html);
        assert!(
            !visible_html.contains("Open Estate") && !visible_html.contains("open-estate.local"),
            "{path} visible HTML outside <script> leaked default brand"
        );

        let cleaned_all = strip_js_comments_and_internal_ids(&html);
        assert!(
            !cleaned_all.contains("Open Estate") && !cleaned_all.contains("open-estate.local"),
            "{path} leaked default brand in HTML/script body"
        );

        if let Some(i18n_map) =
            extract_i18n_bundles_from_html(&html, "window.__OPEN_ESTATE_I18N_RESOURCES__")
        {
            for (lang_key, bundle) in &i18n_map {
                let mut leaks = Vec::new();
                collect_leaked_strings_in_json(
                    bundle,
                    &format!("i18nResources.{lang_key}"),
                    &mut leaks,
                );
                assert!(
                    leaks.is_empty(),
                    "{path} embedded __OPEN_ESTATE_I18N_RESOURCES__[{lang_key}] leaked default brand: {leaks:?}"
                );
                assert_eq!(bundle["brand"]["name"], CUSTOM_BRAND);
                assert_eq!(bundle["brand"]["domain"], CUSTOM_DOMAIN);
            }
        }
        if let Some(res_map) = extract_i18n_bundles_from_html(&html, "const resMap =") {
            for (lang_key, bundle) in &res_map {
                let mut leaks = Vec::new();
                collect_leaked_strings_in_json(bundle, &format!("dealRoomRes.{lang_key}"), &mut leaks);
                assert!(
                    leaks.is_empty(),
                    "{path} deal-room resMap[{lang_key}] leaked default brand: {leaks:?}"
                );
                assert_eq!(bundle["brand"]["name"], CUSTOM_BRAND);
            }
        }
    }

    // 2. `/admin` login gate HTML + authenticated Admin Console HTML
    let gate_html = open_estate::web::admin_console::render_admin_login_gate_html(
        &open_estate::web::ops_dashboard::AdminAuthError {
            status: StatusCode::UNAUTHORIZED,
            code: "ADMIN_AUTH_REQUIRED",
            message: "Administrator token required.",
            token_configured: true,
        },
        CUSTOM_BRAND,
    );
    assert!(extract_html_title(&gate_html).contains(CUSTOM_BRAND));
    assert!(!strip_js_comments_and_internal_ids(&gate_html).contains("Open Estate"));

    let (adm_unauth_status, adm_unauth_html) = http_get(&app, "/admin", None).await;
    assert!(adm_unauth_status == StatusCode::UNAUTHORIZED || adm_unauth_status == StatusCode::OK);
    assert!(extract_html_title(&adm_unauth_html).contains(CUSTOM_BRAND));
    assert!(!strip_js_comments_and_internal_ids(&adm_unauth_html).contains("Open Estate"));

    let (adm_status, adm_html) = http_get(&app, "/admin", Some(ADMIN_TOKEN)).await;
    assert_eq!(adm_status, StatusCode::OK);
    assert!(extract_html_title(&adm_html).contains(CUSTOM_BRAND));
    let adm_cleaned = strip_js_comments_and_internal_ids(&adm_html);
    assert!(
        !adm_cleaned.contains("Open Estate") && !adm_cleaned.contains("open-estate.local"),
        "/admin console leaked default brand"
    );

    // 3. `/r/<slug>` report page across EN / 中文 / ES
    for lang in ["en", "zh-CN", "es"] {
        let path = format!("/r/{slug}?lang={lang}");
        let (status, html) = http_get(&app, &path, None).await;
        assert_eq!(status, StatusCode::OK, "{path} should return 200");

        let title = extract_html_title(&html);
        assert!(
            title.contains(CUSTOM_BRAND) && !title.contains("Open Estate"),
            "{path} <title> bad: {title:?}"
        );

        let visible_html = strip_scripts_and_styles(&html);
        assert!(
            !visible_html.contains("Open Estate") && !visible_html.contains("open-estate.local"),
            "{path} visible HTML outside <script> leaked default brand"
        );

        let report_json = extract_script_tag_json(&html, "report-data")
            .expect("rendered report HTML must include <script id=\"report-data\"> JSON");
        assert_eq!(report_json["brand"]["name"], CUSTOM_BRAND);
        assert_eq!(report_json["brand"]["domain"], CUSTOM_DOMAIN);
        let mut r_leaks = Vec::new();
        collect_leaked_strings_in_json(&report_json, "reportData", &mut r_leaks);
        assert!(
            r_leaks.is_empty(),
            "{path} <script id=\"report-data\"> leaked default brand: {r_leaks:?}"
        );

        let i18n_res = extract_i18n_bundles_from_html(&html, "window.__OPEN_ESTATE_I18N_RES__")
            .expect("rendered report HTML must include window.__OPEN_ESTATE_I18N_RES__");
        for (lang_key, bundle) in &i18n_res {
            let mut i18n_leaks = Vec::new();
            collect_leaked_strings_in_json(
                bundle,
                &format!("reportI18n.{lang_key}"),
                &mut i18n_leaks,
            );
            assert!(
                i18n_leaks.is_empty(),
                "{path} __OPEN_ESTATE_I18N_RES__[{lang_key}] leaked default brand: {i18n_leaks:?}"
            );
            assert_eq!(bundle["brand"]["name"], CUSTOM_BRAND);
            assert_eq!(bundle["brand"]["domain"], CUSTOM_DOMAIN);
        }
        assert_eq!(
            i18n_res["en"]["dealRoom"]["exportFilename"],
            "acmehomes-deal-room-comparison.csv"
        );

        // Template HTML inline code outside the standalone static JS library fallbacks
        let template_prefix = html
            .split("window.__OPEN_ESTATE_I18N_RES__")
            .nth(1)
            .unwrap_or("");
        assert!(
            template_prefix.contains(&format!("\"{CUSTOM_BRAND}\""))
                && template_prefix.contains(&format!("\"{CUSTOM_DOMAIN}\"")),
            "{path} template must inject custom brand JSON"
        );
    }

    // 4. `/api/i18n/:lang` JSON endpoints across EN / zh-CN / es
    for lang_code in ["en", "zh-CN", "es"] {
        let (status, body) = http_get(&app, &format!("/api/i18n/{lang_code}"), None).await;
        assert_eq!(status, StatusCode::OK);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["brand"]["name"], CUSTOM_BRAND);
        assert_eq!(json["brand"]["domain"], CUSTOM_DOMAIN);
        let mut leaks = Vec::new();
        collect_leaked_strings_in_json(&json, &format!("api.i18n.{lang_code}"), &mut leaks);
        assert!(leaks.is_empty(), "/api/i18n/{lang_code} leaked: {leaks:?}");
    }

    // 5. `/r/:slug/report.json` API localization (even when DB cached old default brand)
    let (rjson_status, rjson_body) =
        http_get(&app, &format!("/r/{slug}/report.json"), None).await;
    assert_eq!(rjson_status, StatusCode::OK);
    let rjson: Value = serde_json::from_str(&rjson_body).unwrap();
    assert_eq!(rjson["brand"]["name"], CUSTOM_BRAND);
    assert_eq!(rjson["brand"]["domain"], CUSTOM_DOMAIN);
    assert_eq!(rjson["engagement"]["platforms"][0]["name"], CUSTOM_BRAND);
    let mut rj_leaks = Vec::new();
    collect_leaked_strings_in_json(&rjson, "reportJson", &mut rj_leaks);
    assert!(rj_leaks.is_empty(), "/report.json leaked: {rj_leaks:?}");

    // 6. `/api/ops/telemetry` connector metadata must reflect custom brand
    let (ops_api_status, ops_api_body) = http_get(&app, "/api/ops/telemetry", None).await;
    assert_eq!(ops_api_status, StatusCode::OK);
    let ops_json: Value = serde_json::from_str(&ops_api_body).unwrap();
    let mut ops_leaks = Vec::new();
    collect_leaked_strings_in_json(&ops_json, "opsTelemetry", &mut ops_leaks);
    assert!(ops_leaks.is_empty(), "/api/ops/telemetry leaked: {ops_leaks:?}");

    // 7. Scraper guardrail & direct LLM block endpoints must never leak default brand
    let scraper_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/report/enqueue")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "curl/8.4.0")
                .header(header::ORIGIN, "http://127.0.0.1")
                .header(header::HOST, "127.0.0.1")
                .body(Body::from(
                    json!({ "address": common::SYNTH_PRIMARY_ADDRESS }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let scraper_bytes = axum::body::to_bytes(scraper_resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let scraper_txt = String::from_utf8_lossy(&scraper_bytes);
    assert!(
        !scraper_txt.contains("Open Estate"),
        "guardrail error leaked default brand: {scraper_txt}"
    );
    let slug_err = validate_slug_param("bad slug!").unwrap_err();
    assert!(!slug_err.message.contains("Open Estate"));

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn branded_emails_ai_fallbacks_scoring_and_static_js_contracts_hold() {
    let branded_env = EnvConfig::from_map(HashMap::from([
        ("BRAND_NAME".to_string(), CUSTOM_BRAND.to_string()),
        ("BRAND_DOMAIN".to_string(), CUSTOM_DOMAIN.to_string()),
    ]));
    assert_eq!(branded_env.brand_name(), CUSTOM_BRAND);
    assert_eq!(branded_env.brand_domain(), CUSTOM_DOMAIN);

    // 1. Alert digest emails across EN / 中文 / ES must override intro, subject, footer & links
    let sample_match = AlertMatchedItem {
        event_type: "new_listing".to_string(),
        old_price: None,
        event_key: "evt-1".to_string(),
        listing: json!({
            "id": "listing-synth-1",
            "address": common::SYNTH_PRIMARY_ADDRESS,
            "price": 1_550_000,
            "beds": 4,
            "baths": 2.5,
            "sqft": 2100,
            "reportSlug": common::SYNTH_PRIMARY_SLUG,
        }),
        match_score: 91,
        match_tier: "Strong Match".to_string(),
        why_it_matches: "Matches bedroom and school criteria.".to_string(),
        tradeoffs: None,
    };
    for lang_code in ["en-US", "zh-CN", "es"] {
        let email = build_match_alert_email_with_brand(
            Some("Alex"),
            "4bd home in 95118",
            &["95118".to_string()],
            &[sample_match.clone()],
            "",
            "tok-unsub-brand",
            lang_code,
            CUSTOM_BRAND,
            CUSTOM_DOMAIN,
        );
        assert!(
            email.subject.contains(CUSTOM_BRAND),
            "{lang_code} email subject missing {CUSTOM_BRAND}: {}",
            email.subject
        );
        assert!(
            email.text.contains(CUSTOM_BRAND) && email.text.contains(CUSTOM_DOMAIN),
            "{lang_code} email text missing custom brand/domain: {}",
            email.text
        );
        assert!(
            !email.subject.contains("Open Estate")
                && !email.text.contains("Open Estate")
                && !email.html.contains("Open Estate")
                && !email.text.contains("open-estate.local")
                && !email.html.contains("open-estate.local"),
            "{lang_code} email leaked default brand"
        );
    }

    // 2. CreateSubscriptionRequest defaults base_origin cleanly without hardcoding external host
    let parsed_sub: CreateSubscriptionRequest = serde_json::from_value(json!({
        "email": format!("buyer@{CUSTOM_DOMAIN}"),
        "zips": ["95118"],
        "requirements": "4bd single family"
    }))
    .unwrap();
    assert!(
        parsed_sub.base_origin.is_empty(),
        "default_origin must be empty so renderer uses env.brand_domain()"
    );

    // 3. Offline AI analyst fallback, walkthrough slides, HomeFactors, and HomeQuality respect report brand
    let mut branded_report = common::synth_report_primary();
    branded_report["meta"]["brand"] = branded_env.brand_json();
    branded_report["brand"] = branded_env.brand_json();

    let http = HttpClient::offline_loopback_only(None);
    let ai_out = run_ai_analyst_with_refresh(&http, &branded_env, &branded_report, None, false)
        .await
        .unwrap();
    let ai_src = ai_out.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(" ");
    assert!(ai_src.contains(CUSTOM_BRAND) && !ai_src.contains("Open Estate"));
    let mut ai_leaks = Vec::new();
    collect_leaked_strings_in_json(&ai_out.data, "aiFallback", &mut ai_leaks);
    assert!(ai_leaks.is_empty(), "AI fallback leaked default brand: {ai_leaks:?}");

    let slides = build_deterministic_walkthrough_slides(&branded_report);
    let mut slide_leaks = Vec::new();
    collect_leaked_strings_in_json(&slides, "walkthroughSlides", &mut slide_leaks);
    assert!(
        slide_leaks.is_empty(),
        "walkthrough slides leaked default brand: {slide_leaks:?}"
    );

    let factors = HomeFactors::from_report(&branded_report);
    let factors_json = factors.to_report_json();
    let mut f_leaks = Vec::new();
    collect_leaked_strings_in_json(&factors_json, "factors", &mut f_leaks);
    assert!(f_leaks.is_empty(), "HomeFactors leaked default brand: {f_leaks:?}");

    let hq = compute_home_quality(&branded_report);
    assert!(
        hq.source_notes
            .iter()
            .any(|n| n.contains(CUSTOM_BRAND))
            && hq.source_notes.iter().all(|n| !n.contains("Open Estate")),
        "compute_home_quality source_notes leaked default brand: {:?}",
        hq.source_notes
    );

    // 4. Static asset guardrail: `template.html` must have zero raw `'Open Estate'` outside comments,
    //    and static `assets/*.js` fallbacks must always consult `window.__OPEN_ESTATE_BRAND__` first.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tpl = fs::read_to_string(root.join("assets/template.html")).unwrap();
    let tpl_no_comments = Regex::new(r"/\*[\s\S]*?\*/")
        .unwrap()
        .replace_all(&tpl, " ")
        .into_owned();
    assert!(
        !tpl_no_comments.contains("Open Estate") && !tpl_no_comments.contains("open-estate.local"),
        "assets/template.html must use brandInfo() / __REPORT_BRAND_* placeholders instead of hardcoded literals"
    );

    for rel in [
        "assets/report_motion.js",
        "assets/report_workspace.js",
        "assets/house3d.js",
    ] {
        let src = fs::read_to_string(root.join(rel)).unwrap();
        let src_no_comments = Regex::new(r"/\*[\s\S]*?\*/")
            .unwrap()
            .replace_all(&src, " ")
            .into_owned();
        for (idx, line) in src_no_comments.lines().enumerate() {
            if line.contains("Open Estate") || line.contains("open-estate.local") {
                let window_start = idx.saturating_sub(3);
                let context_block: String = src_no_comments
                    .lines()
                    .skip(window_start)
                    .take(5)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    context_block.contains("__OPEN_ESTATE_BRAND__"),
                    "{rel}:{} references default brand without reading window.__OPEN_ESTATE_BRAND__: {line}",
                    idx + 1
                );
            }
        }
    }
}
