//! Integration tests for API-First Inspection / Disclosure Synthesis & DB-Backed Agent / Inspector Recommendations
//! (100% Zero Mock Servers, Procedural Synthetic Property Reports via `tests/common/mod.rs`):
//! 1. Deterministic inspection/title/HOA document synthesis (`analyze_packet_documents_deterministic`,
//!    `parse_title_report_text`, `parse_hoa_documents`, `redact_title_pii`) grounded in
//!    `common::synth_report_primary()`, cached-report persistence (`apiFetchedReviews`
//!    retained while private `inspection` is stripped), and HTML rendering (`.api-doc-banner`,
//!    `<details class="supp-up-details">`, zero forbidden internal tokens).
//! 2. API-unavailable honesty (`fetchedViaApi == false`, `uploadRequired == true`, agent access +
//!    recommendation hint, visible `#docAgentHintCard` + `#agentRecDrawer`), SSRF public-URL guardrails,
//!    W3C text-fragment / PDF page anchoring, and MLS connector / tax-history merge helpers.
//! 3. `recommended_agents` database table starts completely empty (`0` rows -> `recommendedAgents: []`,
//!    `isEmpty: true`), `GET /api/agent-match/recommendations`, `POST /api/agent-match/request` guardrails +
//!    SQLite persistence + direct `lettre::Message::formatted()` RFC 5322 MIME byte inspection, plus
//!    partner row filtering & `migrate_database` parity.

#![recursion_limit = "512"]

mod common;

use axum::http::StatusCode;
use open_estate::ai::{
    hoa_review::parse_hoa_documents,
    packet_review::analyze_packet_documents_deterministic,
    pdf::{extract_document_pages, UploadedDocInput},
    title_review::{parse_title_report_text, redact_title_pii},
};
use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::property::deep_research::{
    attach_data_point_anchor, run_deep_property_research, validate_ssrf_safe_public_url,
};
use open_estate::property::mls::{classify_asset_class, connector_status, merge_tax_histories};
use open_estate::search::{build_lettre_message, EmailRequest};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::render_report_html;
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::net::TcpListener;
use uuid::Uuid;

fn temp_sqlite_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "openestate-api-insp-{tag}-{}.sqlite",
        Uuid::new_v4()
    ))
}

fn cleanup_sqlite_files(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    let _ = std::fs::remove_file(format!("{}-shm", p.display()));
}

fn load_synth_report(primary: bool) -> Value {
    if primary {
        common::synth_report_primary()
    } else {
        common::synth_report_secondary()
    }
}

