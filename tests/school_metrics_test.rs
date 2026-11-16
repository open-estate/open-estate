//! California school metric integration tests using official CDE / CAASPP research file
//! layouts and synthetic 14-digit CDS & 12-digit NCES identifiers.

mod common;

use open_estate::db::{
    build_school_metrics_upsert, migrate_database, mysql_ddl, sqlite_ddl, DbStore, SchoolMetricRow,
    TABLE_NAMES,
};
use open_estate::modules::civic::{
    apply_official_score, apply_school_metrics, assigned_official_schools, official_school_score,
};
use open_estate::modules::school_metrics::{
    build_ca_metric_rows, ca_metric_source, ca_metrics_for_report, caaspp_text_from_bytes,
    find_caaspp_all_students_file, metric_year, parse_caaspp_research_file, parse_cde_directory,
    parse_dashboard_crosswalk, percentile_ranks, school_year_label, zip_entry_names, zip_extract,
    CrosswalkEntry, SchoolLevel,
};
use open_estate::search::{
    assign_schools_to_listing, extract_listings_from_report, get_zip_school_context, ListingInput,
};
use open_estate::shared::EnvConfig;
use open_estate::web::summary::{backfill_facts_schools, summarize_report};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;

const SOURCE_2026: &str = "CAASPP 2025–26, California Department of Education";

/// Official CAASPP 2025–26 caret-delimited layout (`\r\r\n` row terminators) on synthetic
/// California CDS identifiers (`43000010000101` Synth Alpha Elementary, `43000020000103`
/// Synth Alpha High, `43000010000102` Synth Alpha Middle), exercising state/county/district
/// aggregate filtering, student-group `128` filtering, grade `13` summary filtering, and `*`
/// privacy-suppressed cells.
fn synthetic_caaspp_2026_excerpt() -> String {
    let header = "County Code^District Code^District Name^School Code^School Name^Type ID^Filler^Test Year^Test Type^Test ID^Student Group ID^Grade^Total Students Enrolled^Total Students Tested^Mean Scale Score^Percentage Advanced^Percentage Proficient^Percentage Proficient and Above^Percentage Developing^Percentage Minimal";
    let row = |county: &str,
               district: &str,
               dist_name: &str,
               school: &str,
               name: &str,
               ty: u8,
               test: u8,
               group: u16,
               grade: u8,
               pct: &str| {
        format!("{county}^{district}^{dist_name}^{school}^{name}^{ty}^^2026^B^{test}^{group}^{grade}^108^105^2564.2^38.00^32.00^{pct}^18.00^12.00")
    };
    let mut lines = vec![header.to_string()];
    // State (`Type ID = 4`), County (`Type ID = 5`), and District (`Type ID = 6`) aggregates are ignored.
    lines.push(row(
        "00",
        "00000",
        "State of California",
        "0000000",
        "",
        4,
        1,
        1,
        3,
        "47.20",
    ));
    lines.push(row(
        "43",
        "00000",
        "Santa Clara County",
        "0000000",
        "",
        5,
        1,
        1,
        3,
        "61.40",
    ));
    lines.push(row(
        "43",
        "00001",
        "Synthetic Elementary District",
        "0000000",
        "",
        6,
        2,
        1,
        3,
        "74.10",
    ));
    // Synth Alpha Elementary (CDS 43000010000101): grade 4 math suppressed (`*`) -> grade 4 dropped;
    // grade 13 summary ignored; student group 128 ignored.
    for (test, grade, pct) in [
        (1, 3, "72.00"),
        (2, 3, "76.00"),
        (1, 4, "78.00"),
        (2, 4, "*"),
        (1, 5, "80.00"),
        (2, 5, "84.00"),
        (1, 13, "76.68"),
        (2, 13, "79.62"),
    ] {
        lines.push(row(
            "43",
            "00001",
            "Synthetic Elementary District",
            "0000101",
            "Synth Alpha Elementary",
            7,
            test,
            1,
            grade,
            pct,
        ));
    }
    lines.push(row(
        "43",
        "00001",
        "Synthetic Elementary District",
        "0000101",
        "Synth Alpha Elementary",
        7,
        1,
        128,
        3,
        "52.00",
    ));
    // Synth Charter Elementary (CDS 43000030000104, Type ID 9 charter) — grade 5 tested.
    lines.push(row(
        "43",
        "00003",
        "Synthetic Charter District",
        "0000104",
        "Synth Charter Elementary",
        9,
        1,
        1,
        5,
        "68.00",
    ));
    lines.push(row(
        "43",
        "00003",
        "Synthetic Charter District",
        "0000104",
        "Synth Charter Elementary",
        9,
        2,
        1,
        5,
        "64.00",
    ));
    // Synth Alpha High (CDS 43000020000103) — grade 11 CAASPP only.
    lines.push(row(
        "43",
        "00002",
        "Synthetic High District",
        "0000103",
        "Synth Alpha High",
        7,
        1,
        1,
        11,
        "80.00",
    ));
    lines.push(row(
        "43",
        "00002",
        "Synthetic High District",
        "0000103",
        "Synth Alpha High",
        7,
        2,
        1,
        11,
        "58.00",
    ));
    // Synth Alpha Middle (CDS 43000010000102): cells suppressed or unreported -> no metric value.
    lines.push(row(
        "43",
        "00001",
        "Synthetic Elementary District",
        "0000102",
        "Synth Alpha Middle",
        7,
        1,
        1,
        6,
        "*",
    ));
    lines.push(row(
        "43",
        "00001",
        "Synthetic Elementary District",
        "0000102",
        "Synth Alpha Middle",
        7,
        2,
        1,
        6,
        "",
    ));
    lines.join("\r\r\n") + "\r\r\n"
}

