//! End-to-end integration tests for the Role-Aware Property Copilot (`/api/copilot/*`, `/api/ask`,
//! `/vendor/buyer_copilot.js`, `/vendor/pdf_studio.js`, `/assistant`, `/r/:slug`, `/alerts`):
//! 1. Entry Flow, Role-Neutral White-Label UI (`<Brand> AI`, zero `"AI Assistant"` / `"Buyer Assistant"`), Frosted Light Modals & PDF Studio:
//!    - Home `/assistant` (`#tabBtnAssistant`, `#buyerCopilotHomeMount`, `#tabBtnAlerts[hidden]`, `#alertsSmtpDisabledBanner`)
//!    - Report HTML (`#buyerCopilotHdrBtn`, frosted light `.bc-modal-backdrop` & `#oePdfStudioBackdrop`, `OpenEstateRole`, `OpenEstatePdfStudio`)
//!    - `/vendor/buyer_copilot.js` & `/vendor/pdf_studio.js` (200 OK)
//!    - `POST /api/copilot/lookup`: `400 INVALID_ADDRESS` vs. off-market `200` `"Not currently listed"` vs. active `onMarket: true` + 3-item `actionMenu` + `activeRole`/`needsRoleSelection`.
//! 2. Unified Role-Aware Q&A (`POST /api/ask`):
//!    - Unset / `"all"` -> `activeRole == "unset"`, `needsRoleSelection == true`, `roleSelectionPrompt`
//!    - `"buyer"` | `"seller"` | `"agent"` -> distinct `activeRole`, `roleSystemPromptId`, `needsRoleSelection == false`, and distinct role-tailored answers & followUps without cache collision.
//! 3. SMTP & Email Monitor Disabled by Default + `mailto:` Prefilled Material Request:
//!    - `EnvConfig::smtp_feature_enabled() == false` & `email_monitor_enabled() == false` by default
//!    - `mailtoRequest` / `materialRequestMailto` on `/api/copilot/lookup`, `/api/copilot/disclosures`, and `/api/agent-match/recommendations`
//!      prefilled with property address, APN, all 6 priority documents, `smtpDisabled: true`, and `uploadFollowUpPrompt`.
//! 4. Function 1 (Comps & Pricing):
//!    - Strict sold comps -> `canExpandSearch: true` -> expanded fallback, plus non-linear follow-ups
//!      (`sold_or_pending`, `similar_active`, `estimate`) reusing the pinned `compSetId` with price range + confidence.
//! 5. Function 2 (Per-User One-Time Analysis, Immediate File Discard, 3 Safe Modes & Registered-User Redacted Sharing):
//!    - Mode 3 (`publicRecordEquivalents.available == true` for any user with 0 uploads)
//!    - Registered user accounts (`POST /api/copilot/auth/register`, `GET /api/copilot/auth/me`)
//!    - Buyer One-Time `.zip` upload -> `fileRetentionPolicy.rawFilesSaved == false`, `rawFilesDistributed == false`,
//!      strict `1..=6` `prioritySections`, `missingDocuments` (`[2, 5]`), cited `redFlags`, `standingDisclaimer`,
//!      `admin_audit_log` via `list_admin_audit_logs`, and `409 ONE_TIME_ANALYSIS_ALREADY_USED` on second attempt
//!    - Mode 2 (Attested Redacted Registered-User Sharing): `401 REGISTERED_USER_REQUIRED`, `404 RECIPIENT_NOT_REGISTERED`,
//!      `400 COLLABORATOR_ATTESTATION_REQUIRED` when `collaboratorAttested != true`, `200` with `collaboratorAttested: true`,
//!      recursive stripping of `quotedExcerpt`/`quote` prose + SSN/email PII redaction + `privityDisclaimer`, and `null` for User C / anonymous
//!    - Mode 1 (Listing-Side Authorized Upload): `400` when `authorizedToDistribute: false`, `200` when authorized,
//!      `admin_audit_log` `"authorized_disclosure_published"`, visible to User C.
//! 6. Function 3 (Guided 6-Group Offer Builder):
//!    - Jurisdiction transfer-tax & title/escrow custom defaults, editable `buyerAgentCommissionPct` (`2.5%` -> `2.0%`),
//!      live `runningSummary`, disclosure red-flag import (`SuggestedDisclosureRedFlags`), and `finalReview.isContract == false`.

#![recursion_limit = "512"]

mod common;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::{write::DeflateEncoder, Compression};
use lopdf::{
    content::{Content, Operation},
    dictionary, Document, Object, Stream, StringFormat,
};
use open_estate::ai::crc32_ieee;
use open_estate::db::{DbStore, TABLE_NAMES};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::search::{ingest_listings, IngestListingsRequest};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{render_report_html, render_report_html_with_brand};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::net::TcpListener;
use uuid::Uuid;

struct CopilotTestServer {
    base_url: String,
    db: DbStore,
    http: HttpClient,
    env: EnvConfig,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl CopilotTestServer {
    async fn start(entries: &[(&str, Value)]) -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-copilot-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");

        let db_path = temp_dir.join("copilot.sqlite");
        let env = EnvConfig::from_map(HashMap::from([
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

        let http = HttpClient::from_env(&env, None);
        let db = DbStore::connect(&env).await.expect("connect test sqlite");
        db.ensure_schema().await.expect("ensure schema");
        assert_eq!(
            TABLE_NAMES.len(),
            44,
            "strict 44-table schema invariant must hold"
        );

        for (slug, report) in entries {
            db.put_cached_report(slug, report, None)
                .await
                .expect("seed cached synthetic report");
        }

        let shield = HttpDefenseShield::default();
        let state = AppState::with_components(
            env.clone(),
            http.clone(),
            db.clone(),
            shield,
            reports_dir,
            None,
        );
        let router = build_router(state);

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
            db,
            http,
            env,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for CopilotTestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn build_pdf_bytes(pages: &[&[&str]]) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let mut kids: Vec<Object> = Vec::with_capacity(pages.len());
    for lines in pages {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 11.into()]),
            Operation::new("TL", vec![16.into()]),
            Operation::new("Td", vec![48.into(), 740.into()]),
        ];
        for (idx, line) in lines.iter().enumerate() {
            if idx > 0 {
                ops.push(Operation::new("T*", vec![]));
            }
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(
                    (*line).as_bytes().to_vec(),
                    StringFormat::Literal,
                )],
            ));
        }
        ops.push(Operation::new("ET", vec![]));

        let raw = Content { operations: ops }.encode().expect("encode PDF");
        let mut stream = Stream::new(dictionary! {}, raw);
        stream.compress().expect("compress PDF stream");
        let content_id = doc.add_object(stream);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        kids.push(page_id.into());
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => pages.len() as i64,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut out = Vec::new();
    doc.save_to(&mut out).expect("save PDF");
    out
}

fn build_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut cd_records: Vec<Vec<u8>> = Vec::with_capacity(entries.len());

    for &(name, raw) in entries {
        let local_offset = buf.len() as u32;
        let crc = crc32_ieee(raw);
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
        enc.write_all(raw).expect("deflate");
        let comp = enc.finish().expect("finish deflate");
        let name_bytes = name.as_bytes();

        buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&8u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&comp);

        let mut cd = Vec::new();
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&8u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&local_offset.to_le_bytes());
        cd.extend_from_slice(name_bytes);
        cd_records.push(cd);
    }

    let cd_offset = buf.len() as u32;
    let mut cd_size = 0u32;
    for cd in &cd_records {
        cd_size += cd.len() as u32;
        buf.extend_from_slice(cd);
    }

    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf
}

fn build_zip_bomb_bytes() -> Vec<u8> {
    // 320 KB of highly-compressible PDF comment padding -> ~350 bytes deflated (>900:1 > 50:1 limit)
    let mut payload = b"%PDF-1.5\n".to_vec();
    let line = b"%000000000000000000000000000000000000000000000000000000000000000\n";
    while payload.len() < 320_000 {
        payload.extend_from_slice(line);
    }
    payload.extend_from_slice(b"%%EOF\n");
    build_zip_bytes(&[("01_Home_Inspection_Bomb.pdf", &payload)])
}

fn build_nested_zip_bytes(sample_pdf: &[u8]) -> Vec<u8> {
    let inner = build_zip_bytes(&[("inner_inspection.pdf", sample_pdf)]);
    build_zip_bytes(&[
        ("nested_packet.zip", &inner),
        ("01_Home_Inspection.pdf", sample_pdf),
    ])
}

fn build_path_traversal_zip_bytes(sample_pdf: &[u8]) -> Vec<u8> {
    build_zip_bytes(&[("../../traversal_escape.pdf", sample_pdf)])
}

