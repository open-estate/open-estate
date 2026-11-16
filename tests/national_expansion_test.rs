//! Phase 7: National Data Expansion tests (Zero-Mock Edition)
//! - Multi-state EDFacts school metrics ingestion (CSV/TSV/ZIP, CCD crosswalk, range/bounded/suppressed cells, exact state isolation, non-CA neutral notices)
//! - Multi-region self-hosted OSRM routing (`OSRM_REGIONS_JSON`, region selection, fallbacks `no_osrm_region_configured` / `osrm_unavailable`, zero `router.project-osrm.org` calls)
//! - Municipal incident / Socrata open-data crime supplement (`CRIME_MUNICIPAL_ENDPOINTS_JSON`, `builtin_municipal_crime_endpoints`, radius filtering, live Socrata verification when online, factual-only output without "safe"/"unsafe")

mod common;

use open_estate::db::DbStore;
use open_estate::modules::civic::{
    apply_official_score, apply_school_metrics, assigned_official_schools,
    build_municipal_crime_url, builtin_municipal_crime_endpoints, fetch_municipal_crime_supplement,
    official_school_score, parse_municipal_endpoints, run_civic, select_municipal_endpoint,
    summarize_municipal_incidents, ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON,
};
use open_estate::modules::commute::{
    commute_times, EstimateReason, FallbackReason, LatLon, RoutingClient, RoutingConfig,
    ENV_OSRM_BASE_URL, ENV_OSRM_REGIONS_JSON,
};
use open_estate::modules::school_metrics::{
    build_edfacts_metric_rows, ca_metrics_for_report, edfacts_metric_source, ingest,
    missing_school_metrics_notice, normalize_nces_id, normalize_us_state, parse_ccd_directory,
    parse_edfacts_bytes, parse_edfacts_csv, parse_edfacts_pct, state_metrics_for_report,
    EdfactsValueKind, IngestOptions, SchoolLevel, NO_PUBLIC_SCHOOL_DATA_NOTICE,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::summary::summarize_report;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;

fn load_holdout_home(id: &str) -> Value {
    common::synth_holdout_by_id(id)
}

fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        let payload = enc.finish().unwrap();
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0]);
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&payload);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
        central.extend_from_slice(&8u16.to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0]);
        central.extend_from_slice(&crc.sum().to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "openestate_p7_{}_{}_{}",
        std::process::id(),
        name,
        bytes.len()
    ));
    std::fs::write(&p, bytes).unwrap();
    p
}

// ==========================================================================================
// 1. EDFacts Multi-State School Metrics
// ==========================================================================================

#[test]
fn edfacts_parses_ranges_bounded_estimates_and_privacy_tags_safely() {
    assert_eq!(parse_edfacts_pct("64").unwrap().value, 64.0);
    assert_eq!(
        parse_edfacts_pct("64.25%").unwrap().kind,
        EdfactsValueKind::Exact
    );

    // Midpoints for hyphen and en-dash ranges
    let r1 = parse_edfacts_pct("40-44").unwrap();
    assert_eq!(r1.value, 42.0);
    assert_eq!(r1.kind, EdfactsValueKind::RangeMidpoint);

    let r2 = parse_edfacts_pct("40–44").unwrap();
    assert_eq!(r2.value, 42.0);
    assert_eq!(r2.kind, EdfactsValueKind::RangeMidpoint);

    // Bounded estimates
    let ge = parse_edfacts_pct("GE80").unwrap();
    assert_eq!(ge.value, 89.5);
    assert_eq!(ge.kind, EdfactsValueKind::BoundedEstimate);

    let gte = parse_edfacts_pct(">=80").unwrap();
    assert_eq!(gte.value, 89.5);
    assert_eq!(gte.kind, EdfactsValueKind::BoundedEstimate);

    let gt = parse_edfacts_pct("GT90").unwrap();
    assert_eq!(gt.value, 95.0);
    assert_eq!(gt.kind, EdfactsValueKind::BoundedEstimate);

    let lt = parse_edfacts_pct("LT50").unwrap();
    assert_eq!(lt.value, 24.5);
    assert_eq!(lt.kind, EdfactsValueKind::BoundedEstimate);

    let le = parse_edfacts_pct("<=10").unwrap();
    assert_eq!(le.value, 5.0);
    assert_eq!(le.kind, EdfactsValueKind::BoundedEstimate);

    // Privacy tags & missing cells must skip safely without crashing
    for tag in [
        "PS",
        "ps",
        "S",
        "*",
        "N/A",
        "NA",
        ".",
        "-",
        "",
        "NULL",
        "SUPPRESSED",
        "garbage",
    ] {
        assert_eq!(parse_edfacts_pct(tag), None, "tag {tag:?} must skip");
    }

    assert_eq!(normalize_us_state("tx"), Some("TX"));
    assert_eq!(normalize_us_state("Texas"), Some("TX"));
    assert_eq!(normalize_us_state("48"), Some("TX"));
    assert_eq!(normalize_us_state("36"), Some("NY"));
    assert_eq!(normalize_us_state("53"), Some("WA"));
    assert_eq!(
        normalize_nces_id("64032006678").as_deref(),
        Some("064032006678")
    );
}