#[test]
fn parses_caaspp_research_excerpt_with_suppressed_cells() {
    let parsed = parse_caaspp_research_file(&synthetic_caaspp_2026_excerpt()).expect("parse");
    assert_eq!(parsed.test_year, Some(2026));
    assert_eq!(parsed.schools.len(), 4, "{:?}", parsed.schools.keys());
    assert_eq!(parsed.suppressed_cells, 2);
    assert_eq!(parsed.missing_cells, 1);

    let alpha_elem = &parsed.schools["43000010000101"];
    assert_eq!(alpha_elem.type_id, Some(7));
    assert_eq!(alpha_elem.name.as_deref(), Some("Synth Alpha Elementary"));
    assert_eq!(
        alpha_elem.grades.keys().copied().collect::<Vec<_>>(),
        vec![3, 4, 5],
        "grade 13 summary row is excluded from per-grade average"
    );
    assert_eq!(
        alpha_elem.grades[&4],
        (Some(78.0), None),
        "suppressed math cell"
    );
    let vo = alpha_elem.value().expect("alpha_elem metric value");
    assert_eq!(vo.grades_used, 2, "grade 4 requires both ELA and math");
    assert_eq!(
        (vo.ela_pct, vo.math_pct, vo.combined_pct),
        (76.0, 80.0, 78.0)
    );

    let charter = parsed.schools["43000030000104"].value().unwrap();
    assert_eq!(
        (
            charter.ela_pct,
            charter.math_pct,
            charter.combined_pct,
            charter.grades_used
        ),
        (68.0, 64.0, 66.0, 1)
    );

    let alpha_high = parsed.schools["43000020000103"].value().unwrap();
    assert_eq!(
        (
            alpha_high.ela_pct,
            alpha_high.math_pct,
            alpha_high.combined_pct,
            alpha_high.grades_used
        ),
        (80.0, 58.0, 69.0, 1)
    );

    assert!(
        parsed.schools["43000010000102"].value().is_none(),
        "Synth Alpha Middle row with suppressed/blank cells produces None"
    );
}

#[test]
fn parses_the_pre_2026_layout_and_rejects_other_files() {
    // Pre-2026 CDE files use "Percentage Standard Met and Above" and omit Type ID (0000000 = aggregate).
    let text = "County Code^District Code^School Code^Filler^Test Year^Student Group ID^Test Type^Test ID^Grade^Percentage Standard Met and Above\n\
                43^00004^0000000^^2025^1^B^1^3^54.20\n\
                43^00004^0000201^^2025^1^B^1^3^61.50\n\
                43^00004^0000201^^2025^1^B^2^3^58.50\n";
    let parsed = parse_caaspp_research_file(text).unwrap();
    assert_eq!(parsed.test_year, Some(2025));
    assert_eq!(parsed.schools.len(), 1);
    assert_eq!(
        parsed.schools["43000040000201"]
            .value()
            .unwrap()
            .combined_pct,
        60.0
    );

    let fixed_width = "0000000000000004    2026State Of California\n";
    let err = parse_caaspp_research_file(fixed_width)
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing column"), "{err}");
}

