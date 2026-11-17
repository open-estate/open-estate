//! Regression tests verifying `minijinja` HTML auto-escaping (`AutoEscape::Html` via `.html`
//! templates and explicit `set_auto_escape_callback`), pre-compiled `Lazy<Environment<'static>>`
//! reuse, `| safe` / `Value::from_safe_string` integrity, and `AutoEscape::None` preservation
//! on non-HTML URL/query templates.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use open_estate::db::DbStore;
use open_estate::extensions::yaml_runner::render_query_template;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::search::alerts::{
    build_manage_link_email_with_brand, build_match_alert_email_with_brand,
    build_welcome_confirm_email_with_brand, AlertMatchedItem,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::alerts_ui::render_alerts_manager_html;
use open_estate::web::server::{build_router, AppState};
use open_estate::web::ui::{
    render_home_html_with_options, render_job_html_with_brand,
    render_unsub_html_with_token_and_brand,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

const XSS_PAYLOAD: &str = "\"><script>alert(1)</script>&'";
const XSS_ESCAPED: &str = "&quot;&gt;&lt;script&gt;alert(1)&lt;&#x2f;script&gt;&amp;&#x27;";
const SCRIPT_BREAKOUT_PAYLOAD: &str = "100 SynthAlpha Ln</script><script>alert(9)</script>";

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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
            "oe-minijinja-js-chk-{label}-{idx}-{}.js",
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

#[test]
fn ui_job_html_escapes_xss_in_address_and_step_while_keeping_inline_json_valid() {
    let html = render_job_html_with_brand(
        "job-xss-1",
        XSS_PAYLOAD,
        "slug-xss",
        "raw-slug-xss",
        "<b>step_error</b>",
        "en-US",
        XSS_PAYLOAD,
    );

    assert!(
        !html.contains("<script>alert(1)</script>"),
        "raw script payload must never appear in job.html"
    );
    assert!(
        !html.contains("<b>step_error</b>"),
        "raw HTML step payload must be escaped in job.html"
    );
    assert!(
        html.contains(XSS_ESCAPED),
        "expected HTML-escaped XSS payload in job.html"
    );
    assert!(
        html.contains("&lt;b&gt;step_error&lt;&#x2f;b&gt;"),
        "expected HTML-escaped step payload in job.html"
    );

    // Trusted HTML & inline script JSON slots must remain intact and uncorrupted.
    assert!(html.contains(r#"<link rel="icon" type="image/svg+xml""#));
    assert!(html.contains(r#"const id="job-xss-1",slug="slug-xss",rawSlug="raw-slug-xss",addr=""#));
    assert!(
        !html.contains("addr=\"&quot;"),
        "addr_json inside <script> must not be HTML-entity-escaped"
    );
    check_inline_scripts_with_node(&html, "job_xss");

    // Closing </script> inside address must be escaped via safe_inline_json (\u003c) so it cannot break out.
    let breakout_html = render_job_html_with_brand(
        "job-xss-2",
        SCRIPT_BREAKOUT_PAYLOAD,
        "slug-xss-2",
        "raw-slug-xss-2",
        "geocoding",
        "en-US",
        "Open Estate",
    );
    assert!(
        !breakout_html.contains("</script><script>alert(9)</script>"),
        "</script> breakout must be neutralized in both HTML and inline JS"
    );
    assert!(
        breakout_html.contains(r"\u003c/script>\u003cscript>alert(9)\u003c/script>"),
        "inline JSON must escape '<' as \\u003c inside <script>"
    );
    check_inline_scripts_with_node(&breakout_html, "job_breakout");
}

#[test]
fn ui_unsub_html_escapes_xss_in_email_and_zips_while_preserving_safe_slots() {
    let zips = vec![format!("95118{XSS_PAYLOAD}")];
    let html = render_unsub_html_with_token_and_brand(
        true,
        XSS_PAYLOAD,
        &zips,
        "en-US",
        XSS_PAYLOAD,
        Some("tok_test&1\"<>'"),
    );

    assert!(
        !html.contains("<script>alert(1)</script>"),
        "raw script payload must never appear in unsub.html"
    );
    assert!(
        html.contains(XSS_ESCAPED),
        "expected HTML-escaped email/zip payload in unsub.html body"
    );
    assert!(html.contains(r#"<link rel="icon" type="image/svg+xml""#));
    assert!(
        html.contains(r#"<a class="mbtn" href="/alerts?token=tok_test%261%22%3C%3E%27">"#),
        "manage_url href must preserve slashes and URL-encode token safely without &#x2f; corruption"
    );
}

#[test]
fn ui_home_and_alerts_html_escape_untrusted_brand_and_preserve_safe_scripts_and_json() {
    let home_html = render_home_html_with_options(
        true,
        "sqlite",
        "sqlx",
        true,
        &[],
        "en-US",
        XSS_PAYLOAD,
        "test.example\"><script>alert(2)</script>",
    );

    assert!(
        !home_html.contains("<script>alert(1)</script>"),
        "raw brand_name script must not appear in home.html"
    );
    assert!(
        !home_html.contains("<script>alert(2)</script>"),
        "raw brand_domain script must not appear in home.html"
    );
    assert!(
        home_html.contains(XSS_ESCAPED),
        "brand_name must be HTML-escaped in home.html"
    );
    assert!(
        home_html.contains("&lt;$2.2M"),
        "Fluent copy '<$2.2M' must be cleanly HTML-escaped to '&lt;$2.2M'"
    );
    assert!(home_html.contains(r#"<link rel="icon" type="image/svg+xml""#));
    assert!(home_html.contains(r#"<img class="logo-mark""#));
    assert!(home_html.contains(r#"window.__OPEN_ESTATE_INIT_LANG__ = "en";"#));
    assert!(home_html.contains(r#"window.__OPEN_ESTATE_LANG_COOKIE__ = "open_estate_lang";"#));
    assert!(home_html.contains(r#"window.__OPEN_ESTATE_BRAND__ = {"#));
    assert!(home_html.contains(r"\u003cscript>alert(1)\u003c/script>"));
    assert!(
        !home_html.contains("window.__OPEN_ESTATE_INIT_LANG__ = &quot;"),
        "inline JS JSON slots must not be HTML-escaped"
    );
    check_inline_scripts_with_node(&home_html, "home_xss");

    let alerts_html = render_alerts_manager_html(
        "en-US",
        XSS_PAYLOAD,
        "test.example\"><script>alert(2)</script>",
    );

    assert!(
        !alerts_html.contains("<script>alert(1)</script>"),
        "raw brand_name script must not appear in alerts.html"
    );
    assert!(
        !alerts_html.contains("<script>alert(2)</script>"),
        "raw brand_domain script must not appear in alerts.html"
    );
    assert!(
        alerts_html.contains(XSS_ESCAPED),
        "brand_name must be HTML-escaped in alerts.html"
    );
    assert!(
        alerts_html.contains("Find &amp; Manage Your Saved Alerts"),
        "ampersands in static UI copy must be HTML-escaped"
    );
    assert!(alerts_html.contains(r#"<link rel="icon" type="image/svg+xml""#));
    assert!(alerts_html.contains(r#"<img class="logo-mark""#));
    assert!(alerts_html.contains(r#"window.__OPEN_ESTATE_ALERTS_BOOT__ = {"#));
    assert!(alerts_html.contains(r"\u003cscript>alert(1)\u003c/script>"));
    assert!(
        !alerts_html.contains("window.__OPEN_ESTATE_ALERTS_BOOT__ = {&quot;"),
        "alerts boot JSON must not be HTML-escaped"
    );
    check_inline_scripts_with_node(&alerts_html, "alerts_xss");
}

#[test]
fn alert_email_templates_escape_xss_and_preserve_brand_wordmark_and_urls() {
    let matched = vec![AlertMatchedItem {
        event_type: "new".to_string(),
        old_price: None,
        event_key: "evt-xss-1".to_string(),
        listing: json!({
            "id": "listing-xss-1",
            "address": format!("100 SynthAlpha Ln {XSS_PAYLOAD}"),
            "street": "100 SynthAlpha Ln",
            "status": "for_sale",
            "price": 1_250_000,
            "beds": 4,
            "baths": 3.0,
            "sqft": 2100,
            "photoUrl": "https://photos.example.org/home-1.jpg?w=600&h=400",
            "reportSlug": "100-synthalpha-ln",
        }),
        match_score: 92,
        match_tier: "Strong Match".to_string(),
        why_it_matches: "Matches bedroom and budget".to_string(),
        tradeoffs: None,
    }];

    let match_email = build_match_alert_email_with_brand(
        Some("Alex"),
        &format!("4+ bd quiet street {XSS_PAYLOAD}"),
        &[format!("95118{XSS_PAYLOAD}")],
        &matched,
        "https://openestate.org",
        "tok_match_123",
        "en-US",
        XSS_PAYLOAD,
        "openestate.org",
    );

    assert!(!match_email.html.contains("<script>alert(1)</script>"));
    assert!(match_email.html.contains(XSS_ESCAPED));
    assert!(
        match_email.html.contains(r#"<b style="color:#0f766e">"#),
        "trusted brand_wordmark HTML must remain intact"
    );
    assert!(
        match_email
            .html
            .contains("https://photos.example.org/home-1.jpg?w=600&amp;h=400"),
        "photoUrl must preserve URL slashes while escaping '&' in attribute context"
    );
    assert!(
        match_email
            .html
            .contains("https://openestate.org/alerts?token=tok_match_123"),
        "manage_url must keep URL slashes intact"
    );
    assert!(
        match_email
            .html
            .contains("https://openestate.org/api/subscriptions/unsubscribe?token=tok_match_123"),
        "unsub_url must keep URL slashes intact"
    );

    let welcome_email = build_welcome_confirm_email_with_brand(
        Some("Alex"),
        &[format!("95118{XSS_PAYLOAD}")],
        "daily",
        "https://openestate.org",
        "tok_welcome_123",
        "en-US",
        XSS_PAYLOAD,
        "openestate.org",
    );
    assert!(!welcome_email.html.contains("<script>alert(1)</script>"));
    assert!(welcome_email.html.contains(XSS_ESCAPED));
    assert!(welcome_email.html.contains(r#"<b style="color:#0f766e">"#));
    assert!(welcome_email
        .html
        .contains("https://openestate.org/alerts?token=tok_welcome_123"));

    let manage_email = build_manage_link_email_with_brand(
        "https://openestate.org",
        "tok_manage_123",
        "en-US",
        XSS_PAYLOAD,
        "openestate.org",
    );
    assert!(!manage_email.html.contains("<script>alert(1)</script>"));
    assert!(manage_email.html.contains(XSS_ESCAPED));
    assert!(manage_email.html.contains(r#"<b style="color:#0f766e">"#));
    assert!(manage_email
        .html
        .contains("https://openestate.org/alerts?manage=tok_manage_123"));
}

#[test]
fn yaml_runner_query_template_preserves_un_escaped_url_semantics() {
    let mut ctx = BTreeMap::new();
    ctx.insert("address".to_string(), "100 SynthAlpha Ln & Unit \"A\"".to_string());
    ctx.insert("zip".to_string(), "95118".to_string());
    let rendered = render_query_template(
        "https://api.example.org/v1/search?zip={{ zip }}&addr={{ address }}",
        &ctx,
    )
    .expect("render query template");
    assert_eq!(
        rendered,
        "https://api.example.org/v1/search?zip=95118&addr=100 SynthAlpha Ln & Unit \"A\""
    );
}

#[tokio::test]
async fn axum_routes_escape_xss_on_home_alerts_and_unsubscribe() {
    let dir = std::env::temp_dir().join(format!("oe-minijinja-srv-{}", Uuid::new_v4()));
    let reports_dir = dir.join("reports");
    std::fs::create_dir_all(&reports_dir).unwrap();
    let _guard = TempDirGuard(dir.clone());
    let db_path = dir.join("test.sqlite");
    let env = EnvConfig::default().with_overrides([
        ("SQLITE_DB_PATH", db_path.to_string_lossy().into_owned()),
        ("SQLITE_PATH", db_path.to_string_lossy().into_owned()),
        ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
        ("SYNC_INTERVAL_MIN", "0".to_string()),
        ("BRAND_NAME", XSS_PAYLOAD.to_string()),
        ("BRAND_DOMAIN", "test.example\"><script>alert(2)</script>".to_string()),
    ]);
    let db = DbStore::connect(&env).await.expect("sqlite");
    db.ensure_schema().await.expect("ensure_schema");

    let token = "unsub-xss-tok-12345";
    db.execute_params(
        "INSERT INTO subscriptions (id, email, zips_json, requirements_text, parsed_filters_json, active, unsubscribe_token, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)",
        &[
            "sub-xss-1".into(),
            XSS_PAYLOAD.into(),
            r#"["95118"]"#.into(),
            "4bd".into(),
            "{}".into(),
            token.into(),
            "2026-01-01T00:00:00Z".into(),
            "2026-01-01T00:00:00Z".into(),
        ],
    )
    .await
    .expect("insert xss subscription");

    let http = HttpClient::offline_loopback_only(None);
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(env, http, db, shield, reports_dir, None);
    let app = build_router(state);

    for uri in ["/", "/alerts", "/api/subscriptions/unsubscribe?token=unsub-xss-tok-12345"] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "unexpected status for {uri}");
        let body = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            !body.contains("<script>alert(1)</script>"),
            "route {uri} rendered unescaped <script>alert(1)</script>"
        );
        assert!(
            !body.contains("<script>alert(2)</script>"),
            "route {uri} rendered unescaped <script>alert(2)</script>"
        );
        assert!(
            body.contains(XSS_ESCAPED),
            "route {uri} missing HTML-escaped payload"
        );
    }
}