#[test]
fn edfacts_wide_and_ccd_crosswalk_enforce_exact_state_isolation() {
    // Texas schools have lower raw proficiency (30, 50, 70) than California (85, 95)
    // and must NEVER be ranked against California. Subgroup columns must be ignored.
    let edfacts_csv = "\
ST,NCESSCH,ST_SCHID,SCHNAM,SCHOOL_YEAR,ALL_RLA00PCTPROF,ALL_MTH00PCTPROF,MAM_RLA00PCTPROF
TX,,TX-227901-104,Barton Hills Elementary,2022-23,30,30,99
TX,480894000372,TX-227901-112,Casis Elementary,2022-23,40-44,58,99
TX,480894000394,TX-227901-125,Zilker Elementary,2022-23,GE80,50.5,99
TX,480894000360,TX-227901-002,Austin High School,2022-23,GT90,85,99
TX,480894000399,TX-227901-199,Pease Elementary,2022-23,PS,60,99
CA,062994004684,CA-4369641-01,Addison Elementary,2022-23,95,95,99
CA,062994004688,CA-4369641-02,Duveneck Elementary,2022-23,85,85,99
";
    let ccd_tsv = "\
NCESSCH\tST_SCHID\tSTABR\tSCHOOL_LEVEL\tGSLO\tGSHI
480894000366\tTX-227901-104\tTX\tElementary\tKG\t05
480894000372\tTX-227901-112\tTX\tElementary\tKG\t05
480894000394\tTX-227901-125\tTX\tElementary\tKG\t05
480894000360\tTX-227901-002\tTX\tHigh\t09\t12
062994004684\tCA-4369641-01\tCA\tElementary\tKG\t05
062994004688\tCA-4369641-02\tCA\tElementary\tKG\t05
";

    let dir = parse_ccd_directory(ccd_tsv).unwrap();
    let parsed_all = parse_edfacts_csv(edfacts_csv, None, Some(2023)).unwrap();
    assert_eq!(parsed_all.suppressed_cells, 1);
    assert_eq!(parsed_all.range_cells, 1);
    assert_eq!(parsed_all.bounded_cells, 2);

    let (rows, stats) = build_edfacts_metric_rows(&parsed_all, &dir, 2023, "2026-11-11T00:00:00Z");
    assert_eq!(stats.source, "EDFacts (2022-23)");
    assert_eq!(stats.school_year, "2022-23");

    // TX-227901-104 matched its NCES ID via ST_SCHID in CCD directory
    let tx1 = rows
        .iter()
        .find(|r| r.nces_id.as_deref() == Some("480894000366"))
        .unwrap();
    let tx2 = rows
        .iter()
        .find(|r| r.nces_id.as_deref() == Some("480894000372"))
        .unwrap();
    let tx3 = rows
        .iter()
        .find(|r| r.nces_id.as_deref() == Some("480894000394"))
        .unwrap();
    let tx4 = rows
        .iter()
        .find(|r| r.nces_id.as_deref() == Some("480894000360"))
        .unwrap();

    assert_eq!(tx2.ela_pct, Some(42.0)); // "40-44" -> 42.0
    assert_eq!(tx2.math_pct, Some(58.0));
    assert_eq!(tx2.combined_pct, 50.0);
    assert_eq!(tx3.ela_pct, Some(89.5)); // "GE80" -> 89.5
    assert_eq!(tx3.combined_pct, 70.0); // (89.5 + 50.5) / 2
    assert_eq!(tx4.ela_pct, Some(95.0)); // "GT90" -> 95.0

    // Exact state isolation: TX's 3 elementary schools (30, 50, 70) get 16.7, 50.0, 83.3
    // even though CA's elementary schools (85, 95) are higher and present in the same input.
    assert_eq!(tx1.percentile, 16.7);
    assert_eq!(tx2.percentile, 50.0);
    assert_eq!(tx3.percentile, 83.3);
    assert_eq!(tx4.percentile, 50.0, "only high school in TX high pool");
    assert_eq!(tx3.source, edfacts_metric_source(2023));

    // Suppressed school (ELA=PS, Math=60) has no combined metric -> omitted
    assert!(rows
        .iter()
        .all(|r| r.nces_id.as_deref() != Some("480894000399")));
}