#[test]
fn percentile_math() {
    assert_eq!(percentile_ranks(&[]), Vec::<f64>::new());
    assert_eq!(percentile_ranks(&[42.0]), vec![50.0]);
    assert_eq!(
        percentile_ranks(&[10.0, 20.0, 20.0, 30.0]),
        vec![12.5, 50.0, 50.0, 87.5]
    );
    assert_eq!(
        percentile_ranks(&[30.0, 10.0, 20.0]),
        vec![100.0 * 2.5 / 3.0, 100.0 * 0.5 / 3.0, 50.0]
    );
    let many: Vec<f64> = (0..1000).map(|i| i as f64).collect();
    let p = percentile_ranks(&many);
    assert_eq!(p[0], 0.05);
    assert_eq!(p[999], 99.95);
    assert!(p.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn labels_for_the_2025_26_results() {
    assert_eq!(school_year_label(2026), "2025–26");
    assert_eq!(metric_year(2026), "2025-26");
    assert_eq!(ca_metric_source(2026), SOURCE_2026);
    assert_eq!(metric_year(2000), "1999-00");
}

/// Official CDE `pubschls.txt` TSV column layout on synthetic school rows.
fn cde_directory_excerpt_tsv() -> String {
    [
        "CDSCode\tNCESDist\tNCESSchool\tStatusType\tSchool\tEILCode\tEILName\tGSoffered",
        "43000010000101\t0600001\t00101\tActive\tSynth Alpha Elementary\tELEM\tElementary\tK-5",
        "43000010000102\t0600001\t00102\tActive\tSynth Alpha Middle\tINTMIDJR\tIntermediate/Middle/Junior High\t6-8",
        "43000030000104\t0600003\t00104\tActive\tSynth Charter Elementary\tELEM\tElementary\tK-5",
        "43000020000103\tNo Data\tNo Data\tActive\tSynth Alpha High\tHS\tHigh School\t9-12",
        "43000050000105\t0600005\t00105\tActive\tSynth Combined Academy\tELEMHIGH\tElementary-High Combination\tK-12",
        "43000040000000\t0600004\tNo Data\tActive\tNo Data\tNo Data\tNo Data\tNo Data",
    ]
    .join("\r\n")
}

#[test]
fn school_code_mapping_and_levels() {
    let dir = parse_cde_directory(&cde_directory_excerpt_tsv()).unwrap();
    assert_eq!(
        dir["43000010000101"],
        CrosswalkEntry {
            nces_id: Some("060000100101".into()),
            level: Some(SchoolLevel::Elementary),
        }
    );
    assert_eq!(dir["43000010000102"].level, Some(SchoolLevel::Middle));
    assert_eq!(
        dir["43000030000104"].nces_id.as_deref(),
        Some("060000300104")
    );
    assert_eq!(
        dir["43000020000103"].nces_id, None,
        "\"No Data\" is not an NCES ID"
    );
    assert_eq!(dir["43000020000103"].level, Some(SchoolLevel::High));
    assert_eq!(dir["43000050000105"].level, Some(SchoolLevel::Combined));
    assert_eq!(
        dir["43000040000000"],
        CrosswalkEntry {
            nces_id: None,
            level: None,
        }
    );

    let dash = parse_dashboard_crosswalk(&[
        json!({ "attributes": { "CDSCode": "43000020000103", "FedID": "060000200103", "SchoolLevel": "High", "AAcount": 1620 } }),
        json!({ "CDSCode": "43000050000105", "FedID": "060000500105", "SchoolLevel": "Other" }),
        json!({ "attributes": { "CDSCode": "invalid", "FedID": "060000400201" } }),
        json!({ "attributes": { "CDSCode": "43000010000101", "FedID": "00101", "SchoolLevel": "Elementary" } }),
    ]);
    assert_eq!(dash.len(), 3);
    assert_eq!(
        dash["43000020000103"].nces_id.as_deref(),
        Some("060000200103")
    );
    assert_eq!(dash["43000050000105"].level, Some(SchoolLevel::Combined));
    assert_eq!(
        dash["43000010000101"].nces_id, None,
        "5-digit FedIDs are not 12-digit NCES school IDs"
    );

    let parsed = parse_caaspp_research_file(&synthetic_caaspp_2026_excerpt()).unwrap();
    let (rows, stats) = build_ca_metric_rows(&parsed, &dir, &dash, 2026, "2026-11-11T00:00:00Z");
    assert_eq!(stats.caaspp_schools, 4);
    assert_eq!(stats.schools_with_metric, 3);
    assert_eq!(stats.matched_by_directory, 2);
    assert_eq!(
        stats.matched_by_dashboard, 1,
        "Synth Alpha High maps NCES ID through the Dashboard crosswalk when directory omits NCESSchool"
    );
    assert_eq!(stats.matched_nces, 3);
    assert_eq!(stats.match_rate_pct, 100.0);
    assert_eq!(rows.len(), 3);

    let alpha_elem = rows
        .iter()
        .find(|r| r.cds_code.as_deref() == Some("43000010000101"))
        .unwrap();
    assert_eq!(alpha_elem.id, "CA-2026-43000010000101");
    assert_eq!(alpha_elem.nces_id.as_deref(), Some("060000100101"));
    assert_eq!(
        (alpha_elem.state.as_str(), alpha_elem.level.as_str(), alpha_elem.year),
        ("CA", "elementary", 2026)
    );
    assert_eq!(
        (alpha_elem.ela_pct, alpha_elem.math_pct, alpha_elem.combined_pct),
        (Some(76.0), Some(80.0), 78.0)
    );
    assert_eq!(
        alpha_elem.percentile, 75.0,
        "higher of two elementary schools (78.0 vs Charter 66.0)"
    );
    assert_eq!(alpha_elem.source, SOURCE_2026);

    let alpha_high = rows
        .iter()
        .find(|r| r.cds_code.as_deref() == Some("43000020000103"))
        .unwrap();
    assert_eq!(
        (alpha_high.level.as_str(), alpha_high.nces_id.as_deref()),
        ("high", Some("060000200103"))
    );

    // Without the Dashboard fallback, Synth Alpha High stays unmatched on NCES ID but is still ranked & stored.
    let (rows_no_dash, stats_no_dash) =
        build_ca_metric_rows(&parsed, &dir, &HashMap::new(), 2026, "t");
    assert_eq!(stats_no_dash.matched_nces, 2);
    assert_eq!(
        stats_no_dash.unmatched_examples,
        vec!["43000020000103 — Synth Alpha High".to_string()]
    );
    assert_eq!(
        rows_no_dash.iter().filter(|r| r.nces_id.is_none()).count(),
        1
    );

    // A school with no level in either directory or Dashboard is skipped cleanly.
    let mut dash_only_hs = HashMap::new();
    dash_only_hs.insert("43000020000103".to_string(), dash["43000020000103"].clone());
    let (rows_only_hs, stats_only_hs) =
        build_ca_metric_rows(&parsed, &HashMap::new(), &dash_only_hs, 2026, "t");
    assert_eq!(stats_only_hs.without_level, 2);
    assert_eq!(rows_only_hs.len(), 1);
    assert_eq!(rows_only_hs[0].cds_code.as_deref(), Some("43000020000103"));
}

#[test]
fn percentiles_are_within_level_across_synthetic_schools() {
    let schools = [
        ("43", "00004", "0000200", "0600004", "00200", "ELEM", 48.0),
        ("43", "00004", "0000201", "0600004", "00201", "ELEM", 60.0),
        ("43", "00004", "0000202", "0600004", "00202", "ELEM", 60.0),
        ("43", "00001", "0000101", "0600001", "00101", "ELEM", 78.0),
        ("43", "00002", "0000203", "0600002", "00203", "HS", 66.0),
        ("43", "00002", "0000103", "0600002", "00103", "HS", 74.0),
    ];

    let mut text = String::from(
        "County Code^District Code^School Code^Type ID^Test Year^Test Type^Test ID^Student Group ID^Grade^Percentage Proficient and Above\n",
    );
    let mut dir = String::from("CDSCode\tNCESDist\tNCESSchool\tEILCode\n");
    for (co, dist, sch, nces_dist, nces_sch, eil, pct) in schools {
        let grade = if eil == "HS" { 11 } else { 3 };
        for test in [1, 2] {
            text.push_str(&format!(
                "{co}^{dist}^{sch}^7^2026^B^{test}^1^{grade}^{pct:.2}\n"
            ));
        }
        dir.push_str(&format!(
            "{co}{dist}{sch}\t{nces_dist}\t{nces_sch}\t{eil}\n"
        ));
    }

    let parsed = parse_caaspp_research_file(&text).unwrap();
    let directory = parse_cde_directory(&dir).unwrap();
    let (rows, stats) = build_ca_metric_rows(&parsed, &directory, &HashMap::new(), 2026, "t");
    assert_eq!(stats.by_level.get("elementary"), Some(&4));
    assert_eq!(stats.by_level.get("high"), Some(&2));

    let pct_of = |nces: &str| {
        rows.iter()
            .find(|r| r.nces_id.as_deref() == Some(nces))
            .unwrap()
            .percentile
    };
    assert_eq!(pct_of("060000400200"), 12.5);
    assert_eq!(pct_of("060000400201"), 50.0);
    assert_eq!(pct_of("060000400202"), 50.0);
    assert_eq!(pct_of("060000100101"), 87.5);
    assert_eq!(pct_of("060000200203"), 25.0, "lower of two high schools");
    assert_eq!(pct_of("060000200103"), 75.0, "higher of two high schools");
}

/// Builds a ZIP archive in memory: one stored entry and one deflated entry.
fn make_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, deflate) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let payload = if *deflate {
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(data).unwrap();
            enc.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0]);
        out.extend_from_slice(&method.to_le_bytes());
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
        central.extend_from_slice(&method.to_le_bytes());
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

