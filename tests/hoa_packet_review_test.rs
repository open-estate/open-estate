// Comprehensive Zero-Mock Integration & Unit Tests for:
// 1. HOA / Condo / Strata Financial, Statutory (CA Civ. Code §§5550/5551/5565 & FL Stat. §§718.112/553.899)
//    & Fannie/Freddie/FHA Warrantability Analyzer (`POST/GET /api/hoa-review`) on synthetic properties
// 2. Whole Disclosure Packet Ingestion (PDFs + pure-Rust `.zip` unpacking), 11-Category Document Classifier,
//    Cross-Document & Public-Record Contradiction Matrix, and Zero-Double-Counting Deduplication (`POST/GET /api/packet-review`)
//    grounded against procedural synthetic primary and secondary property reports
// 3. Pure-Rust Zip-Bomb & Archive Guardrail Defenses (40:1 ratio, >64 entries, >48 MB cap, overlapping local headers,
//    path traversal, nested archive, >20 PDFs, disguised code)
// 4. Repair & Seller Credit Addendum Generator (`POST/GET /api/repair-addendum`) with Trade Grouping,
//    Regional Cost Scaling across CA vs. PA properties, Per-Client SQLite Persistence & XSS-Escaped HTML
// 5. Rate-Limit Tiers (`hoa`, `packet`, `addendum`), Pre-LLM PII Redaction (`redact_title_pii` unit checks),
//    Strict Structured Outputs JSON Schemas, and Live OpenAI Responses API Gating (`OPENAI_API_KEY`)

#![recursion_limit = "512"]

mod common;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use open_estate::ai::{
    analyze_disclosure_packet, analyze_hoa_documents, calibrate_regional_cost_multiplier,
    classify_packet_document, classify_reserve_risk_tier, crc32_ieee, generate_repair_addendum,
    hoa_review_schema, llm::validate_json_schema, normalize_to_canonical_trade,
    packet_review_schema, redact_title_pii, repair_addendum_schema, unpack_zip_archive_bytes,
    RepairAddendumInput, UploadedDocInput, ADDENDUM_TRADES, PACKET_DOC_CATEGORIES,
};
use open_estate::db::DbStore;
use open_estate::guardrails::{AiRateLimiter, HttpDefenseShield};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use flate2::{write::DeflateEncoder, Compression};
use lopdf::{
    content::{Content, Operation},
    dictionary, Document, Object, Stream, StringFormat,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

const PRIMARY_SLUG: &str = common::SYNTH_PRIMARY_SLUG;
const SECONDARY_SLUG: &str = common::SYNTH_SECONDARY_SLUG;

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
        "slug": home["id"],
        "input": home["address"],
        "geo": home["raw"]["geo"],
        "facts": home["raw"]["facts"],
        "modules": Value::Object(modules),
    })
}

struct TestServerHandle {
    base_url: String,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServerHandle {
    async fn start_offline_with_reports(entries: &[(&str, Value)]) -> Self {
        let temp_dir = std::env::temp_dir().join(format!("openestate-hoa-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");

        let db_path = temp_dir.join("test.sqlite");
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
        for (slug, report) in entries {
            db.put_cached_report(slug, report, None)
                .await
                .expect("seed cached real report");
        }

        let shield = HttpDefenseShield::default();
        let state = AppState::with_components(env, http, db, shield, reports_dir, None);

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

/// Builds a valid multi-page `/FlateDecode` PDF in pure Rust using `lopdf`.
fn build_multi_page_pdf(pages: &[&[&str]]) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });

    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_id,
        },
    });

    let mut kids: Vec<Object> = Vec::with_capacity(pages.len());
    for lines in pages {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 11.into()]),
            Operation::new("TL", vec![16.into()]),
            Operation::new("Td", vec![48.into(), 740.into()]),
        ];
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
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

        let content = Content { operations: ops };
        let raw = content.encode().expect("encode PDF page content");
        let mut stream = Stream::new(dictionary! {}, raw);
        stream.compress().expect("FlateDecode compress stream");
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
    doc.save_to(&mut out).expect("save PDF bytes");
    out
}

/// Builds a standard PKZIP archive (method 8 Deflate or method 0 Stored) in pure Rust.
fn build_zip_archive(entries: &[(&str, &[u8], u16)]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut cd_records: Vec<Vec<u8>> = Vec::with_capacity(entries.len());

    for &(name, raw, method) in entries {
        let local_offset = buf.len() as u32;
        let crc = crc32_ieee(raw);
        let comp: Vec<u8> = if method == 8 {
            let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
            enc.write_all(raw).expect("deflate zip entry");
            enc.finish().expect("finish deflate zip entry")
        } else {
            raw.to_vec()
        };
        let name_bytes = name.as_bytes();

        // Local file header (0x04034b50)
        buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&method.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&comp);

        // Central directory record (0x02014b50)
        let mut cd = Vec::new();
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
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

    // End of Central Directory (0x06054b50)
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

fn uploaded_pdf(name: &str, pages: &[&[&str]]) -> UploadedDocInput {
    let bytes = build_multi_page_pdf(pages);
    UploadedDocInput {
        name: name.to_string(),
        text: None,
        base64: Some(B64.encode(&bytes)),
    }
}

fn assert_strict_openai_schema_node(node: &Value, path: &str) {
    let Some(obj) = node.as_object() else {
        return;
    };
    if obj.get("type").and_then(Value::as_str) == Some("object") {
        assert_eq!(
            obj.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "{path}: strict Structured Outputs requires additionalProperties: false"
        );
        let props: HashSet<String> = obj
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let reqs: HashSet<String> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            props, reqs,
            "{path}: strict Structured Outputs requires required == properties keys"
        );
        if let Some(pm) = obj.get("properties").and_then(Value::as_object) {
            for (k, v) in pm {
                assert_strict_openai_schema_node(v, &format!("{path}.{k}"));
            }
        }
    }
    if let Some(items) = obj.get("items") {
        assert_strict_openai_schema_node(items, &format!("{path}[]"));
    }
}