#[tokio::test]
async fn edfacts_ingest_zip_and_multi_state_db_roundtrip_and_civic_scoring() {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();
    let env = EnvConfig::default();
    let http = HttpClient::new(None);

    // Pre-seed a CA row for 2023 to prove ingesting TX / NY / WA never touches CA rows.
    let ca_csv = "ST,NCESSCH,SCHNAM,LEVEL,ALL_RLA00PCTPROF,ALL_MTH00PCTPROF\nCA,062994004684,Addison Elementary,Elementary,90,90\n";
    let ca_parsed = parse_edfacts_csv(ca_csv, Some("CA"), Some(2023)).unwrap();
    let (ca_rows, _) =
        build_edfacts_metric_rows(&ca_parsed, &HashMap::new(), 2023, "2026-11-11T00:00:00Z");
    db.replace_school_metrics("CA", 2023, &ca_rows)
        .await
        .unwrap();

    // Long TSV files (RLA + Math separate) bundled in a ZIP for TX, NY, WA
    let rla_tsv = "ST\tNCESSCH\tSCHNAM\tLEVEL\tSUBJECT\tGRADE\tSUBGROUP\tPCTPROF\n\
                   TX\t480894000366\tBarton Hills Elementary\tElementary\tRLA\t00\tAll Students\t40–44\n\
                   TX\t480894000366\tBarton Hills Elementary\tElementary\tRLA\t00\tEcon Disadv\t10\n\
                   TX\t480894000409\tO Henry Middle School\tMiddle\tELA\t00\tAll Students\tGE80\n\
                   NY\t360009001925\tPS 321 William Penn\tElementary\tReading/Language Arts\t00\tAll Students\t70\n\
                   WA\t530771001209\tLincoln High School\tHigh\tELA\t00\tAll Students\t<=10\n";
    let math_tsv = "ST\tNCESSCH\tSCHNAM\tLEVEL\tSUBJECT\tGRADE\tSUBGROUP\tPCTPROF\n\
                    TX\t480894000366\tBarton Hills Elementary\tElementary\tMATH\t00\tAll Students\t58\n\
                    TX\t480894000409\tO Henry Middle School\tMiddle\tMTH\t00\tAll Students\t70.5\n\
                    NY\t360009001925\tPS 321 William Penn\tElementary\tMathematics\t00\tAll Students\t80\n\
                    WA\t530771001209\tLincoln High School\tHigh\tMATH\t00\tAll Students\tLT50\n";
    let zip_bytes = make_zip(&[
        ("rla_sch_2023.tsv", rla_tsv.as_bytes()),
        ("math_sch_2023.tsv", math_tsv.as_bytes()),
    ]);
    let (_, parsed_zip) = parse_edfacts_bytes(zip_bytes.clone(), Some("TX"), Some(2023)).unwrap();
    assert_eq!(parsed_zip.schools.len(), 2);

    let zip_path = temp_file("edfacts_multi.zip", &zip_bytes);
    let stats = ingest(
        &db,
        &http,
        &IngestOptions {
            state: "ALL".to_string(),
            year: 2023,
            file: Some(zip_path.clone()),
            directory_file: None,
            use_dashboard: false,
        },
    )
    .await
    .unwrap();
    let _ = std::fs::remove_file(&zip_path);

    assert_eq!(stats.rows_written, 4);
    assert_eq!(stats.source, "EDFacts (2022-23)");
    assert_eq!(stats.school_year, "2022-23");
    assert_eq!(
        db.count_school_metrics(Some("CA")).await.unwrap(),
        1,
        "CA rows preserved"
    );
    assert_eq!(db.count_school_metrics(Some("TX")).await.unwrap(), 2);
    assert_eq!(db.count_school_metrics(Some("NY")).await.unwrap(), 1);
    assert_eq!(db.count_school_metrics(Some("WA")).await.unwrap(), 1);

    // ca_metrics_for_report stays CA-only; state_metrics_for_report retrieves TX/NY/WA
    let tx_ids = vec!["480894000366".to_string(), "480894000409".to_string()];
    assert!(ca_metrics_for_report(Some(&db), &env, &tx_ids)
        .await
        .is_empty());
    let tx_metrics = state_metrics_for_report(Some(&db), &env, Some("TX"), &tx_ids).await;
    assert_eq!(tx_metrics.len(), 2);
    assert_eq!(tx_metrics["480894000366"].combined_pct, 50.0);
    assert_eq!(tx_metrics["480894000409"].combined_pct, 80.0);

    // WA school with <=10 (5.0) and LT50 (24.5)
    let wa_metrics =
        state_metrics_for_report(Some(&db), &env, Some("WA"), &["530771001209".to_string()]).await;
    assert_eq!(wa_metrics["530771001209"].ela_pct, Some(5.0));
    assert_eq!(wa_metrics["530771001209"].math_pct, Some(24.5));
    assert_eq!(wa_metrics["530771001209"].combined_pct, 14.75);

    // Apply to civic school cards and verify EDFacts badge, source, year, and no CAASPP key
    let mut cards = vec![
        json!({
            "name": "Barton Hills Elementary",
            "level": "elementary",
            "role": "Elementary",
            "ncesId": "480894000366",
            "gsRating": 9,
            "nicheGrade": "A+"
        }),
        json!({
            "name": "John Stanford International School",
            "level": "elementary",
            "role": "Elementary",
            "ncesId": "530771003198",
            "gsRating": 10,
            "nicheGrade": "A+"
        }),
    ];
    assert_eq!(apply_school_metrics(&mut cards, &tx_metrics), 1);
    for c in cards.iter_mut() {
        apply_official_score(c);
    }
    let barton = official_school_score(&cards[0]).unwrap();
    assert_eq!(barton.badge, "EDFacts");
    assert_eq!(barton.source, "EDFacts (2022-23)");
    assert_eq!(barton.year, "2022-23");
    assert_eq!(barton.score, 5.0);
    assert!(
        cards[0].get("caaspp").is_none(),
        "non-CA school must not carry caaspp key"
    );
    assert_eq!(cards[0]["edfacts"]["elaPct"], json!(42.0));
    assert!(cards[0]["scoreDetail"]
        .as_str()
        .unwrap()
        .contains("TX elementary pct."));

    // Unscored non-CA school stays strictly null (never falls back to GreatSchools/Niche)
    assert!(official_school_score(&cards[1]).is_none());
    assert!(cards[1]["score"].is_null());
    assert!(cards[1]["scoreSource"].is_null());
    assert!(cards[1]["scoreYear"].is_null());

    // Missing academic data notice for non-CA states
    assert_eq!(
        missing_school_metrics_notice(Some("WA")),
        Some(NO_PUBLIC_SCHOOL_DATA_NOTICE)
    );
    assert_eq!(missing_school_metrics_notice(Some("CA")), None);

    // Assigned schools + report summary on a synthetic Seattle holdout property stay clear of third-party ratings
    let sea_home = load_holdout_home("synth-wa-98103-adu-sfr");
    let civic_data = json!({
        "schools": {
            "assignedOrNearest": [cards[1].clone()]
        }
    });
    let assigned = assigned_official_schools(&civic_data);
    assert_eq!(assigned.summary_score(), None);
    let summary = summarize_report(&json!({
        "meta": { "displayAddress": sea_home["address"] },
        "geo": sea_home["raw"]["geo"],
        "modules": { "civic": { "data": civic_data } }
    }));
    let badges = summary["badges"].as_array().unwrap();
    assert!(!badges.iter().any(|b| b["k"] == "schools"));
}