#[test]
fn reads_research_files_from_zip_archives() {
    let research = synthetic_caaspp_2026_excerpt();
    let zip = make_zip(&[
        (
            "sb_ca2026entities_csv.txt",
            b"County Code^District Code\r\n",
            false,
        ),
        ("sb_ca2026_1_csv_v1.txt", research.as_bytes(), true),
    ]);
    assert_eq!(
        zip_entry_names(&zip).unwrap(),
        vec!["sb_ca2026entities_csv.txt", "sb_ca2026_1_csv_v1.txt"]
    );
    let (name, text) = caaspp_text_from_bytes(zip.clone()).unwrap();
    assert_eq!(name.as_deref(), Some("sb_ca2026_1_csv_v1.txt"));
    assert_eq!(text, research);
    let (n, bytes) = zip_extract(&zip, |n| n.contains("entities")).unwrap();
    assert_eq!(
        (n.as_str(), bytes.as_slice()),
        (
            "sb_ca2026entities_csv.txt",
            b"County Code^District Code\r\n".as_slice()
        )
    );
    assert!(zip_extract(&zip, |n| n == "missing.txt").is_err());

    let mut bad = make_zip(&[("sb_ca2026_1_csv_v1.txt", b"County Code^District", false)]);
    let pos = bad.windows(6).position(|w| w == b"County").unwrap();
    bad[pos] = b'X';
    assert!(zip_extract(&bad, |_| true)
        .unwrap_err()
        .to_string()
        .contains("CRC"));

    let (none, same) = caaspp_text_from_bytes(research.clone().into_bytes()).unwrap();
    assert!(none.is_none());
    assert_eq!(same, research);

    let page = r#"<a href="https://caaspp-elpac.ets.org/caaspp/researchfiles/sb_ca2026_1_csv_v1.zip">v1</a>
                  <a href="https://caaspp-elpac.ets.org/caaspp/researchfiles/sb_ca2026_1_csv_v2.zip">v2</a>
                  <a href="https://caaspp-elpac.ets.org/caaspp/researchfiles/sb_ca2026_1_ascii_v3.zip">fixed width</a>
                  <a href="https://caaspp-elpac.ets.org/caaspp/researchfiles/sb_ca2026_all_csv_v4.zip">all groups</a>"#;
    assert_eq!(
        find_caaspp_all_students_file(page, 2026).as_deref(),
        Some("https://caaspp-elpac.ets.org/caaspp/researchfiles/sb_ca2026_1_csv_v2.zip")
    );
    assert_eq!(find_caaspp_all_students_file(page, 2025), None);
}