fn assert_valid_mailto_payload(mailto: &Value, expected_recipient: &str, expected_apn: &str) {
    assert_eq!(mailto["smtpDisabled"], json!(true));
    assert_eq!(mailto["recipientEmail"], json!(expected_recipient));
    let href = mailto["mailtoHref"].as_str().expect("mailtoHref string");
    assert!(
        href.starts_with(&format!("mailto:{expected_recipient}?subject=")),
        "mailtoHref must start with mailto:{expected_recipient}?subject=, got: {href}"
    );
    assert!(
        href.contains("&body="),
        "mailtoHref must include &body=: {href}"
    );
    let subject = mailto["subject"].as_str().unwrap_or("");
    let body = mailto["body"].as_str().unwrap_or("");
    assert!(
        subject.contains("100 Synthparcelalpha Ln"),
        "mailto subject missing address: {subject}"
    );
    assert!(
        body.contains("100 Synthparcelalpha Ln"),
        "mailto body missing address: {body}"
    );
    assert!(
        body.contains(expected_apn),
        "mailto body missing APN {expected_apn}: {body}"
    );
    for doc_kw in [
        "Home Inspection",
        "Termite",
        "Preliminary Title",
        "Natural Hazard",
        "HOA",
        "Building Permits",
    ] {
        assert!(
            body.contains(doc_kw),
            "mailto body missing priority document '{doc_kw}'"
        );
    }
    let prompt = mailto["uploadFollowUpPrompt"].as_str().unwrap_or("");
    assert!(
        !prompt.trim().is_empty()
            && (prompt.to_ascii_lowercase().contains("upload")
                || prompt.to_ascii_lowercase().contains(".zip")),
        "uploadFollowUpPrompt must instruct user to upload received files: {prompt}"
    );
}