#[test]
fn edfacts_level_infers_from_grades_when_directory_absent() {
    let csv = "\
ST,NCESSCH,SCHNAM,SUBJECT,GRADE,PCTPROF
WA,530771001146,Green Lake Elementary,ELA,03,70
WA,530771001146,Green Lake Elementary,MATH,03,80
WA,530771001146,Green Lake Elementary,ELA,04,60
WA,530771001146,Green Lake Elementary,MATH,04,*
WA,530771001209,Lincoln High School,ELA,11,50
WA,530771001209,Lincoln High School,MATH,11,70
";
    let parsed = parse_edfacts_csv(csv, Some("WA"), None).unwrap();
    let (rows, stats) =
        build_edfacts_metric_rows(&parsed, &HashMap::new(), 2023, "2026-11-11T00:00:00Z");
    assert_eq!(rows.len(), 2);
    assert_eq!(stats.by_level.get("elementary"), Some(&1));
    assert_eq!(stats.by_level.get("high"), Some(&1));
    let elem = rows
        .iter()
        .find(|r| r.nces_id.as_deref() == Some("530771001146"))
        .unwrap();
    assert_eq!(elem.level, SchoolLevel::Elementary.as_str());
    assert_eq!(
        elem.combined_pct, 75.0,
        "grade 4 skipped because math is suppressed *"
    );
}