fn metric_row(cds: &str, nces: &str, level: &str, year: i64, percentile: f64) -> SchoolMetricRow {
    SchoolMetricRow {
        id: format!("CA-{year}-{cds}"),
        state: "CA".into(),
        nces_id: Some(nces.into()),
        cds_code: Some(cds.into()),
        level: level.into(),
        year,
        ela_pct: Some(61.5),
        math_pct: Some(58.25),
        combined_pct: 59.88,
        percentile,
        source: ca_metric_source(year),
        updated_at: "2026-11-11T00:00:00Z".into(),
    }
}

#[test]
fn school_metrics_table_ships_for_both_dialects() {
    assert!(TABLE_NAMES.contains(&"school_metrics"));
    assert_eq!(mysql_ddl().len(), TABLE_NAMES.len());
    let my = mysql_ddl()
        .iter()
        .find(|s| s.contains("CREATE TABLE IF NOT EXISTS school_metrics"))
        .expect("MySQL DDL");
    assert!(my.contains("PRIMARY KEY") && my.contains("INDEX idx_school_metrics_nces_year"));
    assert!(
        my.contains("LONGTEXT"),
        "HeatWave compatibility check requires LONGTEXT"
    );
    assert!(sqlite_ddl()
        .iter()
        .any(|s| s.contains("CREATE TABLE IF NOT EXISTS school_metrics")));
    assert!(sqlite_ddl()
        .iter()
        .any(|s| s.contains("idx_school_metrics_nces_year")));
    for col in [
        "state",
        "nces_id",
        "cds_code",
        "level",
        "year",
        "ela_pct",
        "math_pct",
        "combined_pct",
        "percentile",
        "source",
        "updated_at",
    ] {
        assert!(my.contains(&format!("{col} ")), "MySQL DDL lacks {col}");
    }
    let up = build_school_metrics_upsert("mysql", 2);
    assert!(
        up.contains("AS new_row ON DUPLICATE KEY UPDATE") && !up.contains("VALUES("),
        "{up}"
    );
    assert_eq!(up.matches('?').count(), 24);
    let up = build_school_metrics_upsert("sqlite", 3);
    assert!(up.contains("ON CONFLICT(id) DO UPDATE SET"), "{up}");
    assert_eq!(up.matches('?').count(), 36);
}