#[tokio::test]
async fn buyer_copilot_full_workflow_gate_comps_disclosures_and_offer_builder() {
    // 0. Ensure zero hardcoded personal Gmail SMTP credentials exist anywhere in src/, assets/, or tests/
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for sub in ["src", "assets", "tests"] {
        let dir = manifest_dir.join(sub);
        let mut stack = vec![dir];
        while let Some(cur) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&cur) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if let Ok(txt) = std::fs::read_to_string(&p) {
                        assert!(
                            !txt.contains(concat!("bwu187", "@gmail.com")),
                            "raw credential leaked in {}",
                            p.display()
                        );
                    }
                }
            }
        }
    }

    // Verify default-disabled SMTP & Email Monitor invariants on fresh EnvConfig
    let default_env = EnvConfig::from_map(HashMap::new());
    assert!(!default_env.smtp_feature_enabled());
    assert!(!default_env.email_monitor_enabled());
    assert!(!default_env.smtp_configured());

    let host_without_flag = EnvConfig::from_map(HashMap::from([
        ("SMTP_HOST".to_string(), "smtp.example.invalid".to_string()),
        ("SMTP_FROM".to_string(), "alerts@open-estate.local".to_string()),
    ]));
    assert!(!host_without_flag.smtp_feature_enabled());
    assert!(!host_without_flag.email_monitor_enabled());

    // Prepare 2 synthetic cached reports:
    // - ACTIVE_SLUG: on-market San Jose SFR (`forSale: true`) with APN, listing agent email, and controlled strict + expanded comps
    // - OFF_MARKET_SLUG: off-market Campbell SFR (`forSale: false`) to test soft "Not currently listed" gate
    let mut active_rep = common::synth_report_primary();
    active_rep["facts"]["asOf"] = json!("2026-11-15");
    active_rep["facts"]["forSale"] = json!(true);
    active_rep["facts"]["listingStatus"] = json!("Active");
    active_rep["facts"]["listPrice"] = json!(1_620_000);
    active_rep["facts"]["sqft"] = json!(2000);
    active_rep["facts"]["yearBuilt"] = json!(1988);
    active_rep["facts"]["propertyType"] = json!("Single Family");
    active_rep["facts"]["apn"] = json!("421-18-022");
    active_rep["facts"]["mlsListing"] = json!({
        "status": "Active",
        "listPrice": 1_620_000,
        "mlsNumber": "ML81999999",
        "agentEmail": "listing.agent@example.com",
        "agentName": "Jane Listing",
        "officeName": "Silicon Valley Realty"
    });
    active_rep["facts"]["comps"] = json!([
        {
            "address": "104 Synthparcelalpha Ln, San Jose, CA 95118",
            "soldPrice": 1_600_000,
            "soldDate": "2026-10-12",
            "sqft": 1980,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1990,
            "propertyType": "Single Family",
            "distanceMi": 0.28
        },
        {
            "address": "118 Synthparcelalpha Ln, San Jose, CA 95118",
            "soldPrice": 1_660_000,
            "soldDate": "2026-09-25",
            "sqft": 2060,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1986,
            "propertyType": "Single Family",
            "distanceMi": 0.55
        },
        {
            "address": "410 Synthridge Way, San Jose, CA 95118",
            "soldPrice": 1_710_000,
            "soldDate": "2026-06-20",
            "sqft": 2240,
            "beds": 4,
            "baths": 3.0,
            "yearBuilt": 1979,
            "propertyType": "Single Family",
            "distanceMi": 1.62
        }
    ]);
    active_rep["facts"]["nearbyListings"] = json!([
        {
            "address": "320 Synthmeadow Dr, San Jose, CA 95118",
            "price": 1_645_000,
            "sqft": 2120,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1991,
            "propertyType": "Single Family",
            "distanceMi": 1.38
        },
        {
            "address": "990 Synthmansion Ct, San Jose, CA 95118",
            "price": 3_450_000,
            "sqft": 4300,
            "beds": 6,
            "baths": 5.0,
            "yearBuilt": 2016,
            "propertyType": "Single Family",
            "distanceMi": 1.15
        }
    ]);
    active_rep["facts"]["nearbyPending"] = json!([
        {
            "address": "142 Synthgrove St, San Jose, CA 95118",
            "price": 1_635_000,
            "sqft": 1990,
            "beds": 4,
            "baths": 2.0,
            "yearBuilt": 1987,
            "propertyType": "Single Family",
            "distanceMi": 0.74
        }
    ]);

    let mut off_market_rep = common::synth_report_secondary();
    off_market_rep["facts"]["forSale"] = json!(false);
    off_market_rep["facts"]["listingStatus"] = json!("Off Market");
    off_market_rep["facts"]["listPrice"] = Value::Null;

    let srv = CopilotTestServer::start(&[
        (common::SYNTH_PRIMARY_SLUG, active_rep.clone()),
        (common::SYNTH_SECONDARY_SLUG, off_market_rep),
    ])
    .await;
    assert!(!srv.env.smtp_feature_enabled());
    assert!(!srv.env.email_monitor_enabled());

    // Seed an active spatial listing into `listings` within 1.5 miles to verify DB spatial merge
    let subj_lat = active_rep["geo"]["lat"].as_f64().unwrap();
    let subj_lon = active_rep["geo"]["lon"].as_f64().unwrap();
    ingest_listings(
        &srv.db,
        &srv.http,
        &srv.env,
        IngestListingsRequest {
            zip: Some("95118".to_string()),
            listings: vec![json!({
                "id": "mls-synth-active-2",
                "address": "515 Synthbrook Cir, San Jose, CA 95118",
                "city": "San Jose",
                "state": "CA",
                "zip": "95118",
                "status": "Active",
                "price": 1_598_000,
                "listPrice": 1_598_000,
                "beds": 4.0,
                "baths": 2.5,
                "sqft": 1940,
                "yearBuilt": 1985,
                "propertyType": "Single Family",
                "lat": subj_lat + 0.016, // ~1.11 mi north
                "lon": subj_lon,
                "source": "licensed_mls_test"
            })],
            trigger_alerts: false,
            ..Default::default()
        },
    )
    .await
    .expect("seed active spatial listing");

    let client = Client::builder()
        .no_proxy()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/128.0.0.0 Safari/537.36")
        .build()
        .unwrap();
    let cid = "oe_test_copilot_session_001";

    // =========================================================================
    // 1. Home, Report & Alerts UI Integration + White-Label Assistant + PDF Studio
    // =========================================================================
    let forbidden_overlay_brand = concat!("deed", "buddy");
    let home_html = client
        .get(format!("{}/assistant", srv.base_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(home_html.contains("id=\"tabBtnAssistant\""));
    assert!(home_html.contains("id=\"buyerCopilotHomeMount\""));
    assert!(home_html.contains("OpenEstateBuyerCopilot"));
    assert!(home_html.contains("OpenEstatePdfStudio"));
    assert!(home_html.contains("oePdfStudioBackdrop"));
    assert!(home_html.contains("Open Estate AI"));
    assert!(home_html.contains("id=\"alertsSmtpDisabledBanner\""));
    assert!(home_html.contains("id=\"subForm\""));
    assert!(
        home_html.contains("id=\"tabBtnAlerts\" data-i18n=\"home.tabAlerts\" hidden"),
        "alerts tab button must remain present in DOM and hidden by default when SMTP is disabled"
    );
    assert!(
        !home_html.contains("Buyer Assistant")
            && !home_html.contains("Buyer AI Assistant")
            && !home_html.contains("AI Assistant")
            && !home_html.contains("Open Estate Assistant")
            && !home_html.to_ascii_lowercase().contains(forbidden_overlay_brand),
        "home HTML must use white-label '<Brand> AI' and never contain 'AI Assistant', 'Buyer Assistant', '<Brand> Assistant', or downstream brand names"
    );

    let alerts_html = client
        .get(format!("{}/alerts", srv.base_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(alerts_html.contains("id=\"alertsSmtpDisabledBanner\""));
    assert!(alerts_html.contains("id=\"quickAlertForm\""));
    assert!(alerts_html.contains("id=\"navAssistant\""));
    assert!(alerts_html.contains("Open Estate AI"));
    assert!(
        !alerts_html.contains("Buyer Assistant")
            && !alerts_html.contains("AI Assistant")
            && !alerts_html.contains("Open Estate Assistant")
            && !alerts_html.to_ascii_lowercase().contains(forbidden_overlay_brand)
    );

    let rendered_report_html = render_report_html(&active_rep);
    assert!(rendered_report_html.contains("id=\"buyerCopilotHdrBtn\""));
    assert!(rendered_report_html.contains("Open Estate AI"));
    assert!(rendered_report_html.contains("rgba(248,250,252,.82)"));
    assert!(rendered_report_html.contains("rgba(248,250,252,.86)"));
    assert!(rendered_report_html.contains("OpenEstatePdfStudio"));
    assert!(rendered_report_html.contains("oePdfStudioBackdrop"));
    assert!(rendered_report_html.contains("OpenEstateRole"));
    assert!(rendered_report_html.contains("id=\"docMailtoDraftBtn\""));
    assert!(rendered_report_html.contains("id=\"docUploadDiscardNotice\""));
    assert!(rendered_report_html.contains("id=\"docOpenPdfStudioBtn\""));
    assert!(
        !rendered_report_html.contains("Buyer Assistant")
            && !rendered_report_html.contains("Buyer AI Assistant")
            && !rendered_report_html.contains("AI Assistant")
            && !rendered_report_html.contains("Open Estate Assistant")
            && !rendered_report_html.to_ascii_lowercase().contains(forbidden_overlay_brand),
        "report HTML must use '<Brand> AI' and never contain 'AI Assistant', 'Buyer Assistant', or hardcoded downstream brand"
    );

    // Also verify white-label substitution with a synthetic custom brand
    let mut custom_brand_rep = active_rep.clone();
    custom_brand_rep["brand"] = serde_json::json!({
        "name": "Summit Realty",
        "domain": "summit-realty.local"
    });
    let custom_report_html =
        render_report_html_with_brand(&custom_brand_rep, "Summit Realty", "summit-realty.local");
    assert!(
        custom_report_html.contains("Summit Realty AI")
            && !custom_report_html.contains("AI Assistant")
            && !custom_report_html.contains("Summit Realty Assistant"),
        "white-label report HTML must parameterize '<Brand> AI' dynamically"
    );

    let live_report_resp = client
        .get(format!("{}/r/{}", srv.base_url, common::SYNTH_PRIMARY_SLUG))
        .send()
        .await
        .unwrap();
    assert_eq!(live_report_resp.status(), 200);
    let live_report_html = live_report_resp.text().await.unwrap();
    assert!(live_report_html.contains("OpenEstatePdfStudio"));
    assert!(live_report_html.contains("id=\"buyerCopilotHdrBtn\""));
    assert!(live_report_html.contains("Open Estate AI"));
    assert!(!live_report_html.contains("AI Assistant"));

    let vendor_copilot = client
        .get(format!("{}/vendor/buyer_copilot.js", srv.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(vendor_copilot.status(), 200);
    let vendor_copilot_js = vendor_copilot.text().await.unwrap();
    assert!(vendor_copilot_js.contains("OpenEstateBuyerCopilot"));
    assert!(vendor_copilot_js.contains("brandName() + ' AI'"));
    assert!(!vendor_copilot_js.contains("Buyer Assistant"));
    assert!(!vendor_copilot_js.contains("AI Assistant"));
    assert!(!vendor_copilot_js.to_ascii_lowercase().contains(forbidden_overlay_brand));

    let vendor_studio = client
        .get(format!("{}/vendor/pdf_studio.js", srv.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(vendor_studio.status(), 200);
    let vendor_studio_js = vendor_studio.text().await.unwrap();
    assert!(vendor_studio_js.contains("OpenEstatePdfStudio"));
    assert!(vendor_studio_js.contains("PDF Studio"));
    assert!(!vendor_studio_js.to_ascii_lowercase().contains(forbidden_overlay_brand));
    assert!(vendor_studio_js.contains("rgba(248,250,252,.86)"));

    for (vendor_path, expected_mime_sub) in [
        ("/vendor/fflate.min.js", "javascript"),
        ("/vendor/pdf.min.mjs", "javascript"),
        ("/vendor/pdf.worker.min.mjs", "javascript"),
        ("/vendor/pdf_viewer.min.css", "text/css"),
    ] {
        let vresp = client
            .get(format!("{}{}", srv.base_url, vendor_path))
            .send()
            .await
            .unwrap();
        assert_eq!(vresp.status(), 200, "GET {vendor_path} must return 200 OK");
        let ctype = vresp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ctype.contains(expected_mime_sub),
            "GET {vendor_path} content-type must contain {expected_mime_sub}, got: {ctype}"
        );
        let cctrl = vresp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            cctrl.contains("max-age=86400"),
            "GET {vendor_path} must set cache-control max-age=86400, got: {cctrl}"
        );
        let vbody = vresp.bytes().await.unwrap();
        assert!(vbody.len() > 500, "GET {vendor_path} must return non-empty asset");
    }

    // 1a. Lookup Entry Gate: Invalid address -> 400 INVALID_ADDRESS
    let bad_resp = client
        .post(format!("{}/api/copilot/lookup", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({ "address": "not_an_address" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_resp.status(), 400);
    let bad_json: Value = bad_resp.json().await.unwrap();
    assert_eq!(bad_json["ok"], false);
    assert_eq!(bad_json["code"], "INVALID_ADDRESS");

    // 1b. Off-market property -> 200 basic info + soft "Not currently listed" notice
    let off_resp = client
        .post(format!("{}/api/copilot/lookup", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({ "slug": common::SYNTH_SECONDARY_SLUG }))
        .send()
        .await
        .unwrap();
    assert_eq!(off_resp.status(), 200);
    let off_json: Value = off_resp.json().await.unwrap();
    assert_eq!(off_json["ok"], true);
    assert_eq!(off_json["property"]["onMarket"], false);
    assert_eq!(off_json["property"]["listingStatus"], "off_market");
    assert!(off_json["property"]["statusNotice"]
        .as_str()
        .unwrap_or("")
        .contains("Not currently listed"));
    assert!(off_json["property"]["basicInfo"]["sqft"].as_i64().unwrap() > 0);
    assert!(off_json["property"]["actionMenu"]
        .as_array()
        .unwrap()
        .is_empty());

    // 1c. Active on-market property (no role -> unset, needsRoleSelection == true) + mailtoRequest
    let act_resp = client
        .post(format!("{}/api/copilot/lookup", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({ "slug": common::SYNTH_PRIMARY_SLUG }))
        .send()
        .await
        .unwrap();
    assert_eq!(act_resp.status(), 200);
    let act_json: Value = act_resp.json().await.unwrap();
    assert_eq!(act_json["activeRole"], "unset");
    assert_eq!(act_json["needsRoleSelection"], true);
    assert_eq!(act_json["property"]["onMarket"], true);
    assert_eq!(act_json["property"]["listingStatus"], "for_sale");
    assert_eq!(
        act_json["property"]["actionMenu"].as_array().unwrap().len(),
        3
    );
    assert_eq!(act_json["publicRecordEquivalents"]["available"], true);
    assert_eq!(act_json["fileRetentionPolicy"]["rawFilesSaved"], false);
    assert_eq!(act_json["fileRetentionPolicy"]["rawFilesDistributed"], false);
    assert_valid_mailto_payload(
        &act_json["property"]["materialRequestMailto"],
        "listing.agent@example.com",
        "421-18-022",
    );

    // 1d. Lookup with explicit role = "seller" -> activeRole == "seller", needsRoleSelection == false
    let seller_lookup_json: Value = client
        .post(format!("{}/api/copilot/lookup", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({ "slug": common::SYNTH_PRIMARY_SLUG, "role": "seller" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(seller_lookup_json["activeRole"], "seller");
    assert_eq!(seller_lookup_json["needsRoleSelection"], false);

    // 1e. Verify removed brokerage /api/agent-match/recommendations returns 404 NOT_FOUND in Personal Edition
    let rec_resp = client
        .get(format!(
            "{}/api/agent-match/recommendations?slug={}",
            srv.base_url,
            common::SYNTH_PRIMARY_SLUG
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(rec_resp.status(), reqwest::StatusCode::NOT_FOUND);

    // =========================================================================
    // 2. Unified Role-Aware Q&A (`POST /api/ask`: unset | buyer | seller | agent)
    // =========================================================================
    let ask_q = "How should I evaluate pricing, disclosures, and closing costs for this property?";
    let ask_unset: Value = client
        .post(format!("{}/api/ask", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "question": ask_q
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ask_unset["ok"], true);
    assert_eq!(ask_unset["activeRole"], "unset");
    assert_eq!(ask_unset["roleSystemPromptId"], "openestate_role_unset_v1");
    assert_eq!(ask_unset["needsRoleSelection"], true);
    assert!(ask_unset["roleSelectionPrompt"]
        .as_str()
        .unwrap_or("")
        .contains("Buyer, Seller, or Agent"));

    let mut role_answers: HashMap<&str, String> = HashMap::new();
    let mut role_followups: HashMap<&str, Vec<String>> = HashMap::new();
    for role in ["buyer", "seller", "agent"] {
        let resp = client
            .post(format!("{}/api/ask", srv.base_url))
            .header("Origin", &srv.base_url)
            .header("X-OpenEstate-Client-Id", cid)
            .json(&json!({
                "slug": common::SYNTH_PRIMARY_SLUG,
                "question": ask_q,
                "role": role
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let j: Value = resp.json().await.unwrap();
        assert_eq!(j["ok"], true);
        assert_eq!(j["activeRole"], role);
        assert_eq!(
            j["roleSystemPromptId"],
            format!("openestate_role_{role}_v1")
        );
        assert_eq!(j["needsRoleSelection"], false);
        assert!(j.get("roleSelectionPrompt").is_none() || j["roleSelectionPrompt"].is_null());
        let ans = j["answer"].as_str().unwrap_or("").to_string();
        assert!(!ans.trim().is_empty());
        role_answers.insert(role, ans);
        let fu: Vec<String> = j["followUps"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        assert!(!fu.is_empty());
        role_followups.insert(role, fu);
    }
    assert_ne!(
        role_answers["buyer"], role_answers["seller"],
        "buyer vs seller role prompts/answers must differ (zero cache collision)"
    );
    assert_ne!(
        role_answers["seller"], role_answers["agent"],
        "seller vs agent role prompts/answers must differ"
    );
    assert_ne!(
        role_followups["buyer"], role_followups["seller"],
        "buyer vs seller followUps must be role-tailored"
    );

    // =========================================================================
    // 3. Function 1: Comps Table First + Non-Linear Follow-Ups Reusing Pinned Comp Set
    // =========================================================================
    let comps_resp = client
        .post(format!("{}/api/copilot/comps", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "mode": "strict"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(comps_resp.status(), 200);
    let comps_json: Value = comps_resp.json().await.unwrap();
    let strict_set = &comps_json["compSet"];
    let pinned_comp_set_id = strict_set["compSetId"]
        .as_str()
        .expect("compSetId")
        .to_string();
    assert_eq!(strict_set["comps"].as_array().unwrap().len(), 2);
    assert_eq!(strict_set["canExpandSearch"], true);

    // Non-linear follow-ups: 3) sold_or_pending -> 2) similar_active -> 1) estimate
    let sp_json: Value = client
        .post(format!("{}/api/copilot/comps", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "followUp": "sold_or_pending",
            "compSetId": pinned_comp_set_id
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sp_json["followUpResult"]["compSetId"], pinned_comp_set_id);
    assert_eq!(
        sp_json["followUpResult"]["recentlySold"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        sp_json["followUpResult"]["pendingListings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let sim_json: Value = client
        .post(format!("{}/api/copilot/comps", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "followUp": "similar_active",
            "compSetId": pinned_comp_set_id
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sim_json["followUpResult"]["compSetId"], pinned_comp_set_id);
    let active_matches = sim_json["followUpResult"]["activeListings"]
        .as_array()
        .unwrap();
    // Both 320 Synthmeadow (2120 sqft) and 515 Synthbrook (1940 sqft) are within +-15% sqft & <= 2 mi;
    // 990 Synthmansion (4300 sqft) is filtered out.
    assert_eq!(active_matches.len(), 2);
    assert!(active_matches
        .iter()
        .all(|m| !m["address"].as_str().unwrap_or("").contains("Synthmansion")));

    let est_json: Value = client
        .post(format!("{}/api/copilot/comps", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "followUp": "estimate",
            "compSetId": pinned_comp_set_id
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(est_json["followUpResult"]["compSetId"], pinned_comp_set_id);
    let low = est_json["followUpResult"]["estimateRangeUsd"]["low"]
        .as_i64()
        .unwrap();
    let mid = est_json["followUpResult"]["estimateRangeUsd"]["mid"]
        .as_i64()
        .unwrap();
    let high = est_json["followUpResult"]["estimateRangeUsd"]["high"]
        .as_i64()
        .unwrap();
    assert!(
        low < mid && mid < high,
        "estimate must be a range, not one number"
    );
    assert_eq!(est_json["followUpResult"]["confidence"]["level"], "Medium");

    // Expand to 6 months / 2 miles -> brings in 3rd comp
    let exp_json: Value = client
        .post(format!("{}/api/copilot/comps", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", cid)
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "mode": "expanded"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exp_json["compSet"]["comps"].as_array().unwrap().len(), 3);

    // =========================================================================
    // 4. Function 2: Mode 3 Public Baseline, Registered Users, 1x Upload + Discard,
    //    Mode 2 Attested Redacted Sharing, & Mode 1 Listing-Side Authorized Upload
    // =========================================================================
    // 4a. Mode 3: Public-Record Equivalents available before any upload
    let anon_get: Value = client
        .get(format!(
            "{}/api/copilot/disclosures?slug={}",
            srv.base_url,
            common::SYNTH_PRIMARY_SLUG
        ))
        .header("X-OpenEstate-Client-Id", "anon_buyer_0")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(anon_get["ok"], true);
    assert_eq!(anon_get["oneTimeCompleted"], false);
    assert_eq!(anon_get["privateUserAnalysis"], Value::Null);
    assert_eq!(anon_get["sharedWithMeAnalysis"], Value::Null);
    assert_eq!(anon_get["sharedDealTeamAnalysis"], Value::Null);
    assert_eq!(anon_get["publicRecordEquivalents"]["available"], true);
    assert_eq!(
        anon_get["publicRecordEquivalents"]["items"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(anon_get["fileRetentionPolicy"]["rawFilesSaved"], false);
    assert_eq!(anon_get["fileRetentionPolicy"]["rawFilesDistributed"], false);
    assert_valid_mailto_payload(
        &anon_get["mailtoRequest"],
        "listing.agent@example.com",
        "421-18-022",
    );

    // 4b. Verify hosted sharing/account routes remain disabled (404) in Open Estate standalone OSS
    for (method, route, payload) in [
        ("POST", "/api/copilot/auth/register", json!({ "email": "alice.buyer@example.com", "role": "buyer" })),
        ("POST", "/api/copilot/auth/verify", json!({ "email": "alice.buyer@example.com", "code": "000000" })),
        ("POST", "/api/copilot/auth/logout", json!({})),
        ("GET", "/api/copilot/auth/me", Value::Null),
        ("POST", "/api/copilot/disclosures/share", json!({ "slug": common::SYNTH_PRIMARY_SLUG, "recipientEmail": "bob.agent@example.com", "collaboratorAttested": true })),
        ("POST", "/api/copilot/disclosures/authorized", json!({ "slug": common::SYNTH_PRIMARY_SLUG, "uploaderRole": "seller", "signerName": "Alex Owner", "authorizedToDistribute": true, "documents": [] })),
    ] {
        let req = if method == "GET" {
            client.get(format!("{}{}", srv.base_url, route))
        } else {
            client
                .post(format!("{}{}", srv.base_url, route))
                .header("Origin", &srv.base_url)
                .json(&payload)
        };
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), 404, "OSS must not expose sharing route {method} {route}");
    }
    let rcpt_view_oss = client
        .get(format!(
            "{}/api/copilot/disclosures?slug={}&recipientView=true",
            srv.base_url, common::SYNTH_PRIMARY_SLUG
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(rcpt_view_oss.status(), 404, "OSS must disable recipientView shared mode");

    // 4b-ii. Server-Side Malicious Archive Hardening (Zip-bomb >50:1, nested .zip, ../ path traversal -> 400)
    let small_valid_pdf = build_pdf_bytes(&[&[
        "HOME INSPECTION REPORT",
        "Property: 100 Synthparcelalpha Ln, San Jose, CA 95118",
    ]]);
    let bomb_b64 = B64.encode(build_zip_bomb_bytes());
    let nested_b64 = B64.encode(build_nested_zip_bytes(&small_valid_pdf));
    let traversal_b64 = B64.encode(build_path_traversal_zip_bytes(&small_valid_pdf));

    for (label, payload_b64, expected_code) in [
        ("zip-bomb >50:1", &bomb_b64, "ZIP_BOMB_DETECTED"),
        ("nested .zip", &nested_b64, "NESTED_ARCHIVE_PROHIBITED"),
        ("../ path traversal", &traversal_b64, "UNSAFE_ARCHIVE_ENTRY"),
    ] {
        let mal_resp = client
            .post(format!("{}/api/copilot/disclosures", srv.base_url))
            .header("Origin", &srv.base_url)
            .header("X-OpenEstate-Client-Id", "buyer_a_browser")
            .json(&json!({
                "slug": common::SYNTH_PRIMARY_SLUG,
                "documents": [
                    {
                        "name": "Malicious_Packet.zip",
                        "mime": "application/zip",
                        "base64": payload_b64
                    }
                ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            mal_resp.status(),
            400,
            "{label} upload must be rejected with HTTP 400"
        );
        let mal_json: Value = mal_resp.json().await.unwrap();
        assert_eq!(mal_json["ok"], false);
        assert_eq!(
            mal_json["code"], expected_code,
            "{label} expected error code {expected_code}, got: {mal_json}"
        );
    }

    // 4c. Buyer A uploads a .zip packet directly without an account (Home Inspection + Prelim Title + NHD + Permits, omitting Termite & HOA)
    let home_insp_pdf = build_pdf_bytes(&[&[
        "HOME INSPECTION REPORT",
        "Property: 100 Synthparcelalpha Ln, San Jose, CA 95118",
        "Client Contact: seller.private@example.com Phone (408) 555-0199 Tax ID 123-45-6789",
        "Seller Mailing Address: 742 Synth Maple St, San Jose, CA 95118",
        "Electrical Panel: Federal Pacific Electric Stab-Lok subpanel observed in garage (Contact (408) 555-0199 / Loan #LN-99887766); fire hazard, recommend immediate replacement.",
        "Roofing: Active roof leak and moisture staining observed around chimney flashing.",
    ]]);
    let prelim_pdf = build_pdf_bytes(&[&[
        "PRELIMINARY REPORT - ALTA COMMITMENT",
        "Property Address: 100 Synthparcelalpha Ln, San Jose, CA 95118",
        "Escrow Account #ESC-44332211 | Borrower Loan #LN-99887766",
        "Schedule B Exceptions:",
        "Item 8. Notice of Delinquent Property Taxes and Recorded Tax Lien in the amount of $6,400 (Escrow Account #ESC-44332211).",
    ]]);
    let nhd_pdf = build_pdf_bytes(&[&[
        "CALIFORNIA NATURAL HAZARD DISCLOSURE STATEMENT (NHD)",
        "Subject Property IS LOCATED IN a Liquefaction Zone mapped by the California Geological Survey.",
        "Subject Property IS NOT located in a Special Flood Hazard Area.",
    ]]);
    let permit_pdf = build_pdf_bytes(&[&[
        "CITY BUILDING PERMIT HISTORY & RECORDS",
        "Permit #B-2019-4410: Kitchen remodel — EXPIRED permit without final inspection sign-off.",
    ]]);

    let zip_bytes = build_zip_bytes(&[
        ("04_Building_Permits.pdf", &permit_pdf),
        ("03_Natural_Hazard_Disclosure_NHD.pdf", &nhd_pdf),
        ("02_Preliminary_Title_Report.pdf", &prelim_pdf),
        ("01_Home_Inspection_Report.pdf", &home_insp_pdf),
    ]);
    let zip_b64 = B64.encode(&zip_bytes);

    let disc_resp = client
        .post(format!("{}/api/copilot/disclosures", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "documents": [
                {
                    "name": "Disclosure_Package_Export.zip",
                    "mime": "application/zip",
                    "base64": zip_b64
                }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(disc_resp.status(), 200);
    let disc_json: Value = disc_resp.json().await.unwrap();
    assert_eq!(disc_json["ok"], true);
    assert_eq!(disc_json["sharingEnabled"], false);
    assert_eq!(disc_json["oneTimeCompleted"], true);
    assert_eq!(disc_json["fileRetentionPolicy"]["rawFilesSaved"], false);
    assert_eq!(disc_json["fileRetentionPolicy"]["rawFilesDistributed"], false);
    assert_eq!(
        disc_json["fileRetentionPolicy"]["analysisMode"],
        "per_user_one_time"
    );

    let prio = &disc_json["privateUserAnalysis"]["priorityReading"];
    let sections = prio["prioritySections"].as_array().expect("prioritySections");
    assert_eq!(sections.len(), 6);
    for (idx, sec) in sections.iter().enumerate() {
        assert_eq!(sec["priority"].as_u64().unwrap(), (idx + 1) as u64);
    }
    assert_eq!(sections[0]["present"], true);  // 1) Home Inspection
    assert_eq!(sections[1]["present"], false); // 2) Termite (missing)
    assert_eq!(sections[2]["present"], true);  // 3) Preliminary Title
    assert_eq!(sections[3]["present"], true);  // 4) NHD
    assert_eq!(sections[4]["present"], false); // 5) HOA Financials (missing)
    assert_eq!(sections[5]["present"], true);  // 6) Permits

    let missing_docs = prio["missingDocuments"].as_array().unwrap();
    assert_eq!(missing_docs.len(), 2);
    let missing_priorities: Vec<u64> = missing_docs
        .iter()
        .filter_map(|m| m["priority"].as_u64())
        .collect();
    assert_eq!(missing_priorities, vec![2, 5]);

    let red_flags = prio["redFlags"].as_array().unwrap();
    assert!(!red_flags.is_empty(), "must surface cited red flags");
    assert!(red_flags
        .iter()
        .all(|rf| !rf["sourceDoc"].as_str().unwrap_or("").is_empty()));
    assert!(prio["standingDisclaimer"]
        .as_str()
        .unwrap_or("")
        .contains("does not replace reading the full disclosure reports"));

    // Verify source-document audit log persisted via `list_admin_audit_logs`
    let audit_logs = srv.db.list_admin_audit_logs(50).await.expect("list audit logs");
    assert!(
        audit_logs
            .iter()
            .any(|row| row.action == "disclosure_findings_logged"
                && row.target_id == common::SYNTH_PRIMARY_SLUG),
        "disclosure findings must be audit-logged in admin_audit_log"
    );

    // 4d. One-time enforcement: Buyer A attempting second upload receives 409 ONE_TIME_ANALYSIS_ALREADY_USED
    let second_upload = client
        .post(format!("{}/api/copilot/disclosures", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "documents": [
                {
                    "name": "Disclosure_Package_Export.zip",
                    "mime": "application/zip",
                    "base64": zip_b64
                }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second_upload.status(), 409);
    let second_json: Value = second_upload.json().await.unwrap();
    assert_eq!(second_json["code"], "ONE_TIME_ANALYSIS_ALREADY_USED");

    // GET with Buyer A's client id returns Buyer A's saved private analysis
    let get_a: Value = client
        .get(format!(
            "{}/api/copilot/disclosures?slug={}",
            srv.base_url,
            common::SYNTH_PRIMARY_SLUG
        ))
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(get_a["sharingEnabled"], false);
    assert_eq!(get_a["oneTimeCompleted"], true);
    assert!(get_a["privateUserAnalysis"]["priorityReading"].is_object());
    assert_eq!(get_a["sharedWithMeAnalysis"], Value::Null);
    assert_eq!(get_a["authorizedDisclosures"], Value::Null);

    // 4e. Strict isolation: another browser client id sees zero private/shared data
    let get_other: Value = client
        .get(format!(
            "{}/api/copilot/disclosures?slug={}",
            srv.base_url,
            common::SYNTH_PRIMARY_SLUG
        ))
        .header("X-OpenEstate-Client-Id", "anon_buyer_0")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(get_other["privateUserAnalysis"], Value::Null);
    assert_eq!(get_other["sharedWithMeAnalysis"], Value::Null);
    assert_eq!(get_other["sharedDealTeamAnalysis"], Value::Null);
    assert_eq!(get_other["authorizedDisclosures"], Value::Null);

    let audit_logs_after = srv.db.list_admin_audit_logs(50).await.expect("list audit logs");
    let audit_count_before_purge = audit_logs_after.len();
    let actions: HashSet<String> = audit_logs_after.into_iter().map(|r| r.action).collect();
    assert!(actions.contains("disclosure_findings_logged"));

    // =========================================================================
    // 5. Function 3: 6-Group Guided Offer Builder + Lookup Defaults + Legal Disclaimers
    // =========================================================================
    let offer_init_json: Value = client
        .get(format!(
            "{}/api/copilot/offer?slug={}",
            srv.base_url,
            common::SYNTH_PRIMARY_SLUG
        ))
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(offer_init_json["ok"], true);
    let init_draft = &offer_init_json["offer"]["draft"];
    assert_eq!(init_draft["buyerAgentCommissionPct"], 2.5);
    // San Jose (Santa Clara County, Northern CA) lookup defaults:
    assert_eq!(init_draft["cityTransferTaxPayer"], "split_50_50");
    assert_eq!(init_draft["countyTransferTaxPayer"], "seller");
    assert_eq!(init_draft["escrowFeePayer"], "buyer");
    assert_eq!(init_draft["ownersTitlePayer"], "buyer");
    // Cross-module red flags imported from Function 2 disclosures:
    assert!(!offer_init_json["offer"]["context"]["SuggestedDisclosureRedFlags"]
        .as_array()
        .unwrap()
        .is_empty());

    // Step-by-step updates across all 6 Offer Builder groups verifying live runningSummary updates at each step
    // Step 1: Offer Price
    let s1: Value = client
        .post(format!("{}/api/copilot/offer", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "step": 1,
            "offerPriceUsd": 1_580_000
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s1["offer"]["runningSummary"]["offerPriceUsd"], 1_580_000);
    assert_eq!(s1["offer"]["draft"]["completedSteps"], json!([1]));

    // Step 2: Downpayment / Loan % / Earnest Money Deposit
    let s2: Value = client
        .post(format!("{}/api/copilot/offer", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "step": 2,
            "financingType": "conventional",
            "downPaymentPct": 25.0,
            "earnestMoneyPct": 3.5
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s2["offer"]["runningSummary"]["downPaymentUsd"], 395_000);
    assert_eq!(s2["offer"]["runningSummary"]["loanAmountUsd"], 1_185_000);
    assert_eq!(s2["offer"]["runningSummary"]["earnestMoneyPct"], 3.5);
    assert_eq!(s2["offer"]["draft"]["completedSteps"], json!([1, 2]));

    // Step 3: Close of Escrow (COE)
    let s3: Value = client
        .post(format!("{}/api/copilot/offer", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "step": 3,
            "coeDays": 25,
            "targetCloseDate": "2026-12-18"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s3["offer"]["runningSummary"]["coeDays"], 25);
    assert_eq!(
        s3["offer"]["runningSummary"]["targetCloseDate"],
        "2026-12-18"
    );
    assert_eq!(s3["offer"]["draft"]["completedSteps"], json!([1, 2, 3]));

    // Step 4: Contingencies (Loan, Inspection, Appraisal, Rent-Back, Sale-of-Property)
    let s4: Value = client
        .post(format!("{}/api/copilot/offer", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "step": 4,
            "loanContingencyEnabled": true,
            "loanContingencyDays": 17,
            "inspectionContingencyEnabled": true,
            "inspectionContingencyDays": 10,
            "appraisalContingencyEnabled": true,
            "appraisalContingencyDays": 14,
            "rentBackEnabled": true,
            "rentBackDays": 14,
            "rentBackDepositUsd": 5000,
            "saleOfPropertyContingencyEnabled": true,
            "saleOfPropertyAddress": "250 Synth Oak St, Campbell, CA",
            "saleOfPropertyDays": 30
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let s4_conts = s4["offer"]["runningSummary"]["activeContingencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(s4_conts.contains("Loan (17 days)"));
    assert!(s4_conts.contains("Inspection (10 days)"));
    assert!(s4_conts.contains("Appraisal (14 days)"));
    assert!(s4_conts.contains("Seller Rent-Back (14 days, $5000 deposit"));
    assert!(s4_conts.contains("Contingent on Sale of 250 Synth Oak St, Campbell, CA (30 days)"));

    // Step 5: Who Pays What (Commission editable 2.5% -> 2.0%, transfer taxes, title/escrow, home warranty)
    let s5: Value = client
        .post(format!("{}/api/copilot/offer", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "step": 5,
            "offerPriceUsd": 1_600_000,
            "buyerAgentCommissionPct": 2.0,
            "buyerAgentCommissionPayer": "seller",
            "homeWarrantyEnabled": true,
            "homeWarrantyMonths": 12,
            "homeWarrantyMaxUsd": 750,
            "homeWarrantyPayer": "seller",
            "saleOfPropertyContingencyEnabled": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        s5["offer"]["runningSummary"]["whoPaysWhat"]["buyerAgentCommission"]["pct"],
        2.0
    );
    assert_eq!(
        s5["offer"]["runningSummary"]["whoPaysWhat"]["buyerAgentCommission"]["totalUsd"],
        32_000
    );
    assert_eq!(
        s5["offer"]["runningSummary"]["whoPaysWhat"]["homeWarranty"]["maxUsd"],
        750
    );

    // Step 6: Import Function 2 disclosure red flags, then finalize repairs & seller credit
    let s6_import: Value = client
        .post(format!("{}/api/copilot/offer", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "step": 6,
            "importDisclosures": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !s6_import["offer"]["runningSummary"]["sellerRepairs"]
            .as_str()
            .unwrap_or("")
            .trim()
            .is_empty(),
        "importDisclosures: true must populate sellerRepairs from Function 2 red flags"
    );

    let offer_step_json: Value = client
        .post(format!("{}/api/copilot/offer", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .json(&json!({
            "slug": common::SYNTH_PRIMARY_SLUG,
            "step": 6,
            "sellerRepairs": "Replace Federal Pacific subpanel & clear expired kitchen permit prior to COE",
            "sellerCreditUsd": 12000
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(offer_step_json["ok"], true);
    let summary = &offer_step_json["offer"]["runningSummary"];
    assert_eq!(summary["offerPriceUsd"], 1_600_000);
    assert_eq!(summary["downPaymentUsd"], 400_000);
    assert_eq!(summary["loanAmountUsd"], 1_200_000);
    assert_eq!(summary["sellerCreditUsd"], 12_000);
    assert_eq!(
        summary["whoPaysWhat"]["buyerAgentCommission"]["pct"],
        2.0
    );
    assert_eq!(
        summary["whoPaysWhat"]["buyerAgentCommission"]["totalUsd"],
        32_000
    );
    // San Jose conveyance tax ($3.30/$1k = $5,280 split 50/50 -> $2,640 buyer / $2,640 seller)
    assert!(
        summary["whoPaysWhat"]["cityTransferTax"]["totalUsd"]
            .as_i64()
            .unwrap()
            >= 5_280
    );
    let final_rev = &offer_step_json["offer"]["finalReview"];
    assert_eq!(final_rev["isContract"], false);
    assert!(final_rev["disclaimer"]
        .as_str()
        .unwrap_or("")
        .contains("NOT A SIGNED OR BINDING PURCHASE CONTRACT"));
    let post_nar_notice = final_rev["postNarSettlementNotice"]
        .as_str()
        .unwrap_or("");
    assert!(
        !post_nar_notice.is_empty()
            && (post_nar_notice.to_ascii_lowercase().contains("nar")
                || post_nar_notice.to_ascii_lowercase().contains("buyer-broker")
                || post_nar_notice.to_ascii_lowercase().contains("representation")),
        "finalReview.postNarSettlementNotice must disclose post-NAR buyer-broker agreement requirement: {post_nar_notice}"
    );
    let transfer_tax_disc = final_rev["transferTaxDisclaimer"]
        .as_str()
        .unwrap_or("");
    assert!(
        !transfer_tax_disc.is_empty()
            && transfer_tax_disc.to_ascii_lowercase().contains("transfer"),
        "finalReview.transferTaxDisclaimer must disclose state/county transfer-tax customariness: {transfer_tax_disc}"
    );

    // =========================================================================
    // 6. DB TTL Retention Purge (`DbStore::purge_expired_copilot_disclosures`)
    // =========================================================================
    assert_eq!(
        TABLE_NAMES.len(),
        44,
        "TABLE_NAMES.len() == 44 must hold strictly"
    );
    // Fresh private + shared copilot disclosure rows within 30-day TTL are retained (0 purged)
    let purged_fresh = srv
        .db
        .purge_expired_copilot_disclosures(86_400 * 30)
        .await
        .expect("purge_expired_copilot_disclosures(30d)");
    assert_eq!(purged_fresh, 0, "fresh disclosure rows must not be purged before TTL");

    // With ttl_secs = 0, private (`copilot_disclosures`) rows expire immediately
    let purged_expired = srv
        .db
        .purge_expired_copilot_disclosures(0)
        .await
        .expect("purge_expired_copilot_disclosures(0)");
    assert!(
        purged_expired >= 1,
        "expected >= 1 private disclosure row purged on TTL expiry, got {purged_expired}"
    );

    let get_a_after_purge: Value = client
        .get(format!(
            "{}/api/copilot/disclosures?slug={}",
            srv.base_url,
            common::SYNTH_PRIMARY_SLUG
        ))
        .header("X-OpenEstate-Client-Id", "buyer_a_browser")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        get_a_after_purge["privateUserAnalysis"],
        Value::Null,
        "Buyer A's expired private disclosure row must be purged"
    );

    // Audit trail in `admin_audit_log` and `TABLE_NAMES.len() == 44` remain 100% preserved
    let audit_logs_post_purge = srv
        .db
        .list_admin_audit_logs(50)
        .await
        .expect("list audit logs after TTL purge");
    assert!(
        audit_logs_post_purge.len() >= audit_count_before_purge,
        "admin_audit_log rows must be preserved across copilot disclosure TTL purge"
    );
    assert_eq!(TABLE_NAMES.len(), 44);
}

#[test]
fn pdf_studio_and_role_sync_browser_behavior_in_node() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let home_insp_pdf = build_pdf_bytes(&[&[
        "HOME INSPECTION REPORT",
        "Electrical Panel: Federal Pacific Electric Stab-Lok subpanel hazard observed.",
    ]]);
    let prelim_pdf = build_pdf_bytes(&[&["PRELIMINARY REPORT - ALTA COMMITMENT"]]);
    let nhd_pdf = build_pdf_bytes(&[&["NATURAL HAZARD DISCLOSURE NHD"]]);
    let permit_pdf = build_pdf_bytes(&[&["BUILDING PERMIT HISTORY"]]);
    let zip_bytes = build_zip_bytes(&[
        ("01_Home_Inspection.pdf", &home_insp_pdf),
        ("02_Prelim_Title.pdf", &prelim_pdf),
        ("03_NHD.pdf", &nhd_pdf),
        ("04_Permits.pdf", &permit_pdf),
    ]);
    let zip_b64 = B64.encode(&zip_bytes);

    let js_script = r###"
const fs = require('fs');
const path = require('path');
const vm = require('vm');
const assert = require('assert');

const root = process.env.OE_ROOT;
const zipB64 = process.env.OE_ZIP_B64;

function findNodeBinaryCheck() {}
function makeClassList(el) {
  const set = new Set();
  el._classSet = set;
  return {
    add: (...cls) => { cls.forEach(c => set.add(c)); el.className = Array.from(set).join(' '); },
    remove: (...cls) => { cls.forEach(c => set.delete(c)); el.className = Array.from(set).join(' '); },
    toggle: (c, force) => {
      if (force === undefined) { set.has(c) ? set.delete(c) : set.add(c); }
      else if (force) set.add(c);
      else set.delete(c);
      el.className = Array.from(set).join(' ');
      return set.has(c);
    },
    contains: (c) => set.has(c),
  };
}

const byIdMap = new Map();
function registerTreeIds(el) {
  if (!el || typeof el !== 'object') return;
  if (el.id) byIdMap.set(el.id, el);
  for (const ch of el.children || []) registerTreeIds(ch);
}

function createEl(tag) {
  const el = {
    tagName: String(tag || 'DIV').toUpperCase(),
    _id: '',
    get id() { return this._id; },
    set id(v) { this._id = String(v || ''); if (this._id) byIdMap.set(this._id, this); },
    className: '',
    children: [],
    parentNode: null,
    dataset: {},
    attrs: {},
    style: { setProperty() {} },
    listeners: {},
    _innerHTML: '',
    textContent: '',
    value: '',
    disabled: false,
    checked: false,
    clientWidth: 820,
    clientHeight: 920,
    scrollTop: 0,
    scrollLeft: 0,
    width: 612,
    height: 792,
    scrollTo(opts) { if (opts && opts.top != null) this.scrollTop = opts.top; },
    getBoundingClientRect() { return { left: 0, top: 0, width: 612, height: 792 }; },
    setAttribute(k, v) {
      this.attrs[k] = String(v);
      if (k === 'id') this.id = v;
      if (k.startsWith('data-')) {
        const camel = k.slice(5).replace(/-([a-z])/g, (_, ch) => ch.toUpperCase());
        this.dataset[camel] = String(v);
      }
    },
    getAttribute(k) { return this.attrs[k] ?? null; },
    removeAttribute(k) { delete this.attrs[k]; },
    appendChild(ch) {
      ch.parentNode = this;
      this.children.push(ch);
      registerTreeIds(ch);
      return ch;
    },
    insertBefore(ch, ref) { return this.appendChild(ch); },
    remove() { if (this.parentNode) this.parentNode.children = this.parentNode.children.filter(c => c !== this); },
    addEventListener(ev, fn) { (this.listeners[ev] = this.listeners[ev] || []).push(fn); },
    dispatchEvent(ev) {
      ev.target = ev.target || this;
      for (const fn of this.listeners[ev.type] || []) fn.call(this, ev);
      return true;
    },
    click() { this.dispatchEvent({ type: 'click', target: this, preventDefault() {}, stopPropagation() {} }); },
    focus() {},
    closest(sel) {
      if (sel.startsWith('#') && this.id === sel.slice(1)) return this;
      if (sel.startsWith('.') && this.classList.contains(sel.slice(1))) return this;
      return this.parentNode ? this.parentNode.closest(sel) : null;
    },
    getContext() {
      return {
        setTransform() {}, fillRect() {}, strokeRect() {}, clearRect() {}, beginPath() {}, moveTo() {}, lineTo() {},
        rect() {}, roundRect() {}, quadraticCurveTo() {}, bezierCurveTo() {}, setLineDash() {}, clip() {},
        stroke() {}, fill() {}, arc() {}, ellipse() {}, closePath() {}, save() {}, restore() {},
        fillText() {}, strokeText() {}, measureText(s) { return { width: String(s || '').length * 7 }; },
        drawImage() {}, translate() {}, rotate() {}, scale() {},
        fillStyle: '#000', strokeStyle: '#000', lineWidth: 1, font: '12px sans-serif', globalAlpha: 1,
        textAlign: 'left', textBaseline: 'alphabetic', shadowColor: 'transparent', shadowBlur: 0,
      };
    },
    toDataURL() { return 'data:image/png;base64,AA=='; },
    querySelectorAll(sel) {
      const out = [];
      const walk = (node) => {
        for (const ch of node.children || []) {
          if (matchesSel(ch, sel)) out.push(ch);
          walk(ch);
        }
      };
      walk(this);
      return out;
    },
    querySelector(sel) { return this.querySelectorAll(sel)[0] || null; },
  };
  el.classList = makeClassList(el);
  Object.defineProperty(el, 'innerHTML', {
    get() { return this._innerHTML; },
    set(html) {
      this._innerHTML = String(html || '');
      this.children = [];
      // Parse tags with id=, class=, data-*, value= into lightweight element nodes
      const tagRe = /<(div|span|button|input|select|option|textarea|canvas|form|details|summary|p|a|strong|h[1-6]|ul|li|table|thead|tbody|tr|th|td|label|link|style)\b([^>]*)>([^<]*)/gi;
      let m;
      while ((m = tagRe.exec(this._innerHTML)) !== null) {
        const node = createEl(m[1]);
        const attrsStr = m[2] || '';
        if (m[3]) node.textContent = m[3];
        const attrRe = /([a-zA-Z0-9_:-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')/g;
        let am;
        while ((am = attrRe.exec(attrsStr)) !== null) {
          const ak = am[1];
          const av = am[2] !== undefined ? am[2] : (am[3] || '');
          if (ak === 'id') node.id = av;
          else if (ak === 'class') av.split(/\s+/).filter(Boolean).forEach(c => node.classList.add(c));
          else if (ak === 'value') node.value = av;
          else node.setAttribute(ak, av);
        }
        node.parentNode = this;
        this.children.push(node);
        registerTreeIds(node);
      }
    }
  });
  return el;
}

function matchesSel(el, sel) {
  for (const part of String(sel).split(',').map(s => s.trim()).filter(Boolean)) {
    if (part.startsWith('#') && el.id === part.slice(1)) return true;
    if (part.startsWith('.') && el.classList.contains(part.slice(1))) return true;
    const attrM = part.match(/^\[([a-zA-Z0-9_-]+)(?:="([^"]*)")?\]$/);
    if (attrM) {
      const val = el.getAttribute(attrM[1]);
      if (val !== null && (attrM[2] === undefined || val === attrM[2])) return true;
    }
    if (/^[a-zA-Z]+$/.test(part) && el.tagName === part.toUpperCase()) return true;
  }
  return false;
}

const lsMap = new Map();
const docHead = createEl('head');
const docBody = createEl('body');
const docHtml = createEl('html');
docHtml.setAttribute('lang', 'en');
docHtml.appendChild(docHead);
docHtml.appendChild(docBody);

// Pre-seed required page mount elements
const copilotMount = createEl('div'); copilotMount.id = 'buyerCopilotHomeMount'; docBody.appendChild(copilotMount);
const hdrBtn = createEl('button'); hdrBtn.id = 'buyerCopilotHdrBtn'; docBody.appendChild(hdrBtn);
const roleHdr = createEl('div'); roleHdr.id = 'roleSwitchHdr'; docBody.appendChild(roleHdr);
const repScript = createEl('script'); repScript.id = 'report-data';
repScript.textContent = JSON.stringify({
  meta: { slug: '100-synthparcelalpha-ln-san-jose-ca-95118', displayAddress: '100 Synthparcelalpha Ln, San Jose, CA 95118' },
  geo: { matchedAddress: '100 Synthparcelalpha Ln, San Jose, CA 95118', city: 'San Jose', state: 'CA', zip: '95118' },
  facts: { sqft: 2000, beds: 4, baths: 2.5, yearBuilt: 1988, listPrice: 1620000, forSale: true },
  modules: {}
});
docBody.appendChild(repScript);

const winListeners = {};
const docListeners = {};
class SyntheticDomEvent { constructor(type, init) { this.type = type; this.detail = (init && init.detail) || {}; } }

const askCalls = [];
const lookupCalls = [];

const sandbox = {
  console, setTimeout, clearTimeout, setInterval, clearInterval,
  Uint8Array, ArrayBuffer, DataView, TextEncoder, TextDecoder, Blob, URL, URLSearchParams,
  DecompressionStream, Response, Headers, Request,
  atob: (b64) => Buffer.from(b64, 'base64').toString('binary'),
  btoa: (bin) => Buffer.from(bin, 'binary').toString('base64'),
  CustomEvent: SyntheticDomEvent,
  Event: SyntheticDomEvent,
  performance: { now: () => Date.now() },
  requestAnimationFrame: (fn) => setTimeout(() => fn(Date.now()), 0),
  cancelAnimationFrame: (id) => clearTimeout(id),
  localStorage: {
    getItem: (k) => lsMap.has(k) ? lsMap.get(k) : null,
    setItem: (k, v) => lsMap.set(k, String(v)),
    removeItem: (k) => lsMap.delete(k),
  },
  history: { state: null, replaceState() {} },
  matchMedia: () => ({ matches: false, addEventListener() {} }),
  fetch: async (url, init) => {
    const u = String(url);
    const body = init && init.body ? JSON.parse(init.body) : {};
    if (u.includes('/api/ask')) {
      askCalls.push(body);
      return {
        ok: true,
        status: 200,
        json: async () => ({
          ok: true,
          activeRole: body.role || 'unset',
          answer: 'Role=' + (body.role || 'unset') + ' grounded answer (p. 1).',
          citations: ['01_Home_Inspection.pdf (p. 1)'],
          followUps: ['Follow up 1'],
          ms: 5
        })
      };
    }
    if (u.includes('/api/copilot/lookup')) {
      lookupCalls.push(body);
      return {
        ok: true,
        status: 200,
        json: async () => ({
          ok: true,
          activeRole: body.role || 'unset',
          needsRoleSelection: !body.role,
          hasFullReport: true,
          property: {
            slug: '100-synthparcelalpha-ln-san-jose-ca-95118',
            matchedAddress: '100 Synthparcelalpha Ln, San Jose, CA 95118',
            listingStatus: 'for_sale',
            listingStatusLabel: 'Actively For Sale',
            onMarket: true,
            basicInfo: { sqft: 2000, beds: 4, baths: 2.5, yearBuilt: 1988, listPrice: 1620000 },
            actionMenu: [{ id: 'comps_pricing' }, { id: 'disclosures' }, { id: 'offer_builder' }]
          },
          publicRecordEquivalents: { available: true, items: [] },
          fileRetentionPolicy: { rawFilesSaved: false, rawFilesDistributed: false },
          savedState: {}
        })
      };
    }
    if (u.includes('/api/copilot/disclosures')) {
      return {
        ok: true,
        status: 200,
        json: async () => ({
          ok: true,
          oneTimeCompleted: false,
          publicRecordEquivalents: { available: true, items: [] },
          fileRetentionPolicy: { rawFilesSaved: false, rawFilesDistributed: false },
        })
      };
    }
    return { ok: true, status: 200, json: async () => ({ ok: true }) };
  },
  document: {
    readyState: 'complete',
    documentElement: docHtml,
    head: docHead,
    body: docBody,
    createElement: createEl,
    getElementById: (id) => byIdMap.get(id) || null,
    querySelectorAll: (sel) => docHtml.querySelectorAll(sel),
    querySelector: (sel) => docHtml.querySelector(sel),
    addEventListener: (ev, fn) => { (docListeners[ev] = docListeners[ev] || []).push(fn); },
  },
  window: null,
};
sandbox.window = sandbox;
sandbox.location = { pathname: '/r/100-synthparcelalpha-ln-san-jose-ca-95118', search: '', hash: '', origin: 'http://127.0.0.1' };
sandbox.navigator = { clipboard: { writeText: async () => {} } };
sandbox.addEventListener = (ev, fn) => { (winListeners[ev] = winListeners[ev] || []).push(fn); };
sandbox.dispatchEvent = (ev) => {
  for (const fn of winListeners[ev.type] || []) fn.call(sandbox, ev);
  return true;
};

// Provide deterministic PDF.js 4.4.168 (`getDocument`, `TextLayer`) via official `__OPEN_ESTATE_PDFJS_OVERRIDE__` hook
let textLayerRenderCount = 0;
sandbox.__OPEN_ESTATE_PDFJS_OVERRIDE__ = {
  GlobalWorkerOptions: { workerSrc: '' },
  TextLayer: class SyntheticPdfTextLayer {
    constructor({ textContentSource, container }) {
      this.tc = textContentSource;
      this.container = container;
    }
    async render() {
      textLayerRenderCount++;
      if (this.container) {
        const txt = (this.tc && this.tc.items || []).map(i => i.str).join(' ');
        this.container.innerHTML = '<span>' + txt + '</span>';
      }
    }
  },
  getDocument() {
    return {
      promise: Promise.resolve({
        numPages: 2,
        async getPage(p) {
          return {
            getViewport: ({ scale }) => ({
              width: 612 * scale,
              height: 792 * scale,
              convertToViewportPoint: (x, y) => [x * scale, (792 - y) * scale]
            }),
            render: () => ({ promise: Promise.resolve(), cancel() {} }),
            async getTextContent() {
              return {
                items: p === 1
                  ? [
                      { str: 'HOME INSPECTION REPORT', transform: [1,0,0,1,48,740], width: 200, height: 12 },
                      { str: 'Federal Pacific Electric Stab-Lok subpanel hazard observed', transform: [1,0,0,1,48,680], width: 320, height: 12 },
                    ]
                  : [{ str: 'Page 2 details', transform: [1,0,0,1,48,740], width: 140, height: 12 }]
              };
            }
          };
        }
      })
    };
  }
};

vm.createContext(sandbox);
for (const rel of ['assets/report_workspace.js', 'assets/pdf_studio.js', 'assets/buyer_copilot.js']) {
  const code = fs.readFileSync(path.join(root, rel), 'utf8');
  vm.runInContext(code, sandbox, { filename: rel });
}

(async () => {
  const Studio = sandbox.OpenEstatePdfStudio;
  const RoleApi = sandbox.OpenEstateRole;
  assert(Studio && typeof Studio.ingestFiles === 'function', 'OpenEstatePdfStudio must exist');
  assert(RoleApi && typeof RoleApi.setRole === 'function', 'OpenEstateRole must exist');

  // (a) Unpack real .zip bytes built in Rust in browser memory
  const zipRaw = Uint8Array.from(Buffer.from(zipB64, 'base64'));
  const slug = '100-synthparcelalpha-ln-san-jose-ca-95118';
  const ingested = await Studio.ingestFiles([{
    name: 'Disclosure_Packet.zip',
    type: 'application/zip',
    arrayBuffer: async () => zipRaw.buffer.slice(zipRaw.byteOffset, zipRaw.byteOffset + zipRaw.byteLength)
  }], slug);
  assert.strictEqual(Studio.hasLocalFiles(slug), true, 'hasLocalFiles must be true after .zip ingest');
  assert.strictEqual(ingested.length, 4, 'all 4 PDFs inside .zip must unpack in browser memory');

  const analysis = {
    priorityReading: {
      redFlags: [{
        title: 'Federal Pacific Electric Stab-Lok subpanel hazard',
        severity: 'Critical',
        detail: 'Immediate replacement recommended.',
        sourceDoc: '01_Home_Inspection.pdf',
        sourcePage: 1
      }],
      prioritySections: [{ priority: 1, label: 'Home Inspection', present: true, problematicItems: [] }]
    }
  };
  await Studio.openStudio({
    slug,
    address: '100 Synthparcelalpha Ln, San Jose, CA 95118',
    analysis,
    citation: { sourceDoc: '01_Home_Inspection.pdf', sourcePage: 1, title: 'Federal Pacific Electric Stab-Lok subpanel hazard', severity: 'Critical' }
  });

  const st1 = Studio.getState();
  assert.strictEqual(st1.isOpen, true);
  assert.strictEqual(st1.docs.length, 4);
  assert.strictEqual(st1.currentDocName, '01_Home_Inspection.pdf');
  assert.strictEqual(st1.currentPage, 1);
  // (b) Verify PDF.js + TextLayer rendered
  assert(textLayerRenderCount >= 1, 'PDF.js TextLayer must render');
  assert(byIdMap.get('psTextLayer')._innerHTML.includes('Federal Pacific'), 'TextLayer must contain extracted PDF text');
  // (c) Verify jumpToCitation matched text coordinates and created active AI pointer circle
  assert(st1.activeAiPointer && st1.activeAiPointer.matched === true, 'AI pointer circle must match Federal Pacific coordinates');

  // (d) Verify all 5 annotation tools (`circle`, `arrow`, `rect`, `pen`, `pin`) + `undoAnnotation` + `clearPageAnnotations`
  Studio.addAnnotation({ type: 'circle', cx: 0.4, cy: 0.3, rx: 0.1, ry: 0.05 });
  Studio.addAnnotation({ type: 'arrow', x1: 0.1, y1: 0.1, x2: 0.3, y2: 0.3 });
  Studio.addAnnotation({ type: 'rect', x: 0.2, y: 0.2, w: 0.3, h: 0.1 });
  Studio.addAnnotation({ type: 'pen', points: [{ x: 0.1, y: 0.2 }, { x: 0.2, y: 0.25 }] });
  Studio.addAnnotation({ type: 'pin', x: 0.5, y: 0.5 });
  assert.strictEqual(Studio.getState().annotationsCount, 5);
  assert(byIdMap.get('psChatInput').value.includes('[Pin #1: 01_Home_Inspection.pdf p.1]'), 'pin must insert citation token into chat input');
  Studio.undoAnnotation();
  assert.strictEqual(Studio.getState().annotationsCount, 4);
  Studio.clearPageAnnotations();
  assert.strictEqual(Studio.getState().annotationsCount, 0);

  // (e) Verify OpenEstateRole.setRole('seller') dispatches openestate:rolechange syncing workspace, copilot, & PDF studio chat
  let copilotModalTitleBefore = '';
  sandbox.OpenEstateBuyerCopilot.openModal(slug, '100 Synthparcelalpha Ln, San Jose, CA 95118');
  await new Promise(r => setTimeout(r, 30));
  copilotModalTitleBefore = byIdMap.get('bcModalTitle').textContent;

  RoleApi.setRole('seller');
  await new Promise(r => setTimeout(r, 30));
  assert.strictEqual(RoleApi.getRole(), 'seller');
  assert.strictEqual(Studio.getState().role, 'seller', 'PDF Studio role must sync to seller on openestate:rolechange');
  assert(byIdMap.get('bcModalTitle').textContent.includes('Seller Mode'), 'Copilot modal title must update to Seller Mode');
  assert.notStrictEqual(byIdMap.get('bcModalTitle').textContent, copilotModalTitleBefore);

  // Send a chat message inside PDF Studio and verify `role: "seller"` is posted to `/api/ask`
  byIdMap.get('psTabBtnChat').click();
  byIdMap.get('psChatInput').value = 'What pre-listing repairs matter most?';
  byIdMap.get('psChatForm').dispatchEvent({ type: 'submit', preventDefault() {}, stopPropagation() {} });
  await new Promise(r => setTimeout(r, 30));
  assert(askCalls.length >= 1, 'PDF Studio chat must call /api/ask');
  assert.strictEqual(askCalls[askCalls.length - 1].role, 'seller', '/api/ask must receive synced seller role');
  assert(byIdMap.get('psChatLog')._innerHTML.includes('Role=seller grounded answer'), 'PDF Studio chat log must render answer');

  console.log('NODE_PDF_STUDIO_AND_ROLE_SYNC_OK');
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
"###;

    let node_bin = if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok()
    {
        PathBuf::from("node")
    } else {
        let fallback = PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".local/bin/node");
        assert!(fallback.exists(), "node binary must exist for browser JS test");
        fallback
    };

    let out = std::process::Command::new(node_bin)
        .arg("-e")
        .arg(js_script)
        .env("OE_ROOT", manifest_dir)
        .env("OE_ZIP_B64", &zip_b64)
        .output()
        .expect("run node PDF Studio & role sync harness");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("NODE_PDF_STUDIO_AND_ROLE_SYNC_OK"),
        "Node PDF Studio + role sync test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
}