// ==========================================================================================
// 2. Multi-Region Self-Hosted OSRM Routing (Zero Mock Servers)
// ==========================================================================================

#[tokio::test]
async fn multi_region_osrm_rejects_public_demo_and_handles_coverage_and_unavailable_fallbacks() {
    // Public demo server inside OSRM_REGIONS_JSON is dropped at config parse time and NEVER called.
    // Closed local port 127.0.0.1:9 represents an offline self-hosted regional OSRM instance without
    // spinning up any mock HTTP server.
    let regions_with_demo = json!([
        { "name": "ca", "bbox": [-124.5, 32.5, -114.1, 42.0], "baseUrl": "http://127.0.0.1:9/ca" },
        { "name": "wa", "bbox": [-124.9, 45.5, -116.9, 49.1], "baseUrl": "http://127.0.0.1:9/wa" },
        { "name": "bad-demo", "bbox": [-80.0, 25.0, -70.0, 45.0], "baseUrl": "https://router.project-osrm.org" }
    ])
    .to_string();

    let env = EnvConfig::from_map(HashMap::from([(
        ENV_OSRM_REGIONS_JSON.to_string(),
        regions_with_demo,
    )]));
    let cfg = RoutingConfig::from_env(&env);
    assert_eq!(
        cfg.regions.len(),
        2,
        "public OSRM demo server must be dropped from configured regions"
    );
    assert!(cfg
        .issues
        .iter()
        .any(|i| i.contains("public OSRM demo server")));
    assert!(cfg.is_enabled());

    // Use synthetic holdout home coordinates from `tests/common/mod.rs`:
    let sj_sfr = load_holdout_home("synth-ca-95118-arterial-sfr");
    let phl_row = load_holdout_home("synth-pa-19147-rowhouse");

    let sj_home = LatLon::new(
        sj_sfr["raw"]["geo"]["lat"].as_f64().unwrap(),
        sj_sfr["raw"]["geo"]["lon"].as_f64().unwrap(),
    );
    let cupertino_work = LatLon::new(37.3349, -122.0090);

    let phl_home = LatLon::new(
        phl_row["raw"]["geo"]["lat"].as_f64().unwrap(),
        phl_row["raw"]["geo"]["lon"].as_f64().unwrap(),
    );
    let phl_center_city = LatLon::new(39.9526, -75.1652);

    let http = HttpClient::from_env(&env, None);

    // 1. Mixed matrix across covered CA (offline router -> `osrm_unavailable`), uncovered PA
    //    (`no_osrm_region_configured`), and cross-region CA -> PA (`no_osrm_region_configured`).
    let mixed = RoutingClient::from_http(&http)
        .drive_matrix(&[cupertino_work, phl_center_city], &[sj_home, phl_home])
        .await;

    assert_eq!(mixed.routed, 0);
    assert_eq!(mixed.estimated, 4);

    // CA -> CA matched the `ca` bbox, attempted closed self-hosted port 127.0.0.1:9, and fell back cleanly
    let ca_cell = mixed.cells[0][0].unwrap();
    assert!(ca_cell.estimated);
    assert_eq!(ca_cell.reason, Some(EstimateReason::RouterDown));
    assert_eq!(
        ca_cell.fallback_reason,
        Some(FallbackReason::OsrmUnavailable)
    );
    assert_eq!(
        serde_json::to_value(ca_cell).unwrap()["fallbackReason"],
        json!("osrm_unavailable")
    );
    assert!(ca_cell.free_flow_min > 5.0 && ca_cell.distance_mi > 3.0);

    // PA -> PA has no configured region (since `bad-demo` was rejected!) -> `no_osrm_region_configured`
    let pa_cell = mixed.cells[1][1].unwrap();
    assert!(pa_cell.estimated);
    assert_eq!(pa_cell.reason, Some(EstimateReason::OutsideCoverage));
    assert_eq!(
        pa_cell.fallback_reason,
        Some(FallbackReason::NoOsrmRegionConfigured)
    );
    assert_eq!(
        serde_json::to_value(pa_cell).unwrap()["fallbackReason"],
        json!("no_osrm_region_configured")
    );

    // Cross-region Cupertino CA -> Philadelphia PA also reports `no_osrm_region_configured`
    let cross_cell = mixed.cells[0][1].unwrap();
    assert!(cross_cell.estimated);
    assert_eq!(
        cross_cell.fallback_reason,
        Some(FallbackReason::NoOsrmRegionConfigured)
    );

    // 2. When no OSRM regions or base URL are configured at all, `commute_times` uses road-distance
    //    estimates with `EstimateReason::RouterUnset` and zero outbound HTTP calls.
    let unset_http = HttpClient::from_env(&EnvConfig::default(), None);
    let unset_mat = commute_times(&unset_http, &[sj_home], &[cupertino_work]).await;
    assert_eq!(unset_mat.requests, 0);
    let unset_cell = unset_mat.times[0][0].unwrap();
    assert!(unset_cell.estimated);
    assert_eq!(unset_cell.reason, Some(EstimateReason::RouterUnset));
}