#[tokio::test]
async fn db_round_trip_on_in_memory_sqlite() {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();
    assert_eq!(db.count_school_metrics(None).await.unwrap(), 0);

    // 450 synthetic California school rows spanning three multi-row SQLite upsert chunks.
    let mut rows: Vec<SchoolMetricRow> = vec![
        metric_row("43000040000201", "060000400201", "elementary", 2026, 82.1),
        metric_row("43000040000202", "060000400202", "middle", 2026, 91.4),
        metric_row("43000020000203", "060000200203", "high", 2026, 86.6),
        metric_row("43000010000101", "060000100101", "elementary", 2026, 95.0),
        metric_row("43000010000102", "060000100102", "middle", 2026, 79.5),
        metric_row("43000020000103", "060000200103", "high", 2026, 84.2),
    ];
    for i in rows.len()..450 {
        let cds = format!("4300004{:07}", 1_000 + i);
        let nces = format!("0600004{:05}", 1_000 + i);
        rows.push(metric_row(
            &cds,
            &nces,
            "elementary",
            2026,
            (i % 100) as f64,
        ));
    }

    assert_eq!(
        db.replace_school_metrics("CA", 2026, &rows).await.unwrap(),
        450
    );
    assert_eq!(db.count_school_metrics(Some("CA")).await.unwrap(), 450);
    assert_eq!(db.count_school_metrics(Some("TX")).await.unwrap(), 0);

    let last_nces = rows[449].nces_id.clone().unwrap();
    let ids = vec![
        "060000400201".to_string(),
        last_nces.clone(),
        "069999999999".to_string(),
        "invalid".to_string(),
    ];
    let found = db.get_school_metrics_by_nces(&ids).await.unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found["060000400201"], rows[0]);
    assert_eq!(found[&last_nces].percentile, 49.0);

    // Latest year wins; older CAASPP years stay stored.
    let older = metric_row("43000040000201", "060000400201", "elementary", 2025, 78.0);
    db.upsert_school_metrics(&[older]).await.unwrap();
    assert_eq!(db.count_school_metrics(Some("CA")).await.unwrap(), 451);
    assert_eq!(
        db.get_school_metrics_by_nces(&ids[..1]).await.unwrap()["060000400201"].year,
        2026
    );

    // Upsert updates in place.
    let mut changed = rows[0].clone();
    changed.percentile = 88.5;
    db.upsert_school_metrics(&[changed]).await.unwrap();
    assert_eq!(
        db.get_school_metrics_by_nces(&ids[..1]).await.unwrap()["060000400201"].percentile,
        88.5
    );

    // Re-ingesting a year replaces it cleanly without touching other years.
    db.replace_school_metrics("CA", 2026, &rows[..6])
        .await
        .unwrap();
    assert_eq!(db.count_school_metrics(Some("CA")).await.unwrap(), 7);
    assert!(db
        .get_school_metrics_by_nces(std::slice::from_ref(&last_nces))
        .await
        .unwrap()
        .is_empty());

    // `migrate_database` copies `school_metrics` cleanly.
    let dst = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    let summary = migrate_database(&db, &dst).await.unwrap();
    assert_eq!(summary.tables.get("school_metrics"), Some(&7));
    assert_eq!(dst.count_school_metrics(Some("CA")).await.unwrap(), 7);
    assert_eq!(
        dst.get_school_metrics_by_nces(&ids[..1]).await.unwrap()["060000400201"],
        rows[0]
    );
}