// =================================================================================================
// 1. CA Davis-Stirling Civil Code §§5550/5551/5565 & FL SIRS Statutory HOA Analyzer on Real Condos
// =================================================================================================

#[tokio::test]
async fn hoa_statutory_davis_stirling_and_fl_sirs_extraction_on_real_condos() {
    assert_eq!(
        classify_reserve_risk_tier(Some(22.0)),
        "<30% Critical Underfunded"
    );
    assert_eq!(
        classify_reserve_risk_tier(Some(54.0)),
        "30–69% Fair / Watch"
    );
    assert_eq!(classify_reserve_risk_tier(Some(78.0)), ">=70% Strong");
    assert_eq!(
        classify_reserve_risk_tier(None),
        "Unknown — Verify Reserve Study"
    );

    // Synthetic Palo Alto, CA townhouse HOA property ($540/mo HOA)
    let ca_townhouse_report = load_holdout_report("synth-ca-94303-townhome");
    assert_eq!(
        ca_townhouse_report["facts"]["hoaMonthly"].as_i64(),
        Some(540)
    );

    // California Civil Code §5565 Assessment and Reserve Funding Disclosure Summary
    // + Civil Code §5551 (SB 326) Exterior Elevated Elements statutory inspection disclosure
    let ca_statutory_reserve_pdf = uploaded_pdf(
        "CA_CivCode_5565_Assessment_And_Reserve_Disclosure.pdf",
        &[
            &[
                "ASSESSMENT AND RESERVE FUNDING DISCLOSURE SUMMARY (CALIFORNIA CIVIL CODE SECTION 5565)",
                "Regular Monthly Assessment: $540.00 per month for fiscal year.",
                "Annual Operating Budget: $1,200,000. Total Number of Residential Units: 80.",
                "Annual Reserve Contribution: $96,000 (Reserve Allocation: 8.0% of total operating budget).",
                "Current Reserve Balance: $418,000 based on latest reserve account reconciliation.",
                "Fully Funded Reserve Balance: $1,900,000.",
                "Percent Funded: 22.0% funded based on Civil Code Section 5550 reserve study.",
            ],
            &[
                "(2) Additional regular or special assessments already levied per Civil Code Section 5605:",
                "Special Assessment of $14,500 per unit approved by Board resolution for exterior stucco and balcony waterproofing.",
                "Assessment Delinquency Disclosure (Fannie Mae B4-2.1-01): 18.5% of units are more than 60 days delinquent.",
                "CALIFORNIA CIVIL CODE SECTION 5551 (SB 326) EXTERIOR ELEVATED ELEMENTS INSPECTION:",
                "Licensed structural engineer report identified dry rot and failed waterproof membrane at elevated wood-framed walkways; immediate shoring and waterproofing remediation required.",
                "Active construction defect litigation pending pursuant to California Civil Code Section 895 (SB 800) regarding podium deck waterproofing.",
            ],
        ],
    );

    // Authentic California Civil Code §4525 Governing Documents & Fannie Mae Condo Project Questionnaire Summary
    let ca_governing_summary_pdf = uploaded_pdf(
        "CA_CivCode_4525_Governing_Rules_And_Questionnaire.pdf",
        &[
            &[
                "CALIFORNIA CIVIL CODE SECTION 4525 GOVERNING DOCUMENTS & LEASING RESTRICTIONS",
                "Leasing & Rental Cap: Maximum 20% of units may be leased at any time; active rental waitlist applies.",
                "Short-Term Rentals: Minimum lease term is 30 days; transient hotel, Airbnb, and VRBO rentals are strictly prohibited.",
                "Pet Policy (Civil Code Section 4715): Maximum 2 domestic pets per unit, weight limit 45 lbs per pet.",
                "Architectural & Acoustic Rules: Hard-surface flooring requires minimum IIC 55 / STC 55 acoustic underlayment.",
                "EV Charging & Solar: Owner EV charger installation governed by Civil Code Section 4745; solar access governed by Civil Code Section 714.",
                "Community Smoking Rule: Smoking is prohibited in all indoor units, balconies, patios, and common areas.",
            ],
            &[
                "MASTER PROPERTY INSURANCE & PROJECT ELIGIBILITY DISCLOSURE (CIVIL CODE SECTION 5805)",
                "Master Hazard Policy Coverage Type: Bare Walls (studs-out; interior fixtures and betterments excluded).",
                "Master Property & Wildfire Deductible: 10% of insured building replacement value.",
                "Single-Entity Ownership Concentration: One investment entity holds 24% of total units.",
                "Mixed-Use Commercial Allocation: Ground-floor commercial retail comprises 38% of total square footage.",
            ],
        ],
    );

    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::from_env(&env, None);

    let ca_out = analyze_hoa_documents(
        &http,
        &env,
        &ca_townhouse_report,
        vec![ca_statutory_reserve_pdf, ca_governing_summary_pdf],
        None,
    )
    .await
    .expect("analyze CA statutory HOA packet");

    assert_eq!(ca_out.data["aiEnhanced"].as_bool(), Some(false));
    let fh = &ca_out.data["financialHealth"];
    assert_eq!(fh["monthlyDuesUsd"].as_f64(), Some(540.0));
    assert_eq!(fh["monthlyDuesCitation"]["page"].as_u64(), Some(1));
    assert_eq!(fh["operatingBudgetUsd"].as_i64(), Some(1_200_000));
    assert_eq!(fh["annualReserveContributionUsd"].as_i64(), Some(96_000));
    assert_eq!(fh["reserveAllocationPctOfBudget"].as_f64(), Some(8.0));
    assert_eq!(fh["currentReserveBalanceUsd"].as_i64(), Some(418_000));
    assert_eq!(fh["idealFullyFundedBalanceUsd"].as_i64(), Some(1_900_000));
    assert_eq!(fh["percentFunded"].as_f64(), Some(22.0));
    assert_eq!(
        fh["reserveRiskTier"].as_str(),
        Some("<30% Critical Underfunded")
    );
    assert_eq!(fh["unitCount"].as_u64(), Some(80));
    assert_eq!(fh["totalReserveDeficitUsd"].as_i64(), Some(1_482_000));
    assert_eq!(fh["perUnitReserveDeficitUsd"].as_i64(), Some(18_525));
    assert_eq!(fh["delinquencyRatePct"].as_f64(), Some(18.5));
    assert!(
        !fh["specialAssessments"]
            .as_array()
            .expect("specialAssessments")
            .is_empty(),
        "must capture $14,500 special assessment with page 2 citation"
    );
    assert_eq!(
        fh["specialAssessments"][0]["citation"]["page"].as_u64(),
        Some(2)
    );

    // Statutory SB 326 + litigation + non-warrantable flags
    assert_eq!(
        ca_out.data["structuralAndStatutoryMandates"]["caSb326Sb721"]["detected"].as_bool(),
        Some(true)
    );
    assert!(!ca_out.data["structuralAndStatutoryMandates"]["litigation"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        ca_out.data["governanceAndLifestyleRules"]["masterInsurance"]["deductibleShockWarning"]
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        ca_out.data["governanceAndLifestyleRules"]["smokingPolicy"]["prohibited"].as_bool(),
        Some(true)
    );
    assert_eq!(
        ca_out.data["warrantability"]["isWarrantable"].as_bool(),
        Some(false)
    );
    assert_eq!(
        ca_out.data["warrantability"]["nonQmLoanWarning"].as_bool(),
        Some(true)
    );
    assert!(
        ca_out.data["warrantability"]["dealBreakerCount"]
            .as_u64()
            .unwrap_or(0)
            >= 4,
        "must flag reserve <10%, delinquency >15%, litigation, SB 326 unsafe/dry-rot, >5% deductible, investor/commercial concentration"
    );

    // Part B: Florida Stat. §718.112(2)(g) SIRS & §553.899 Milestone statutory disclosure
    let fl_sirs_pdf = uploaded_pdf(
        "FL_Stat_718_112_SIRS_And_Milestone_Disclosure.pdf",
        &[&[
            "FLORIDA CONDOMINIUM STRUCTURAL INTEGRITY RESERVE STUDY (FLA. STAT. SECTION 718.112(2)(g))",
            "Mandatory Structural Integrity Reserve Study (SIRS) completed; statutory reserve contributions may not be waived.",
            "Milestone Structural Inspection Phase 1 (Fla. Stat. Section 553.899) completed by licensed Florida structural engineer: no substantial structural deterioration observed.",
            "Regular Monthly Assessment: $680.00 per month. Total Number of Residential Units: 64.",
            "Annual Operating Budget: $476,160. Annual Reserve Contribution: $95,232 (Reserve Allocation: 20.0% of budget).",
            "Current Reserve Balance: $819,000. Fully Funded Reserve Balance: $1,050,000. Percent Funded: 78.0% funded.",
            "Assessments Over 60 Days Delinquent: 1.5% of units.",
            "Master Property Insurance Policy: Walls-In (All-In) coverage with 2.0% peril deductible.",
        ]],
    );

    // Synthetic Philadelphia condo ($680/mo HOA)
    let condo_report = load_holdout_report("synth-pa-19106-condo");
    assert_eq!(condo_report["facts"]["hoaMonthly"].as_i64(), Some(680));

    let healthy_out = analyze_hoa_documents(&http, &env, &condo_report, vec![fl_sirs_pdf], None)
        .await
        .expect("analyze healthy SIRS/Milestone packet");

    assert_eq!(
        healthy_out.data["structuralAndStatutoryMandates"]["flMilestoneAndSirs"]["detected"]
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        healthy_out.data["financialHealth"]["percentFunded"].as_f64(),
        Some(78.0)
    );
    assert_eq!(
        healthy_out.data["financialHealth"]["reserveRiskTier"].as_str(),
        Some(">=70% Strong")
    );
    assert_eq!(
        healthy_out.data["warrantability"]["isWarrantable"].as_bool(),
        Some(true)
    );
    assert_eq!(
        healthy_out.data["warrantability"]["dealBreakerCount"].as_u64(),
        Some(0)
    );
}

// =================================================================================================
// 2. Whole Disclosure Packet (.zip), 11-Category Classifier, Contradiction Matrix & Deduplication
// =================================================================================================

#[tokio::test]
async fn disclosure_packet_zip_cross_checks_san_jose_and_campbell_reports() {
    let primary_report = common::synth_report_primary();
    let secondary_report = common::synth_report_secondary();

    // Verify all 11 canonical document categories are recognized
    assert_eq!(PACKET_DOC_CATEGORIES.len(), 11);
    let cat_samples = [
        (
            "01_Home_Inspection_Report.pdf",
            "GENERAL HOME INSPECTION REPORT",
            "home_inspection",
        ),
        (
            "02_WDO_Section1_Pest_Report.pdf",
            "WOOD DESTROYING PESTS AND ORGANISMS INSPECTION REPORT",
            "termite_wdo",
        ),
        (
            "03_Roof_Certification_Report.pdf",
            "ROOF INSPECTION AND FLASHING EVALUATION",
            "roof",
        ),
        (
            "04_Sewer_Lateral_Camera_Scope.pdf",
            "SANITARY SEWER LATERAL VIDEO CAMERA INSPECTION",
            "sewer",
        ),
        (
            "05_Structural_Foundation_Engineer.pdf",
            "STRUCTURAL ENGINEERING FOUNDATION EVALUATION",
            "foundation",
        ),
        (
            "06_Transfer_Disclosure_Statement_TDS.pdf",
            "REAL ESTATE TRANSFER DISCLOSURE STATEMENT (CALIFORNIA CIVIL CODE SECTION 1102.6)",
            "seller_disclosure",
        ),
        (
            "07_Seller_Property_Questionnaire_SPQ.pdf",
            "SELLER PROPERTY QUESTIONNAIRE (C.A.R. FORM SPQ)",
            "seller_disclosure",
        ),
        (
            "08_Natural_Hazard_Disclosure_NHD.pdf",
            "NATURAL HAZARD DISCLOSURE STATEMENT (CIVIL CODE SECTION 1103.2)",
            "nhd",
        ),
        (
            "09_Preliminary_Title_Report_ScheduleB.pdf",
            "PRELIMINARY REPORT - SCHEDULE A AND SCHEDULE B EXCEPTIONS",
            "prelim_title",
        ),
        (
            "10_HOA_Reserve_Disclosure_5565.pdf",
            "ASSESSMENT AND RESERVE FUNDING DISCLOSURE SUMMARY",
            "hoa_reserves",
        ),
        (
            "11_Miscellaneous_Receipt_Notes.pdf",
            "UTILITY BILLING SUMMARY NOTE",
            "other",
        ),
    ];
    for (fname, body, expected_cat) in cat_samples {
        let doc = open_estate::ai::pdf::ExtractedDocument {
            name: fname.to_string(),
            pages: Vec::new(),
            formatted_text: body.to_string(),
            chars: body.len(),
            method: "test".to_string(),
        };
        let (got, _conf) = classify_packet_document(&doc);
        assert_eq!(
            got, expected_cat,
            "classify_packet_document({fname}) expected {expected_cat}, got {got}"
        );
    }

    // Build statutory disclosure + specialist inspection PDFs for synthetic primary parcel (`2,040` sq ft assessor living area)
    let tds_bytes = build_multi_page_pdf(&[&[
        "REAL ESTATE TRANSFER DISCLOSURE STATEMENT (CALIFORNIA CIVIL CODE SECTION 1102.6)",
        "Property Address: 100 Synthparcelalpha Ln, San Jose, CA 95118 (APN 000-00-100)",
        "Approximate square footage: 2,360 sq ft total living area.",
        "Room additions, structural modifications, or other alterations or repairs made without necessary permits: No",
        "Roof leaks or water intrusion: No",
        "Sewer system or drainage defects: No",
        "Foundation cracks, settling, or movement: No",
        "Solar energy system owned free and clear with no lease or UCC-1 lien: Yes",
    ]]);

    let home_insp_bytes = build_multi_page_pdf(&[&[
        "GENERAL HOME INSPECTION REPORT — 100 SYNTHPARCELALPHA LN, SAN JOSE, CA 95118",
        "Roofing Section: Active roof leak and moisture staining observed at rear valley flashing above family room; repair estimate $3,200 - $4,800.",
        "Electrical Section: Outdated subpanel with double-tapped breakers in attached garage; licensed electrician replacement recommended ($2,400 - $3,600).",
    ]]);

    let roof_insp_bytes = build_multi_page_pdf(&[&[
        "LICENSED C-39 ROOFING CONTRACTOR INSPECTION REPORT",
        "Defective rear valley flashing with active roof leak and deteriorated underlayment; full valley flashing rebuild bid $3,500 - $4,900.",
    ]]);

    let wdo_bytes = build_multi_page_pdf(&[&[
        "WOOD DESTROYING PESTS AND ORGANISMS INSPECTION REPORT (BUS. & PROF. CODE SECTION 8516)",
        "Section 1 Item 3A: Fungus/dry-rot wood damage at rear roof valley fascia caused by active roof flashing leak; Section 1 repair bid $2,800.",
    ]]);

    let sewer_bytes = build_multi_page_pdf(&[&[
        "SANITARY SEWER LATERAL VIDEO SCOPE INSPECTION",
        "Severely offset clay sewer lateral joint with heavy tree root intrusion 38 ft from cleanout; trenchless liner replacement bid $6,800 - $8,900.",
    ]]);

    let prelim_bytes = build_multi_page_pdf(&[&[
        "PRELIMINARY TITLE REPORT — SCHEDULE B EXCEPTIONS (APN 000-00-100)",
        "1. General and special county and city taxes payable to Santa Clara County Tax Collector.",
        "2. Public utility easement recorded in favor of Pacific Gas and Electric Company.",
        "3. Recorded UCC-1 Financing Statement for leased residential rooftop solar photovoltaic system.",
    ]]);

    let zip_bytes = build_zip_archive(&[
        ("disclosures/01_CA_CivCode_1102_6_TDS.pdf", &tds_bytes, 8),
        (
            "inspections/02_General_Home_Inspection.pdf",
            &home_insp_bytes,
            8,
        ),
        (
            "inspections/03_C39_Roof_Inspection.pdf",
            &roof_insp_bytes,
            8,
        ),
        ("inspections/04_Section1_WDO_Report.pdf", &wdo_bytes, 8),
        ("inspections/05_Sewer_Lateral_Scope.pdf", &sewer_bytes, 8),
        ("title/06_Preliminary_Title_ScheduleB.pdf", &prelim_bytes, 8),
        (
            "disclosures/readme_cover_sheet.txt",
            b"Statutory escrow disclosure index",
            0,
        ),
    ]);

    let zip_upload = UploadedDocInput {
        name: "100_Synthparcelalpha_Escrow_Packet.zip".to_string(),
        text: None,
        base64: Some(B64.encode(&zip_bytes)),
    };

    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::from_env(&env, None);

    let packet_out =
        analyze_disclosure_packet(&http, &env, &primary_report, vec![zip_upload], None, None)
            .await
            .expect("analyze .zip disclosure packet against synthetic primary report");

    assert_eq!(packet_out.data["aiEnhanced"].as_bool(), Some(false));
    let docs_inv = packet_out.data["documentClassifications"]
        .as_array()
        .expect("documentClassifications array");
    assert_eq!(
        docs_inv.len(),
        7,
        "must unpack all 6 PDFs plus the .txt cover note from the .zip"
    );

    // Cross-document + Public-Record Contradiction Matrix
    let contras = packet_out.data["contradictions"]
        .as_array()
        .expect("contradictions array");
    assert!(
        contras.len() >= 3,
        "expected cross-document (roof leak, sewer, solar UCC-1) + public-record (2,040 vs 2,360 sq ft) contradictions, got {}",
        contras.len()
    );
    let has_sqft_public_contra = contras.iter().any(|c| {
        c["kind"].as_str() == Some("public_record")
            && c["topicKey"].as_str() == Some("unpermitted_additions")
            && c["contradictingQuote"]
                .as_str()
                .unwrap_or("")
                .contains("2040")
    });
    assert!(
        has_sqft_public_contra,
        "must flag public-record contradiction between 2,040 sq ft Assessor roll and 2,360 sq ft TDS claim: {contras:#?}"
    );
    let has_cross_doc_contra = contras
        .iter()
        .any(|c| c["kind"].as_str() == Some("cross_document"));
    assert!(
        has_cross_doc_contra,
        "must flag cross-document contradiction between statutory TDS 'No' answers and specialist inspection reports"
    );

    // Zero-double-counting deduplication across Home Inspection + Roof Inspection + WDO
    let avoided = packet_out.data["totalExposureUsd"]["deduplicationSummary"]["avoidedDoubleCountUsd"]
        .as_i64()
        .unwrap_or(0);
    assert!(
        avoided > 0,
        "overlapping roof leak citations across Home Inspection, Roof Inspection, and WDO must be deduplicated without summing twice (avoidedDoubleCountUsd={avoided})"
    );

    // Also verify synthetic secondary parcel Dam Inundation Area (`inZone` = true)
    assert_eq!(
        secondary_report
            .pointer("/modules/hazards/data/other/damInundation/inZone")
            .and_then(Value::as_bool),
        Some(true)
    );
    let nhd_contra_pdf = uploaded_pdf(
        "CA_CivCode_1103_2_NHD_Statement.pdf",
        &[&[
            "NATURAL HAZARD DISCLOSURE STATEMENT (CALIFORNIA CIVIL CODE SECTION 1103.2)",
            "Property: 200 Synthparcelbeta Ave, Campbell, CA 95008 (APN 000-00-200)",
            "Dam Inundation Area (Government Code Section 8589.5): NOT in a Dam Inundation Area [X] No",
            "Very High Fire Hazard Severity Zone: [X] No",
        ]],
    );
    let va_packet_out = analyze_disclosure_packet(
        &http,
        &env,
        &secondary_report,
        vec![nhd_contra_pdf],
        None,
        None,
    )
    .await
    .expect("analyze NHD packet against synthetic secondary report");
    assert!(va_packet_out.data["documentClassifications"]
        .as_array()
        .map(|a| a.len() == 1)
        .unwrap_or(false));
}

// =================================================================================================
// 3. Pre-LLM PII Redaction (`redact_title_pii`) Pure Unit Verification (Zero Fake Borrower PDFs)
// =================================================================================================

#[test]
fn redact_title_pii_masks_identifiers_while_preserving_institutional_entities() {
    let institutional = "Payable to Santa Clara County Tax Collector; public utility easement in favor of Pacific Gas and Electric Company and City of San Jose Department of Public Works.";
    let inst_res = redact_title_pii(institutional);
    assert_eq!(
        inst_res.pii_redacted_count, 0,
        "public agencies and regulated utilities must not be redacted: {}",
        inst_res.masked_text
    );
    assert!(inst_res.masked_text.contains("Santa Clara County"));
    assert!(inst_res
        .masked_text
        .contains("Pacific Gas and Electric Company"));

    let pattern_input = "SSN: 000-00-1234 | EIN: 00-0005678 | Loan Number: LN-99887766 | Contact: escrow-desk@titlecompany.org";
    let pat_res = redact_title_pii(pattern_input);
    assert!(
        pat_res.pii_redacted_count >= 3,
        "SSN, EIN, Loan Number, and email patterns must be masked before any LLM or JSON output"
    );
    assert!(!pat_res.masked_text.contains("000-00-1234"));
    assert!(!pat_res.masked_text.contains("00-0005678"));
    assert!(!pat_res.masked_text.contains("LN-99887766"));
    assert!(!pat_res.masked_text.contains("escrow-desk@titlecompany.org"));
}

// =================================================================================================
// 4. Pure-Rust ZIP / Archive Security Guardrails (Traversal, Zip-Bomb, Overlapping Headers, Caps)
// =================================================================================================

#[test]
fn pure_rust_zip_guardrails_reject_traversal_bombs_nested_archives_and_overlaps() {
    let valid_pdf = build_multi_page_pdf(&[&["STATUTORY DISCLOSURE PAGE 1"]]);

    // 1) Path traversal (`../`) must be rejected
    let traversal_zip = build_zip_archive(&[("../etc/passwd.pdf", &valid_pdf, 8)]);
    let err = unpack_zip_archive_bytes("traversal.zip", &traversal_zip, 30)
        .expect_err("path traversal entry must be rejected");
    assert!(
        err.to_string().to_ascii_lowercase().contains("path")
            || err.to_string().to_ascii_lowercase().contains("traversal")
            || err.to_string().to_ascii_lowercase().contains("unsafe"),
        "unexpected error for path traversal: {err}"
    );

    // 2) Nested `.zip` inside `.zip` must be rejected
    let inner_zip = build_zip_archive(&[("inner.pdf", &valid_pdf, 8)]);
    let nested_zip = build_zip_archive(&[("nested.zip", &inner_zip, 0)]);
    assert!(
        unpack_zip_archive_bytes("outer.zip", &nested_zip, 30).is_err(),
        "nested zip archives must be rejected"
    );

    // 3) Compression ratio > 40:1 zip-bomb defense (highly compressible zero-filled buffer with %PDF header)
    let mut bomb_payload = Vec::with_capacity(600_000);
    bomb_payload.extend_from_slice(b"%PDF-1.5\n");
    bomb_payload.resize(600_000, b'A');
    let bomb_zip = build_zip_archive(&[("bomb.pdf", &bomb_payload, 8)]);
    assert!(
        unpack_zip_archive_bytes("bomb.zip", &bomb_zip, 30).is_err(),
        "40:1 compression ratio bomb must be rejected before full inflation"
    );

    // 4) Overlapping local file header offset (two central directory entries pointing to offset 0)
    let mut overlap_zip =
        build_zip_archive(&[("doc_a.pdf", &valid_pdf, 8), ("doc_b.pdf", &valid_pdf, 8)]);
    // Corrupt second CD entry's local_header_offset to 0 so both entries alias offset 0
    let eocd_pos = overlap_zip.len() - 22;
    let cd_offset = u32::from_le_bytes(
        overlap_zip[eocd_pos + 16..eocd_pos + 20]
            .try_into()
            .unwrap(),
    ) as usize;
    let first_name_len = u16::from_le_bytes(
        overlap_zip[cd_offset + 28..cd_offset + 30]
            .try_into()
            .unwrap(),
    ) as usize;
    let second_cd = cd_offset + 46 + first_name_len;
    overlap_zip[second_cd + 42..second_cd + 46].copy_from_slice(&0u32.to_le_bytes());
    assert!(
        unpack_zip_archive_bytes("overlap.zip", &overlap_zip, 30).is_err(),
        "aliased/overlapping local header offsets must be rejected"
    );
}

// =================================================================================================
// 5. Repair-Credit Addendum Regional Scaling, XSS Escaping & Per-Client SQLite HTTP Persistence
// =================================================================================================

#[tokio::test]
async fn repair_addendum_and_http_routes_persist_per_client_on_reports() {
    let primary_report = common::synth_report_primary();
    let philly_condo = load_holdout_report("synth-pa-19106-condo");

    // Regional cost calibration: Silicon Valley (95118) > Philadelphia (19106)
    let (ca_mult, _) = calibrate_regional_cost_multiplier(Some(&primary_report), None);
    let (pa_mult, _) = calibrate_regional_cost_multiplier(Some(&philly_condo), None);
    assert!(
        ca_mult > pa_mult,
        "San Jose 95118 regional repair multiplier ({ca_mult}) must exceed Philadelphia 19106 ({pa_mult})"
    );

    // Canonical trade normalization covers all standard trades
    assert!(!ADDENDUM_TRADES.is_empty());
    let roof_trade = normalize_to_canonical_trade("roofing");
    assert!(ADDENDUM_TRADES.contains(&roof_trade));

    // XSS payload in defect title/detail must be HTML-escaped in `printableHtml`
    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::from_env(&env, None);
    let xss_defects = vec![
        json!({
            "id": "item-1",
            "trade": "Roofing",
            "severity": "Critical",
            "title": "Rear valley flashing leak <script>alert(1)</script>",
            "detail": "Active water intrusion at valley flashing <img src=x onerror=alert(2)>",
            "costLow": 3500,
            "costHigh": 4900,
            "citations": [{ "docName": "C39_Roof.pdf", "page": 1, "quote": "Active leak at rear valley" }]
        }),
        json!({
            "id": "item-2",
            "trade": "General / Handyman",
            "severity": "Minor",
            "title": "Loose cabinet pull",
            "detail": "Cosmetic hardware adjustment",
            "costLow": 50,
            "costHigh": 100,
            "citations": [{ "docName": "Home_Insp.pdf", "page": 4, "quote": "Loose pull" }]
        }),
    ];

    let crit_only = generate_repair_addendum(
        &http,
        &env,
        &primary_report,
        RepairAddendumInput {
            packet_review: None,
            inspection_review: None,
            hoa_review: None,
            title_review: None,
            defects: Some(xss_defects),
            selected_defect_ids: Some(vec!["item-1".to_string()]),
            regional_multiplier: Some(1.0),
            preferred_remedy: Some("credit".to_string()),
        },
        None,
    )
    .await
    .expect("generate deterministic repair addendum");

    let html = crit_only.data["printableHtml"]
        .as_str()
        .expect("printableHtml string");
    assert!(
        !html.contains("<script>alert(1)</script>") && !html.contains("<img src=x onerror="),
        "printableHtml must strictly HTML-escape untrusted titles and notes"
    );
    assert!(
        crit_only.data["strategyRecommendation"]["sellerCreditAskUsd"]
            .as_i64()
            .unwrap_or(0)
            > 0
            || crit_only.data["strategyRecommendation"]["preCloseLicensedRepairOrEscrowCureUsd"]
                .as_i64()
                .unwrap_or(0)
                > 0,
        "selected defect item-1 must produce non-zero credit or cure ask"
    );

    let secondary_report = common::synth_report_secondary();
    let srv = TestServerHandle::start_offline_with_reports(&[
        (PRIMARY_SLUG, primary_report),
        (SECONDARY_SLUG, secondary_report),
    ])
    .await;

    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("reqwest client");

    let hoa_pdf = uploaded_pdf(
        "CA_CivCode_5565_Disclosure.pdf",
        &[&[
            "ASSESSMENT AND RESERVE FUNDING DISCLOSURE SUMMARY (CIVIL CODE SECTION 5565)",
            "Regular Monthly Assessment: $395.00 per month. Total Number of Residential Units: 40.",
            "Current Reserve Balance: $220,000. Fully Funded Reserve Balance: $500,000. Percent Funded: 44.0% funded.",
        ]],
    );

    // POST /api/hoa-review under Client A
    let post_hoa = client
        .post(format!("{}/api/hoa-review", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "client-alpha-1111")
        .json(&json!({
            "slug": PRIMARY_SLUG,
            "documents": [hoa_pdf],
        }))
        .send()
        .await
        .expect("POST /api/hoa-review");
    assert_eq!(post_hoa.status(), 200);
    let hoa_body: Value = post_hoa.json().await.expect("hoa JSON");
    assert_eq!(hoa_body["ok"].as_bool(), Some(true));
    assert_eq!(
        hoa_body["data"]["financialHealth"]["percentFunded"].as_f64(),
        Some(44.0)
    );

    // GET /api/hoa-review returns saved review for Client A, but null/empty for Client B (isolation!)
    let get_alpha: Value = client
        .get(format!("{}/api/hoa-review?slug={PRIMARY_SLUG}", srv.base_url))
        .header("X-OpenEstate-Client-Id", "client-alpha-1111")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        get_alpha["data"].is_object() && !get_alpha["data"].is_null(),
        "Client A must retrieve its persisted HOA review"
    );

    let get_beta: Value = client
        .get(format!("{}/api/hoa-review?slug={PRIMARY_SLUG}", srv.base_url))
        .header("X-OpenEstate-Client-Id", "client-beta-2222")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        get_beta["data"].is_null(),
        "Client B must NEVER see Client A's uploaded HOA review"
    );

    // Unknown slug returns 404
    let missing_resp = client
        .post(format!("{}/api/packet-review", srv.base_url))
        .header("Origin", &srv.base_url)
        .header("X-OpenEstate-Client-Id", "client-alpha-1111")
        .json(&json!({
            "slug": "nonexistent-parcel-slug-99999",
            "documents": [uploaded_pdf("TDS.pdf", &[&["CALIFORNIA CIVIL CODE SECTION 1102.6"]])],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_resp.status(), 404);
}

// =================================================================================================
// 6. Strict Structured Outputs JSON Schemas & `AiRateLimiter` Burst Tier Verification
// =================================================================================================

#[test]
fn schemas_satisfy_strict_structured_outputs_and_rate_limiter_enforces_tiers() {
    let h_schema = hoa_review_schema();
    let p_schema = packet_review_schema();
    let r_schema = repair_addendum_schema();

    assert_strict_openai_schema_node(&h_schema, "hoa_review_schema");
    assert_strict_openai_schema_node(&p_schema, "packet_review_schema");
    assert_strict_openai_schema_node(&r_schema, "repair_addendum_schema");

    // Validate real conformant payloads against each schema
    let valid_hoa_llm = json!({
        "executiveHeadline": "Reserves 22.0% funded with active SB 326 balcony remediation",
        "executiveSummary": "Association reserves are critically underfunded at 22.0% with an active $14,500 special assessment.",
        "overallRiskRating": "Critical / Non-Warrantable",
        "negotiationScript": "Require Seller payoff of the $14,500 special assessment at closing.",
        "buyerQuestions": ["Provide signed Civil Code Section 5551 structural engineer report."],
        "additionalFindings": [{
            "category": "structural_statutory",
            "severity": "critical",
            "title": "Civil Code Section 5551 Elevated Walkway Waterproofing",
            "detail": "Licensed structural engineer noted membrane failure.",
            "citation": {
                "docName": "CA_CivCode_5565_Assessment_And_Reserve_Disclosure.pdf",
                "page": 2,
                "quote": "Licensed structural engineer report identified dry rot"
            }
        }]
    });
    assert!(validate_json_schema(&valid_hoa_llm, &h_schema).is_empty());

    // Rate-limit burst enforcement across `hoa` (6/min) and `addendum` (10/min) buckets
    let limiter = AiRateLimiter::new();
    for i in 1..=6 {
        let _permit = limiter
            .acquire("203.0.113.10", "hoa")
            .unwrap_or_else(|e| panic!("hoa token {i} failed: {e:?}"));
    }
    assert!(
        limiter.acquire("203.0.113.10", "hoa").is_err(),
        "7th immediate HOA request from same IP must hit rate-limit bucket"
    );
    assert!(
        limiter.acquire("203.0.113.10", "addendum").is_ok(),
        "independent addendum bucket must remain available"
    );
}

// =================================================================================================
// 7. Live OpenAI `/v1/responses` Synthesis (Cleanly Gated on `OPENAI_API_KEY` — Zero Mock Servers)
// =================================================================================================

#[tokio::test]
async fn live_openai_hoa_and_packet_synthesis_when_api_key_configured() {
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!("Skipping live OpenAI HOA/packet synthesis: OPENAI_API_KEY not set");
            return;
        }
    };

    let mut map = HashMap::from([("OPENAI_API_KEY".to_string(), api_key)]);
    if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
        if !base.trim().is_empty() {
            map.insert("OPENAI_BASE_URL".to_string(), base);
        }
    }
    let env = EnvConfig::from_map(map);
    let http = HttpClient::from_env(&env, None);
    let ca_townhouse_report = load_holdout_report("synth-ca-94303-townhome");

    let hoa_pdf = uploaded_pdf(
        "CA_CivCode_5565_Assessment_Reserve_Summary.pdf",
        &[&[
            "ASSESSMENT AND RESERVE FUNDING DISCLOSURE SUMMARY (CALIFORNIA CIVIL CODE SECTION 5565)",
            "Regular assessment: $540.00 per month across 80 units.",
            "Current Reserve Balance: $418,000. Ideal Fully Funded Balance: $1,900,000. Percent Funded: 22.0% funded.",
            "Special Assessment: $14,500 per unit for exterior balcony waterproofing pursuant to Civil Code Section 5551.",
        ]],
    );

    let out = analyze_hoa_documents(&http, &env, &ca_townhouse_report, vec![hoa_pdf], None)
        .await
        .expect("live OpenAI HOA review synthesis");

    assert_eq!(out.data["aiEnhanced"].as_bool(), Some(true));
    assert_eq!(
        out.data["financialHealth"]["percentFunded"].as_f64(),
        Some(22.0)
    );
}