#[test]
fn multi_region_selects_most_specific_bbox_and_supports_base_url_fallback() {
    let regions = json!([
        { "name": "us-west", "bbox": [-125.0, 31.0, -102.0, 49.1], "baseUrl": "http://osrm-west:5000" },
        { "name": "ca", "bbox": [-124.5, 32.5, -114.1, 42.0], "baseUrl": "http://osrm-ca:5000" }
    ])
    .to_string();
    let env = EnvConfig::from_map(HashMap::from([
        (ENV_OSRM_REGIONS_JSON.to_string(), regions),
        (
            ENV_OSRM_BASE_URL.to_string(),
            "http://osrm-us:5000".to_string(),
        ),
    ]));
    let cfg = RoutingConfig::from_env(&env);

    let sj_sfr = load_holdout_home("synth-ca-95118-arterial-sfr");
    let campbell = load_holdout_home("synth-ca-95008-adu-sfr");
    let seattle = load_holdout_home("synth-wa-98103-adu-sfr");
    let cambridge = load_holdout_home("synth-ma-02139-condo");

    let sj = LatLon::new(
        sj_sfr["raw"]["geo"]["lat"].as_f64().unwrap(),
        sj_sfr["raw"]["geo"]["lon"].as_f64().unwrap(),
    );
    let camp = LatLon::new(
        campbell["raw"]["geo"]["lat"].as_f64().unwrap(),
        campbell["raw"]["geo"]["lon"].as_f64().unwrap(),
    );
    let sea = LatLon::new(
        seattle["raw"]["geo"]["lat"].as_f64().unwrap(),
        seattle["raw"]["geo"]["lon"].as_f64().unwrap(),
    );
    let bos = LatLon::new(
        cambridge["raw"]["geo"]["lat"].as_f64().unwrap(),
        cambridge["raw"]["geo"]["lon"].as_f64().unwrap(),
    );

    assert_eq!(
        cfg.select_region(sj, camp).unwrap().name,
        "ca",
        "smaller CA bbox wins over broader us-west bbox"
    );
    assert_eq!(cfg.resolve_endpoint(sj, camp), Some("http://osrm-ca:5000"));
    assert_eq!(cfg.resolve_endpoint(sj, sea), Some("http://osrm-west:5000"));
    assert_eq!(
        cfg.resolve_endpoint(sj, bos),
        Some("http://osrm-us:5000"),
        "uncovered pairs fall back to OSRM_BASE_URL when set"
    );
}

// ==========================================================================================
// 3. Municipal Incident / Socrata Crime Open-Data Supplement (Zero Mock Servers)
// ==========================================================================================