#[tokio::test]
async fn report_lookup_reads_only_the_database() {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();
    let env = EnvConfig::default();
    let ids = vec!["060000100101".to_string()];
    assert!(ca_metrics_for_report(Some(&db), &env, &ids)
        .await
        .is_empty());
    assert!(ca_metrics_for_report(Some(&db), &env, &ids)
        .await
        .is_empty());
    db.replace_school_metrics(
        "CA",
        2026,
        &[metric_row(
            "43000010000101",
            "060000100101",
            "elementary",
            2026,
            95.0,
        )],
    )
    .await
    .unwrap();
    let found = ca_metrics_for_report(Some(&db), &env, &ids).await;
    assert_eq!(found["060000100101"].percentile, 95.0);

    // Verify lazy opening via `EnvConfig` (`SQLITE_DB_PATH`).
    let tmp_path = std::env::temp_dir().join(format!(
        "openestate_school_metrics_env_{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp_path);
    let path_str = tmp_path.to_string_lossy().to_string();
    let file_db = DbStore::connect_url(&path_str, Some("sqlite"))
        .await
        .unwrap();
    file_db
        .replace_school_metrics(
            "CA",
            2026,
            &[metric_row(
                "43000010000101",
                "060000100101",
                "elementary",
                2026,
                88.4,
            )],
        )
        .await
        .unwrap();
    drop(file_db);
    let file_env = EnvConfig::from_map(HashMap::from([("SQLITE_DB_PATH".into(), path_str)]));
    let env_found = ca_metrics_for_report(None, &file_env, &ids).await;
    assert_eq!(env_found["060000100101"].percentile, 88.4);
    let _ = std::fs::remove_file(&tmp_path);
}

fn synth_secondary_with_dashboard() -> Value {
    let mut report = common::synth_report_secondary();
    if let Some(comps) = report["facts"]["comps"].as_array_mut() {
        for c in comps.iter_mut() {
            c["source"] = json!("County Recorder");
        }
    }
    let cards = json!([
        {
            "name": "Synth Beta Elementary",
            "level": "Elementary",
            "role": "Elementary",
            "grades": "K-5",
            "type": "Public",
            "assigned": true,
            "ncesId": "060000400201",
            "distMi": 0.42,
            "dashboardUrl": "https://www.caschooldashboard.org/",
            "caDashboard": {
                "year": "2024-25",
                "elaTier": 4,
                "mathTier": 4,
                "sciTier": 4,
                "url": "https://www.caschooldashboard.org/"
            }
        },
        {
            "name": "Synth Beta Middle",
            "level": "Middle",
            "role": "Middle",
            "grades": "6-8",
            "type": "Public",
            "assigned": true,
            "ncesId": "060000400202",
            "distMi": 0.88,
            "score": 10.0,
            "scoreSource": "California School Dashboard (CDE)",
            "scoreYear": "2024-25",
            "caDashboard": {
                "year": "2024-25",
                "elaTier": 5,
                "mathTier": 5,
                "sciTier": 5,
                "url": "https://www.caschooldashboard.org/"
            }
        },
        {
            "name": "Synth Beta High",
            "level": "High",
            "role": "High",
            "grades": "9-12",
            "type": "Public",
            "assigned": true,
            "ncesId": "060000200203",
            "distMi": 1.35,
            "caDashboard": {
                "year": "2024-25",
                "elaTier": 4,
                "mathTier": 4,
                "sciTier": 4,
                "cciTier": 5,
                "gradTier": 4,
                "url": "https://www.caschooldashboard.org/"
            }
        }
    ]);
    report["modules"]["civic"]["data"]["schools"]["assignedOrNearest"] = cards;
    report
}

#[test]
fn reports_use_caaspp_percentiles_with_source_and_year() {
    let mut report = synth_secondary_with_dashboard();
    // Synth Beta Elementary (`060000400201`) and Synth Beta High (`060000200203`) carry CAASPP metrics;
    // Synth Beta Middle (`060000400202`) keeps its California School Dashboard score.
    let mut metrics = HashMap::new();
    metrics.insert(
        "060000400201".to_string(),
        metric_row("43000040000201", "060000400201", "elementary", 2026, 82.1),
    );
    metrics.insert(
        "060000200203".to_string(),
        metric_row("43000020000203", "060000200203", "high", 2026, 86.6),
    );
    let schools = &mut report["modules"]["civic"]["data"]["schools"];
    for key in ["assignedOrNearest"] {
        let list = schools[key].as_array_mut().unwrap();
        assert_eq!(apply_school_metrics(list, &metrics), 2);
        for s in list.iter_mut() {
            apply_official_score(s);
        }
    }
    let cards = schools["assignedOrNearest"].as_array().unwrap();
    let beta_elem = &cards[0];
    assert_eq!(beta_elem["name"], "Synth Beta Elementary");
    assert_eq!(beta_elem["percentile"], json!(82.1));
    assert_eq!(beta_elem["percentileLevel"], json!("elementary"));
    assert_eq!(beta_elem["metricSource"], json!(SOURCE_2026));
    assert_eq!(beta_elem["metricYear"], json!("2025-26"));
    assert_eq!(beta_elem["score"], json!(8.2));
    assert_eq!(beta_elem["scoreSource"], json!(SOURCE_2026));
    assert_eq!(beta_elem["scoreYear"], json!("2025-26"));
    assert_eq!(beta_elem["caaspp"]["elaPct"], json!(61.5));
    assert_eq!(
        beta_elem["caDashboard"]["elaTier"],
        json!(4),
        "Dashboard tiers stay on the report"
    );
    assert!(beta_elem["dashboardUrl"]
        .as_str()
        .unwrap_or("")
        .contains("caschooldashboard.org"));
    let beta_mid = &cards[1];
    assert!(beta_mid
        .get("percentile")
        .map(|v| v.is_null())
        .unwrap_or(true));
    assert_eq!(beta_mid["score"], json!(10.0));
    assert_eq!(
        beta_mid["scoreSource"],
        json!("California School Dashboard (CDE)")
    );
    assert_eq!(official_school_score(&cards[2]).unwrap().score, 8.7);

    let assigned = assigned_official_schools(&report["modules"]["civic"]["data"]);
    assert_eq!(
        assigned.summary_score(),
        Some(9.0),
        "(8.2 + 10.0 + 8.7) / 3 = 8.97"
    );
    assert_eq!(
        assigned.sources(),
        vec![
            SOURCE_2026.to_string(),
            "California School Dashboard (CDE)".to_string()
        ]
    );
    assert_eq!(
        assigned.years(),
        vec!["2025-26".to_string(), "2024-25".to_string()]
    );

    let badge = summarize_report(&report)["badges"]
        .as_array()
        .and_then(|arr| arr.iter().find(|b| b["k"] == "schools"))
        .cloned()
        .expect("schools badge");
    assert_eq!(badge["score"], json!(9.0));
    assert!(badge["source"].as_str().unwrap().contains("CAASPP 2025–26"));
    assert!(badge["year"].as_str().unwrap().contains("2025-26"));

    let mut facts = json!({});
    backfill_facts_schools(
        &mut facts,
        Some(&report["modules"]["civic"]["data"]),
        &report["geo"],
    );
    assert_eq!(facts["schools"]["summaryScore"], json!(9.0));
    assert_eq!(facts["schools"]["assigned"][0]["percentile"], json!(82.1));
    assert!(facts["schools"]["assigned"][0]["dashboardUrl"]
        .as_str()
        .is_some());

    let subject = &extract_listings_from_report(&report)[0];
    assert_eq!(subject.school_elem_rating, Some(8.2));
    assert_eq!(subject.school_mid_rating, Some(10.0));
    assert_eq!(subject.school_high_rating, Some(8.7));
    assert_eq!(subject.school_score, Some(9.0));
}

#[tokio::test]
async fn zip_synced_listings_use_the_same_metric() {
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();
    let legacy = json!([
        { "nces_id": "060000100101", "name": "Synth Alpha Elementary", "level": "elementary", "lat": 37.2550, "lon": -121.8990, "rating": 7.8, "rating_source": "nces_staffing_proxy" },
        { "nces_id": "060000200103", "name": "Synth Alpha High", "level": "high", "lat": 37.2600, "lon": -121.8970, "rating": 8.5, "rating_source": "nces_staffing_proxy" }
    ]);
    db.upsert_row(
        "zip_school_cache",
        &[
            ("zip", "95118".into()),
            ("lat", Some(37.2580).into()),
            ("lon", Some(-121.8980).into()),
            ("schools_json", legacy.to_string().into()),
            ("updated_at", open_estate::shared::now_iso().into()),
        ],
        &["zip"],
    )
    .await
    .unwrap();
    db.replace_school_metrics(
        "CA",
        2026,
        &[metric_row(
            "43000010000101",
            "060000100101",
            "elementary",
            2026,
            95.0,
        )],
    )
    .await
    .unwrap();

    let ctx = get_zip_school_context(&db, None, &EnvConfig::default(), "95118", None, None)
        .await
        .unwrap();
    let alpha_elem = ctx
        .schools
        .iter()
        .find(|s| s.name == "Synth Alpha Elementary")
        .unwrap();
    assert_eq!(
        alpha_elem.rating,
        Some(9.5),
        "percentile 95.0 ÷ 10, like the report"
    );
    assert_eq!(alpha_elem.rating_source.as_deref(), Some(SOURCE_2026));
    let alpha_high = ctx
        .schools
        .iter()
        .find(|s| s.name == "Synth Alpha High")
        .unwrap();
    assert_eq!(
        alpha_high.rating, None,
        "no metric -> unknown, never the old staffing proxy"
    );

    let mut item = ListingInput {
        address: common::SYNTH_PRIMARY_ADDRESS.into(),
        zip: "95118".into(),
        lat: Some(37.2580),
        lon: Some(-121.8980),
        ..Default::default()
    };
    assign_schools_to_listing(&mut item, Some(&ctx));
    assert_eq!(item.school_elem_rating, Some(9.5));
    assert_eq!(item.school_high_rating, None);
    assert_eq!(item.school_score, Some(9.5));
    assert_eq!(item.school_grade.as_deref(), Some("A+"));
}

#[test]
fn template_renders_caaspp_bilingual_labels_and_dashboard_links() {
    let tpl = std::fs::read_to_string(format!(
        "{}/assets/template.html",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    for required in [
        "CAASPP",
        "CA Dashboard ↗",
        "s.dashboardUrl || s.caDashboard?.url",
        "在加州${lvZh}学校中位列第 ${o.percentile} 百分位",
        "percentile among California",
        "SCH_LEVEL_ZH",
        "schoolSourceShort",
    ] {
        assert!(tpl.contains(required), "template missing {required}");
    }
}