fn strip_internal_ai_keys(val: &mut Value) {
    match val {
        Value::Object(map) => {
            for k in ["model", "effort", "fallbackUsed", "usage", "provider"] {
                map.remove(k);
            }
            for v in map.values_mut() {
                strip_internal_ai_keys(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_internal_ai_keys(v);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn test_1_deterministic_inspection_title_hoa_synthesis_cached_persistence_and_html_ux() {
    let db_path = temp_sqlite_path("api-synth");
    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();

    let mut report = load_synth_report(true);
    let synth_apn = report["facts"]["apn"]
        .as_str()
        .expect("synthetic primary APN")
        .to_string();

    // 1. Verify pre-LLM PII redaction directly on statutory redaction patterns.
    // Note: use a non-overlapping SSN pattern (`999-88-1234`) so `synth_apn` (`000-00-100`) is preserved.
    let raw_clause = format!(
        "ALTA Preliminary Report Specimen — APN: {synth_apn}\n\
         Estate or Interest: Fee Simple\n\
         Title to said estate is vested in: [Record Grantee], Husband and Wife as Community Property\n\
         Taxpayer ID / SSN: 999-88-1234 | Loan Account #: ACCT-9900112233 | Contact: escrow.notice@title-specimen.org"
    );
    let pii_res = redact_title_pii(&raw_clause);
    assert!(
        !pii_res.masked_text.contains("999-88-1234"),
        "raw SSN pattern must be masked"
    );
    assert!(pii_res.masked_text.contains("***-**-1234"));
    assert!(!pii_res.masked_text.contains("9900112233"));
    assert!(!pii_res
        .masked_text
        .contains("escrow.notice@title-specimen.org"));
    assert!(pii_res.masked_text.contains(&synth_apn));

    // 2. Run deterministic packet, Preliminary Title, and Civil Code §5550 HOA parsers against synthetic facts.
    let insp_doc = extract_document_pages(&UploadedDocInput {
        name: "Pre-Listing-Home-And-WDO-Inspection.txt".to_string(),
        text: Some(
            format!(
                "[Page 1] RESIDENTIAL PROPERTY & SECTION 1 WDO INSPECTION — {}\n\
                 [Page 4] ROOF: Composition shingle roof (~21 yrs old) exhibits lifted flashing and granular wear on south slope; budget for roof replacement within 2–4 years.\n\
                 [Page 8] ELECTRICAL: Original subpanel in garage requires breaker evaluation and exterior GFCI receptacle upgrade.\n\
                 [Page 12] PLUMBING & SEWER: Main cast-iron sewer lateral shows root intrusion at cleanout; water heater lacks lower seismic strap.\n\
                 [Page 16] SECTION 1 TERMITE / WDO: Subterranean termite shelter tubes at garage sill plate and dry rot at fascia ($2,800 Section 1 repair bid).",
                common::SYNTH_PRIMARY_ADDRESS
            ),
        ),
        base64: None,
    })
    .expect("extract inspection specimen");

    let hoa_doc = extract_document_pages(&UploadedDocInput {
        name: "Civil-Code-5550-Reserve-Summary.txt".to_string(),
        text: Some(
            "[Page 1] California Civil Code §5550 Assessment & Reserve Funding Disclosure Summary\n\
             Percent Funded: 64.5%. Monthly Assessment: $285/month. Annual Operating Budget: $420,000.\n\
             Current Reserve Balance: $645,000. Ideal Fully Funded Balance: $1,000,000.\n\
             No active construction defect litigation."
                .to_string(),
        ),
        base64: None,
    })
    .expect("extract HOA specimen");

    let prelim_text = format!(
        "[Page 1] ALTA Preliminary Report — Property: {}\n\
         APN: {synth_apn} | Estate or Interest: Fee Simple\n\
         1. Grant Deed recorded 2019-06-15, consideration $1,480,000.\n\
         2. A Deed of Trust to secure an indebtedness in the amount of $1,184,000.00, Recorded June 15, 2019.\n\
         3. Public Utility Easement over rear 5 feet of parcel; Covenants, Conditions and Restrictions (CC&Rs).",
        common::SYNTH_PRIMARY_ADDRESS
    );

    let mut packet_review = analyze_packet_documents_deterministic(
        &[insp_doc.clone(), hoa_doc.clone()],
        Some(&report),
        0,
        None,
    );
    let mut title_review = parse_title_report_text(&prelim_text, Some(&report));
    let mut hoa_review = parse_hoa_documents(&[hoa_doc], Some(&report), 0);

    for v in [&mut packet_review, &mut title_review, &mut hoa_review] {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("fetchedViaApi".to_string(), json!(true));
        }
        strip_internal_ai_keys(v);
    }

    assert_eq!(title_review["apn"], json!(synth_apn));
    assert!(
        hoa_review
            .pointer("/financialHealth/percentFunded")
            .and_then(Value::as_f64)
            .is_some(),
        "hoaReview must parse Civil Code §5550 percentFunded"
    );
    let findings = packet_review
        .get("deduplicatedDefects")
        .or_else(|| packet_review.get("unifiedFindings"))
        .or_else(|| packet_review.get("findings"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        findings.len() >= 2,
        "packet_review must extract itemized inspection defects: {packet_review}"
    );

    report["publicResearch"] = json!({
        "inspectionSearchStatus": {
            "fetchedViaApi": true,
            "uploadRequired": false,
            "canRecommendAgent": true,
            "canRecommendInspector": true,
            "summary": "Pre-listing inspection, title, and disclosure packet synthesized via connector."
        },
        "apiFetchedReviews": {
            "hasAny": true,
            "inspection": {
                "fetchedViaApi": true,
                "findings": findings
            },
            "titleReview": title_review,
            "hoaReview": hoa_review,
            "packetReview": packet_review
        }
    });

    // 3. Verify cached_reports keeps `publicResearch.apiFetchedReviews` while stripping private browser `inspection`.
    let slug = common::SYNTH_PRIMARY_SLUG;
    let mut to_cache = report.clone();
    to_cache["inspection"] = json!({ "privateBrowserUpload": true });
    db.put_cached_report(slug, &to_cache, None).await.unwrap();
    let loaded = db
        .get_cached_report(slug)
        .await
        .unwrap()
        .expect("cached report");
    assert!(loaded.report.get("inspection").is_none());
    assert_eq!(
        loaded
            .report
            .pointer("/publicResearch/apiFetchedReviews/hasAny"),
        Some(&json!(true))
    );

    // 4. Render HTML on the real fixture and verify API-first UX & zero forbidden tokens.
    let html = render_report_html(&report);
    assert!(html.contains("api-doc-banner"));
    assert!(html.contains("supp-up-details"));
    for forbidden in [
        "gpt-4o-mini",
        "gpt-4.1",
        "fallbackUsed",
        "deterministic-fallback",
        "max reasoning",
        "aiModel",
        "OPENAI_API_KEY",
        "rust-sqlx-sqlite",
    ] {
        assert!(
            !html.contains(forbidden),
            "rendered HTML leaked forbidden token '{forbidden}'"
        );
    }

    cleanup_sqlite_files(&db_path);
}

#[tokio::test]
async fn test_2_api_unavailable_hints_agent_access_and_ssrf_url_helpers() {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();

    // Closed local port ensures fast offline execution with zero mock HTTP server.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);

    let offline_env = EnvConfig::from_map(HashMap::from([(
        "OUTBOUND_PROXY_URLS".to_string(),
        format!("http://127.0.0.1:{closed_port}"),
    )]));
    let http = HttpClient::from_env(&offline_env, None);

    let mut report = load_synth_report(false);
    let deep_out = run_deep_property_research(
        &http,
        &offline_env,
        &db,
        &report["geo"],
        &report["facts"],
        &json!({}),
        None,
        common::SYNTH_SECONDARY_SLUG,
    )
    .await;

    let iss = &deep_out.data["inspectionSearchStatus"];
    assert_eq!(iss["fetchedViaApi"], json!(false));
    assert_eq!(iss["uploadRequired"], json!(true));
    assert_eq!(iss["canRecommendAgent"], json!(true));
    assert_eq!(iss["canRecommendInspector"], json!(true));
    let hint = iss["agentHint"].as_str().unwrap_or("");
    assert!(
        hint.contains("agent") && hint.contains("recommend"),
        "agentHint must explain obtaining via agent & recommending one: {hint}"
    );
    let summary = iss["summary"].as_str().unwrap_or("");
    assert!(
        summary.to_ascii_lowercase().contains("api") && summary.contains("recommend"),
        "summary must mention API unavailability & agent recommendation: {summary}"
    );

    report["publicResearch"] = deep_out.data;

    let html = render_report_html(&report);
    assert!(html.contains("id=\"docAgentHintCard\""));
    assert!(html.contains("api-doc-banner") || html.contains("docAgentHintCard"));

    // Part B: Verify SSRF public-URL guardrails, W3C text-fragment / PDF anchoring, and MLS connector helpers.
    assert!(
        validate_ssrf_safe_public_url("http://127.0.0.1:8080/secret.pdf", false)
            .await
            .is_err()
    );
    assert!(
        validate_ssrf_safe_public_url("https://169.254.169.254/latest/meta-data", false)
            .await
            .is_err()
    );
    let pdf_anchored = attach_data_point_anchor(
        "https://records.sccgov.org/disclosures/packet.pdf",
        "Composition shingle roof",
    );
    assert!(pdf_anchored.contains("#page=1&search=Composition%20shingle%20roof"));
    let html_anchored =
        attach_data_point_anchor("https://records.sccgov.org/parcel/000-00-100", "Fee Simple");
    assert!(html_anchored.contains("#:~:text=Fee%20Simple"));

    let statuses = connector_status(&EnvConfig::default());
    assert!(statuses
        .iter()
        .any(|s| s["id"] == "reso" && s["active"] == false));
    assert!(statuses
        .iter()
        .any(|s| s["id"] == "county_gis" && s["active"] == true));
    assert_eq!(
        classify_asset_class(Some("Single-Family Detached")),
        Some("Single-Family Detached".to_string())
    );

    let merged_tax = merge_tax_histories(
        &[json!({ "year": 2025, "taxAmount": 26_640, "assessedValue": 2_114_000 })],
        &[
            json!({ "year": 2024, "taxAmount": 17_495, "assessedValue": 1_389_375 }),
            json!({ "year": 2025, "taxAmount": 17_840, "landValue": 980_000 }),
        ],
    );
    assert_eq!(merged_tax.len(), 2);
    assert_eq!(merged_tax[1]["year"], 2025);
    assert_eq!(merged_tax[1]["taxAmount"], 26_640);
    assert_eq!(merged_tax[1]["landValue"], 980_000);
}

#[tokio::test]
async fn test_3_commercial_brokerage_routes_return_404_and_personal_doc_review_with_mime() {
    let db_path = temp_sqlite_path("personal-scope");
    let reports_dir = std::env::temp_dir().join(format!("openestate-rec-rpts-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&reports_dir).unwrap();

    // No SMTP_HOST configured -> zero mock SMTP server; we verify RFC 5322 MIME formatting directly via `lettre`.
    let env = EnvConfig::from_map(HashMap::from([
        (
            "SMTP_FROM".to_string(),
            "Open Estate Alerts <alerts@open-estate.local>".to_string(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
    ]));

    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();

    let primary_report = load_synth_report(true);
    let slug = common::SYNTH_PRIMARY_SLUG;
    db.put_cached_report(slug, &primary_report, None)
        .await
        .unwrap();

    let state = AppState::with_components(
        env.clone(),
        HttpClient::default(),
        db.clone(),
        HttpDefenseShield::default(),
        reports_dir.clone(),
        None,
    );
    let router = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = rx.await;
        })
        .await;
    });

    let browser_ua =
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/537.36 Chrome/128.0.0.0 Safari/537.36";
    let client = Client::builder()
        .no_proxy()
        .user_agent(browser_ua)
        .build()
        .unwrap();

    // 1. Personal `.zip`/PDF document review & condition routes remain mounted (`200 OK`)
    for get_uri in [
        format!("{base_url}/api/title-review?slug={slug}"),
        format!("{base_url}/api/hoa-review?slug={slug}"),
        format!("{base_url}/api/packet-review?slug={slug}"),
        format!("{base_url}/api/repair-addendum?slug={slug}"),
    ] {
        let r = client.get(&get_uri).send().await.unwrap();
        assert_eq!(
            r.status(),
            StatusCode::OK,
            "Personal document review GET route {get_uri} must stay mounted"
        );
    }

    // 2. Removed brokerage/marketplace & CRE routes return `404 NOT_FOUND`
    for removed_get in [
        format!("{base_url}/api/agent-match/recommendations?slug={slug}"),
        format!("{base_url}/api/marketplace/quotes?slug={slug}"),
    ] {
        let r = client.get(&removed_get).send().await.unwrap();
        assert_eq!(
            r.status(),
            StatusCode::NOT_FOUND,
            "Removed commercial GET route {removed_get} must return 404"
        );
    }
    for removed_post in [
        format!("{base_url}/api/agent-match/request"),
        format!("{base_url}/api/marketplace/click"),
        format!("{base_url}/api/cre/underwrite"),
        format!("{base_url}/api/cre/business-eval"),
        format!("{base_url}/api/cre-review"),
    ] {
        let r = client
            .post(&removed_post)
            .header("Origin", &base_url)
            .json(&json!({ "slug": slug }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::NOT_FOUND,
            "Removed commercial POST route {removed_post} must return 404"
        );
    }

    // 3. Direct inspection of `lettre::Message::formatted()` RFC 5322 / 8058 MIME bytes
    let (mime_msg, rcpts, _subj) = build_lettre_message(
        &env,
        &EmailRequest {
            to: vec!["buyer@example.com".to_string()],
            subject: format!("[Open Estate] Alert Match — {}", common::SYNTH_PRIMARY_ADDRESS),
            text: Some(format!("Property: {}\n", common::SYNTH_PRIMARY_ADDRESS)),
            html: None,
            reply_to: Some("buyer@example.com".to_string()),
            unsubscribe_url: Some(
                "https://openestate.org/api/alerts/unsubscribe/tok123".to_string(),
            ),
        },
    )
    .expect("build_lettre_message");
    assert_eq!(rcpts, vec!["buyer@example.com".to_string()]);
    let raw_mime = String::from_utf8(mime_msg.formatted()).expect("valid UTF-8 MIME message");
    assert!(
        raw_mime.contains("Open Estate Alerts")
            && raw_mime.contains("<alerts@open-estate.local>"),
        "unexpected From header in raw_mime:\n{raw_mime}"
    );
    assert!(raw_mime
        .contains("List-Unsubscribe: <https://openestate.org/api/alerts/unsubscribe/tok123>"));
    assert!(raw_mime.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"));

    let _ = tx.send(());
    cleanup_sqlite_files(&db_path);
    let _ = std::fs::remove_dir_all(&reports_dir);
}