#[test]
fn municipal_socrata_summarizer_radius_filter_and_factual_guardrails() {
    let eps = builtin_municipal_crime_endpoints();
    assert!(eps.len() >= 5);
    for ep in &eps {
        let city = ep.city.as_deref().unwrap();
        let state = ep.state.as_deref().unwrap();
        let geo = json!({ "city": city, "state": state });
        assert_eq!(
            select_municipal_endpoint(&eps, &geo, 0.0, 0.0).map(|e| e.endpoint.as_str()),
            Some(ep.endpoint.as_str())
        );
        let url = build_municipal_crime_url(ep, 37.77, -122.42, "2025-11-11").unwrap();
        assert!(url.starts_with("https://"));
        assert!(url.contains("%24limit=500") || url.contains("$limit=500"));
    }

    // Invalid JSON in CRIME_MUNICIPAL_ENDPOINTS_JSON emits warning and empty vec without panicking
    let bad_env = EnvConfig::from_map(HashMap::from([(
        ENV_CRIME_MUNICIPAL_ENDPOINTS_JSON.to_string(),
        "not json".to_string(),
    )]));
    let (parsed_bad, warns) = parse_municipal_endpoints(&bad_env);
    assert!(parsed_bad.is_empty());
    assert_eq!(warns.len(), 1);

    // Select the built-in Seattle Police Department Socrata endpoint for our synthetic Seattle holdout home
    let seattle_home = load_holdout_home("synth-wa-98103-adu-sfr");
    let sea_geo = &seattle_home["raw"]["geo"];
    let sea_lat = sea_geo["lat"].as_f64().unwrap();
    let sea_lon = sea_geo["lon"].as_f64().unwrap();

    let sea_ep = select_municipal_endpoint(&eps, sea_geo, sea_lat, sea_lon)
        .expect("built-in Seattle Socrata endpoint");

    // Verify deterministic incident classification, radius filtering, and Fair Housing wording guardrails
    // directly via `summarize_municipal_incidents` using the real Seattle endpoint field schema.
    let cat_field = sea_ep
        .category_field
        .as_deref()
        .unwrap_or("offense_parent_group");
    let date_field = sea_ep
        .date_field
        .as_deref()
        .unwrap_or("offense_start_datetime");
    let lat_field = sea_ep.lat_field.as_deref().unwrap_or("latitude");
    let lon_field = sea_ep.lon_field.as_deref().unwrap_or("longitude");

    let mut r1 = serde_json::Map::new();
    r1.insert(date_field.into(), json!("2026-02-14T10:00:00"));
    r1.insert(cat_field.into(), json!("BURGLARY-FORCE-RES"));
    r1.insert(lat_field.into(), json!(format!("{:.6}", sea_lat + 0.001)));
    r1.insert(lon_field.into(), json!(format!("{:.6}", sea_lon + 0.001)));

    let mut r2 = serde_json::Map::new();
    r2.insert(date_field.into(), json!("2026-03-01T21:15:00"));
    r2.insert(cat_field.into(), json!("AGGRAVATED ASSAULT"));
    r2.insert(lat_field.into(), json!(format!("{:.6}", sea_lat + 0.0015)));
    r2.insert(lon_field.into(), json!(format!("{:.6}", sea_lon - 0.001)));

    let mut r3 = serde_json::Map::new();
    r3.insert(date_field.into(), json!("2025-11-20T08:00:00"));
    r3.insert(cat_field.into(), json!("LARCENY-THEFT"));
    r3.insert(lat_field.into(), json!(format!("{:.6}", sea_lat - 0.001)));
    r3.insert(lon_field.into(), json!(format!("{:.6}", sea_lon + 0.0005)));

    let mut r4 = serde_json::Map::new();
    r4.insert(date_field.into(), json!("2026-01-05T12:00:00"));
    r4.insert(cat_field.into(), json!("TRAFFIC VIOLATION"));
    r4.insert(lat_field.into(), json!(format!("{:.6}", sea_lat - 0.0008)));
    r4.insert(lon_field.into(), json!(format!("{:.6}", sea_lon - 0.0008)));

    // > 10 miles away in South Seattle -> must be filtered out by radius check
    let mut r_far = serde_json::Map::new();
    r_far.insert(date_field.into(), json!("2026-01-10T12:00:00"));
    r_far.insert(cat_field.into(), json!("ROBBERY"));
    r_far.insert(lat_field.into(), json!("47.500000"));
    r_far.insert(lon_field.into(), json!("-122.250000"));

    let rows = vec![
        Value::Object(r1),
        Value::Object(r2),
        Value::Object(r3),
        Value::Object(r4),
        Value::Object(r_far),
    ];

    let supp = summarize_municipal_incidents(sea_ep, &rows, sea_lat, sea_lon, sea_geo);
    assert_eq!(supp["totalIncidents"], json!(4));
    assert_eq!(supp["violentIncidents"], json!(1));
    assert_eq!(supp["propertyIncidents"], json!(2));
    assert_eq!(supp["otherIncidents"], json!(1));
    assert_eq!(supp["year"], json!(2026));
    assert_eq!(supp["period"], json!("2025–2026"));

    // Strictly factual check: NEVER emit "safe" or "unsafe" anywhere in output
    let serialized = serde_json::to_string(&supp).unwrap();
    let forbidden = Regex::new(r"(?i)\b(?:un)?safe\b").unwrap();
    assert!(
        !forbidden.is_match(&serialized),
        "municipal crime supplement must never label an area safe/unsafe: {serialized}"
    );

    // Unlisted city returns `None` from `select_municipal_endpoint`
    let campbell = load_holdout_home("synth-ca-95008-adu-sfr");
    assert!(
        select_municipal_endpoint(&eps, &campbell["raw"]["geo"], 37.282000, -121.954000).is_none()
    );
}

#[tokio::test]
async fn run_civic_non_ca_missing_schools_and_live_socrata_factual_guardrails() {
    let seattle_home = load_holdout_home("synth-wa-98103-adu-sfr");
    let sea_geo = seattle_home["raw"]["geo"].clone();
    let sea_lat = sea_geo["lat"].as_f64().unwrap();
    let sea_lon = sea_geo["lon"].as_f64().unwrap();

    // 1. Offline / air-gapped verification (dead outbound proxy 127.0.0.1:9, zero mock server):
    //    `run_civic` on a non-CA property without `school_metrics` emits the explicit neutral
    //    `NO_PUBLIC_SCHOOL_DATA_NOTICE`, leaves `summaryScore` null, and never uses "safe"/"unsafe".
    let offline_env = EnvConfig::from_map(HashMap::from([(
        "OUTBOUND_PROXY_URLS".to_string(),
        "http://127.0.0.1:9".to_string(),
    )]));
    let offline_http = HttpClient::from_env(&offline_env, None);
    let offline_out = run_civic(&offline_http, &offline_env, sea_lat, sea_lon, &sea_geo)
        .await
        .unwrap();

    assert_eq!(
        offline_out.data["schools"]["academicDataNotice"],
        json!(NO_PUBLIC_SCHOOL_DATA_NOTICE)
    );
    assert!(offline_out.data["schools"]["summaryScore"].is_null());
    assert!(offline_out.data["highlights"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["text"]
            .as_str()
            .unwrap_or("")
            .contains(NO_PUBLIC_SCHOOL_DATA_NOTICE)));

    let forbidden = Regex::new(r"(?i)\b(?:un)?safe\b").unwrap();
    let offline_str = serde_json::to_string(&offline_out).unwrap();
    assert!(
        !forbidden.is_match(&offline_str),
        "offline civic output used forbidden safe/unsafe wording: {offline_str}"
    );

    // 2. Live keyless Socrata verification against built-in open-data portal (`data.seattle.gov`)
    //    for synthetic Seattle coordinates. Skips cleanly only if upstream is unreachable.
    let live_env = EnvConfig::default();
    let live_http = HttpClient::from_env(&live_env, None);
    match fetch_municipal_crime_supplement(&live_http, &live_env, &sea_geo, sea_lat, sea_lon).await
    {
        Ok(Some((supp, src_url, _warns))) => {
            assert!(src_url.starts_with("https://"));
            assert!(supp["totalIncidents"].as_i64().unwrap_or(-1) >= 0);
            let live_str = serde_json::to_string(&supp).unwrap();
            assert!(
                !forbidden.is_match(&live_str),
                "live Socrata supplement used forbidden safe/unsafe wording: {live_str}"
            );
        }
        Ok(None) => panic!("built-in Seattle Socrata endpoint must match Seattle geo"),
        Err(err) => {
            eprintln!("Skipping live Socrata crime portal assertion (portal unreachable: {err})");
        }
    }
}

#[tokio::test]
async fn edfacts_ingest_rejects_invalid_state_or_year_mismatch() {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();
    let http = HttpClient::new(None);

    let err_state = ingest(
        &db,
        &http,
        &IngestOptions {
            state: "XX".to_string(),
            year: 2023,
            file: None,
            directory_file: None,
            use_dashboard: false,
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err_state.contains("unsupported --state"), "{err_state}");

    let csv_2022 =
        "ST,NCESSCH,LEVEL,SCHOOL_YEAR,ALL_RLA00PCTPROF,ALL_MTH00PCTPROF\nTX,480894000366,Elementary,2021-22,60,60\n";
    let p = temp_file("year_mismatch.csv", csv_2022.as_bytes());
    let err_yr = ingest(
        &db,
        &http,
        &IngestOptions {
            state: "TX".to_string(),
            year: 2023,
            file: Some(p.clone()),
            directory_file: None,
            use_dashboard: false,
        },
    )
    .await
    .unwrap_err()
    .to_string();
    let _ = std::fs::remove_file(&p);
    assert!(err_yr.contains("2022"), "{err_yr}");
}
