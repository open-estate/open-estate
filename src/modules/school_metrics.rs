//! Public school performance metrics: CAASPP for California and EDFacts for all other US states.
//!
//! Design: implementation plan v3, section 5.6 + Phase 7 (Beyond California).
//!
//! California (`schools::ingest --state CA --year 2026 [--file <path>]`):
//! 1. **Source.** The official CAASPP Smarter Balanced research file: statewide, All Students,
//!    caret-delimited (`sb_ca{year}_1_csv_v{n}.zip`), listed on the CAASPP research-file page. Cells for
//!    groups under 11 students are suppressed (`*`) and blank cells mean "not reported"; both count as missing.
//! 2. **School codes.** County + district + school code (the 14-digit CDS code) map to 12-digit NCES IDs
//!    through the CDE public schools directory (`NCESDist` + `NCESSchool`), falling back to the CDE School
//!    Dashboard layer (`FedID`). Only code and level columns are read from either source.
//! 3. **Metric.** All Students: mean over tested grades (3–8, 11) of (ELA % proficient-and-above + math %) ÷ 2.
//!    A grade counts only when both subjects are reported, so `combined = (ela + math) / 2`.
//! 4. **Level** from the directory's instruction level (elementary, middle, high). The directory's
//!    "Elementary-High Combination" schools (K–12, 6–12) form their own `combined` pool instead of being
//!    forced into one of the three.
//! 5. **Percentile** within the state, the year and the level: mid-rank, 0–100.
//! 6. **Storage** in `school_metrics`, replaced per state and year in one transaction. Reports read it by
//!    NCES ID; nothing is downloaded during a web request. An empty table means reports fall back to
//!    California School Dashboard tiers (logged once).
//!
//! National / EDFacts (`schools::ingest --state TX|NY|WA|EDFACTS|ALL --year 2023 --file <path>`):
//! 1. **Source.** Federal EDFacts school-level ELA/RLA + Math achievement CSV/TSV (or ZIP bundle) with
//!    NCES CCD directory mapping via `NCESSCH` / `ST_SCHID`.
//! 2. **Suppressed & range cells.** Closed ranges (`"40-44"`, `"40–44"`) map to their midpoint (`42.0`);
//!    bounded values (`"GE80"`/`">=80"` -> `89.5`, `"GT90"` -> `95.0`, `"LT50"` -> `24.5`, `"<=10"` -> `5.0`)
//!    use conservative midpoints; privacy tags (`"PS"`, `"S"`, `"*"`, `"N/A"`) are skipped without error.
//! 3. **Exact state isolation.** Within-state mid-rank percentiles by school level (`elementary`, `middle`,
//!    `high`, `combined`) are always computed strictly within a single state's pool.

use crate::db::{DbStore, SchoolMetricRow};
use crate::modules::civic::URL_CA_DASHBOARD;
use crate::shared::{now_iso, round_n, EnvConfig, HttpClient};
use anyhow::{anyhow, bail, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};

pub use crate::registry::endpoints::{
    CAASPP_FILES_BASE, CAASPP_LIST_URL, CDE_DIRECTORY_URL, EDFACTS_INFO_URL,
};
/// Optional env var pointing to a local/mirror EDFacts CSV/TSV/ZIP download URL.
pub const ENV_EDFACTS_FILE_URL: &str = "EDFACTS_FILE_URL";
/// Neutral notice when a non-CA property/school has no public achievement rows yet.
pub const NO_PUBLIC_SCHOOL_DATA_NOTICE: &str =
    "No public academic achievement data for this state/school yet";
/// California state abbreviation.
pub const CA_STATE: &str = "CA";
/// Rows per multi-row INSERT when storing metrics.
pub const SCHOOL_METRICS_BATCH: usize = 200;

/// School level used for the within-state percentile pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchoolLevel {
    Elementary,
    Middle,
    High,
    /// Elementary–high combination schools (K–12, 6–12, …), ranked among themselves.
    Combined,
}

impl SchoolLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Elementary => "elementary",
            Self::Middle => "middle",
            Self::High => "high",
            Self::Combined => "combined",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "elementary" => Some(Self::Elementary),
            "middle" => Some(Self::Middle),
            "high" => Some(Self::High),
            "combined" => Some(Self::Combined),
            _ => None,
        }
    }

    /// CDE directory `EILCode` (educational instruction level).
    pub fn from_cde_eil(code: &str) -> Option<Self> {
        match code.trim().to_ascii_uppercase().as_str() {
            "ELEM" => Some(Self::Elementary),
            "INTMIDJR" => Some(Self::Middle),
            "HS" => Some(Self::High),
            "ELEMHIGH" => Some(Self::Combined),
            _ => None,
        }
    }

    /// `SchoolLevel` of the CDE School Dashboard layer (NCES level names).
    pub fn from_dashboard(level: &str) -> Option<Self> {
        match level.trim().to_ascii_lowercase().as_str() {
            "elementary" | "primary" => Some(Self::Elementary),
            "middle" => Some(Self::Middle),
            "high" | "secondary" => Some(Self::High),
            "other" => Some(Self::Combined),
            _ => None,
        }
    }

    /// `SchoolLevel` from NCES CCD `SCHOOL_LEVEL` / `LEVEL` text or numeric code, with optional
    /// grade span fallback (`GSLO`, `GSHI`).
    pub fn from_ccd(level: Option<&str>, gslo: Option<&str>, gshi: Option<&str>) -> Option<Self> {
        if let Some(raw) = level {
            match raw.trim().to_ascii_lowercase().as_str() {
                "elementary" | "primary" | "elem" | "1" => return Some(Self::Elementary),
                "middle" | "middle/junior" | "junior high" | "2" => return Some(Self::Middle),
                "high" | "secondary" | "high school" | "3" => return Some(Self::High),
                "other" | "combined" | "elementary/secondary" | "k-12" | "4" => {
                    return Some(Self::Combined);
                }
                _ => {}
            }
        }
        fn parse_grade(g: Option<&str>) -> Option<i32> {
            let s = g?.trim().to_ascii_uppercase();
            match s.as_str() {
                "PK" | "PREK" | "PRE-K" => Some(-1),
                "K" | "KG" => Some(0),
                "" | "N" | "M" | "UG" | "AE" | "NO DATA" | "-1" | "-2" | "-9" => None,
                other => other.parse::<i32>().ok().filter(|n| (-1..=12).contains(n)),
            }
        }
        let (lo, hi) = (parse_grade(gslo)?, parse_grade(gshi)?);
        if hi <= 5 || (lo <= 3 && hi == 6) {
            Some(Self::Elementary)
        } else if lo >= 9 {
            Some(Self::High)
        } else if lo >= 5 && hi <= 9 {
            Some(Self::Middle)
        } else if lo <= 8 && hi >= 6 {
            Some(Self::Combined)
        } else {
            None
        }
    }
}

/// "2025–26" (en dash) for spring test year 2026.
pub fn school_year_label(year: i64) -> String {
    format!("{}–{:02}", year - 1, year.rem_euclid(100))
}

/// "2025-26" (hyphen): the `metricYear` value written on report school objects.
pub fn metric_year(year: i64) -> String {
    format!("{}-{:02}", year - 1, year.rem_euclid(100))
}

/// "CAASPP 2025–26, California Department of Education": the `metricSource` of California metrics.
pub fn ca_metric_source(year: i64) -> String {
    format!("CAASPP {}, California Department of Education", school_year_label(year))
}

/// "EDFacts (2022-23)": the `metricSource` of federal EDFacts multi-state metrics.
pub fn edfacts_metric_source(year: i64) -> String {
    format!("EDFacts ({})", metric_year(year))
}

/// Neutral notice emitted when a non-California property or school has no entries in `school_metrics`.
pub fn missing_school_metrics_notice(state: Option<&str>) -> Option<&'static str> {
    let st = state.unwrap_or("").trim();
    if st.is_empty() || st.eq_ignore_ascii_case(CA_STATE) {
        None
    } else {
        Some(NO_PUBLIC_SCHOOL_DATA_NOTICE)
    }
}

/// A 12-digit NCES school ID (`NCESSCH`).
pub fn is_nces_school_id(s: &str) -> bool {
    s.len() == 12 && s.bytes().all(|b| b.is_ascii_digit())
}

/// Normalizes an NCES school ID (`NCESSCH`), zero-padding 11-digit numeric values (when a spreadsheet
/// stripped a leading state FIPS zero, e.g. `"64032006678"` -> `"064032006678"`).
pub fn normalize_nces_id(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if t.len() == 12 {
        Some(t.to_string())
    } else if t.len() == 11 {
        Some(format!("0{t}"))
    } else {
        None
    }
}

/// Resolves a 2-letter US state abbreviation, full state name, or 2-digit FIPS code to its
/// canonical 2-letter uppercase abbreviation (`"TX"`, `"NY"`, `"WA"`, `"CA"`, ...).
pub fn normalize_us_state(raw: &str) -> Option<&'static str> {
    crate::registry::us_geo::normalize_us_state_abbr(raw)
}

/// Resolves the 2-letter state code from the first 2 digits of a normalized 12-digit NCES ID.
pub fn state_from_nces_id(nces_id: &str) -> Option<&'static str> {
    let norm = normalize_nces_id(nces_id)?;
    normalize_us_state(&norm[..2])
}

// ---------------------------------------------------------------- CAASPP research file

/// One school's All-Students Smarter Balanced results: grade → (ELA %, math %) proficient and above.
/// `None` means suppressed (`*`, fewer than 11 students) or not reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaasppSchool {
    /// 14-digit CDS code (county 2 + district 5 + school 7).
    pub cds_code: String,
    pub type_id: Option<u8>,
    pub name: Option<String>,
    pub grades: BTreeMap<u8, (Option<f64>, Option<f64>)>,
}

/// A school's All-Students value (percent of students at or above standard).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SchoolValue {
    pub ela_pct: f64,
    pub math_pct: f64,
    pub combined_pct: f64,
    pub grades_used: usize,
}

impl CaasppSchool {
    /// Mean over tested grades of (ELA % + math %) ÷ 2, using only grades where both subjects are
    /// reported. Values are rounded to two decimals; `None` when no grade qualifies.
    pub fn value(&self) -> Option<SchoolValue> {
        let both: Vec<(f64, f64)> = self
            .grades
            .values()
            .filter_map(|(e, m)| Some(((*e)?, (*m)?)))
            .collect();
        if both.is_empty() {
            return None;
        }
        let n = both.len() as f64;
        let ela = both.iter().map(|x| x.0).sum::<f64>() / n;
        let math = both.iter().map(|x| x.1).sum::<f64>() / n;
        let combined = both.iter().map(|(e, m)| (e + m) / 2.0).sum::<f64>() / n;
        Some(SchoolValue {
            ela_pct: round_n(ela, 2),
            math_pct: round_n(math, 2),
            combined_pct: round_n(combined, 2),
            grades_used: both.len(),
        })
    }
}

/// Result of parsing a caret-delimited research file.
#[derive(Debug, Clone, Default)]
pub struct CaasppParse {
    pub test_year: Option<i64>,
    /// Keyed by CDS code.
    pub schools: BTreeMap<String, CaasppSchool>,
    pub rows_read: usize,
    /// School-level, All Students, ELA/math, grade 3–8 or 11 rows.
    pub rows_used: usize,
    /// Cells marked `*` (groups under 11 students).
    pub suppressed_cells: usize,
    /// Empty or non-numeric cells.
    pub missing_cells: usize,
}

const GRADES: [u8; 7] = [3, 4, 5, 6, 7, 8, 11];
/// The 2025–26 files renamed the achievement levels ("Standard Met and Above" → "Proficient and Above").
const PCT_COLUMNS: [&str; 2] = ["Percentage Proficient and Above", "Percentage Standard Met and Above"];

fn lines_of(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n')
        .map(|l| l.trim_end_matches(['\r', '\n']))
        .filter(|l| !l.trim().is_empty())
}

fn find_col(headers: &[String], name: &str) -> Option<usize> {
    headers.iter().position(|h| h.eq_ignore_ascii_case(name))
}

fn digits(raw: &str, width: usize) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > width || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{t:0>width$}"))
}

/// Parses the statewide caret-delimited research file (any student-group file works; only All Students
/// rows are used). Rejects files without the expected header, e.g. the fixed-width variant.
pub fn parse_caaspp_research_file(text: &str) -> Result<CaasppParse> {
    let mut lines = lines_of(text);
    let header_line = lines.next().ok_or_else(|| anyhow!("empty CAASPP research file"))?;
    let headers: Vec<String> = header_line
        .trim_start_matches('\u{feff}')
        .split('^')
        .map(|h| h.trim().to_string())
        .collect();
    let need = |name: &str| -> Result<usize> {
        find_col(&headers, name).ok_or_else(|| {
            anyhow!("not a caret-delimited CAASPP Smarter Balanced research file: missing column {name:?}")
        })
    };
    let i_county = need("County Code")?;
    let i_district = need("District Code")?;
    let i_school = need("School Code")?;
    let i_year = need("Test Year")?;
    let i_test = need("Test ID")?;
    let i_group = need("Student Group ID")?;
    let i_grade = need("Grade")?;
    let i_pct = PCT_COLUMNS
        .iter()
        .find_map(|c| find_col(&headers, c))
        .ok_or_else(|| anyhow!("CAASPP research file has no {:?} column", PCT_COLUMNS[0]))?;
    let i_type = find_col(&headers, "Type ID");
    let i_test_type = find_col(&headers, "Test Type");
    let i_name = find_col(&headers, "School Name");
    let max_idx = [i_county, i_district, i_school, i_year, i_test, i_group, i_grade, i_pct]
        .into_iter()
        .chain(i_type)
        .chain(i_test_type)
        .max()
        .unwrap_or(0);

    let mut out = CaasppParse::default();
    for line in lines {
        out.rows_read += 1;
        let cells: Vec<&str> = line.split('^').collect();
        if cells.len() <= max_idx {
            continue;
        }
        let int = |i: usize| cells[i].trim().parse::<i64>().ok();
        if let Some(t) = i_test_type {
            if !cells[t].trim().eq_ignore_ascii_case("B") {
                continue;
            }
        }
        if int(i_group) != Some(1) {
            continue;
        }
        let subject = match int(i_test) {
            Some(1) => 0,
            Some(2) => 1,
            _ => continue,
        };
        let Some(grade) = int(i_grade).and_then(|g| u8::try_from(g).ok()).filter(|g| GRADES.contains(g)) else {
            continue;
        };
        let (Some(county), Some(district), Some(school)) =
            (digits(cells[i_county], 2), digits(cells[i_district], 5), digits(cells[i_school], 7))
        else {
            continue;
        };
        if school == "0000000" {
            continue; // district, county or state aggregate
        }
        let type_id = i_type.and_then(|t| cells[t].trim().parse::<u8>().ok());
        if i_type.is_some() && !matches!(type_id, Some(7) | Some(9) | Some(10)) {
            continue; // 7 school, 9 direct-funded charter, 10 locally funded charter
        }
        if out.test_year.is_none() {
            out.test_year = int(i_year);
        }
        let raw = cells[i_pct].trim();
        let pct = if raw == "*" {
            out.suppressed_cells += 1;
            None
        } else {
            match raw.parse::<f64>() {
                Ok(v) if (0.0..=100.0).contains(&v) => Some(v),
                _ => {
                    out.missing_cells += 1;
                    None
                }
            }
        };
        out.rows_used += 1;
        let cds = format!("{county}{district}{school}");
        let entry = out.schools.entry(cds.clone()).or_insert_with(|| CaasppSchool {
            cds_code: cds,
            type_id,
            name: i_name.map(|i| cells[i].trim().to_string()).filter(|s| !s.is_empty()),
            grades: BTreeMap::new(),
        });
        let slot = entry.grades.entry(grade).or_insert((None, None));
        if subject == 0 {
            slot.0 = pct;
        } else {
            slot.1 = pct;
        }
    }
    if out.schools.is_empty() {
        bail!("no school-level All Students rows found in the CAASPP research file");
    }
    Ok(out)
}

static CAASPP_FILE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"researchfiles/(sb_ca(\d{4})_1_csv_v(\d+)\.zip)").unwrap());

/// URL of the newest "statewide, All Students, caret-delimited" file for `year` linked from the
/// official research-file page, if the page lists one.
pub fn find_caaspp_all_students_file(list_html: &str, year: i64) -> Option<String> {
    CAASPP_FILE_RE
        .captures_iter(list_html)
        .filter(|c| c[2].parse::<i64>().ok() == Some(year))
        .max_by_key(|c| c[3].parse::<u32>().unwrap_or(0))
        .map(|c| format!("{CAASPP_FILES_BASE}{}", &c[1]))
}

// ---------------------------------------------------------------- School-code crosswalk

/// What an official directory says about one CDS code.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CrosswalkEntry {
    pub nces_id: Option<String>,
    pub level: Option<SchoolLevel>,
}

/// CDE public schools directory (`pubschls.txt`, tab-delimited). Reads only `CDSCode`, `NCESDist`,
/// `NCESSchool` and `EILCode`; no other column (names, contacts, …) is read.
pub fn parse_cde_directory(text: &str) -> Result<HashMap<String, CrosswalkEntry>> {
    let mut lines = lines_of(text);
    let header_line = lines.next().ok_or_else(|| anyhow!("empty CDE school directory"))?;
    let headers: Vec<String> = header_line
        .trim_start_matches('\u{feff}')
        .split('\t')
        .map(|h| h.trim().to_string())
        .collect();
    let col = |name: &str| -> Result<usize> {
        find_col(&headers, name).ok_or_else(|| anyhow!("CDE school directory: missing column {name:?}"))
    };
    let (i_cds, i_dist, i_sch, i_eil) = (col("CDSCode")?, col("NCESDist")?, col("NCESSchool")?, col("EILCode")?);
    let max_idx = i_cds.max(i_dist).max(i_sch).max(i_eil);
    let mut out = HashMap::new();
    for line in lines {
        let cells: Vec<&str> = line.split('\t').collect();
        if cells.len() <= max_idx {
            continue;
        }
        let Some(cds) = digits(cells[i_cds], 14) else { continue };
        let nces = format!("{}{}", cells[i_dist].trim(), cells[i_sch].trim());
        out.insert(
            cds,
            CrosswalkEntry {
                nces_id: Some(nces).filter(|n| is_nces_school_id(n) && cells[i_dist].trim().len() == 7),
                level: SchoolLevel::from_cde_eil(cells[i_eil]),
            },
        );
    }
    if out.is_empty() {
        bail!("CDE school directory has no rows");
    }
    Ok(out)
}

/// CDE School Dashboard layer rows (ArcGIS `features[]` or bare attribute objects). Reads only
/// `CDSCode`, `FedID` and `SchoolLevel`; the layer's demographic columns are never requested or read.
pub fn parse_dashboard_crosswalk(rows: &[Value]) -> HashMap<String, CrosswalkEntry> {
    let mut out = HashMap::new();
    for r in rows {
        let a = r.get("attributes").unwrap_or(r);
        let text = |k: &str| a.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_string());
        let Some(cds) = text("CDSCode").and_then(|c| digits(&c, 14)) else { continue };
        out.insert(
            cds,
            CrosswalkEntry {
                nces_id: text("FedID").filter(|f| is_nces_school_id(f)),
                level: text("SchoolLevel").and_then(|l| SchoolLevel::from_dashboard(&l)),
            },
        );
    }
    out
}

// ---------------------------------------------------------------- Percentiles and rows

/// Mid-rank percentile of each value within `values`, 0–100: `100 × (below + ½ × equal) ÷ n`.
/// Ties share a percentile; a single value gets 50. Output order matches input order.
pub fn percentile_ranks(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let mut out = vec![0.0; n];
    if n == 0 {
        return out;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut start = 0;
    while start < n {
        let mut end = start + 1;
        while end < n && values[order[end]] == values[order[start]] {
            end += 1;
        }
        let pct = 100.0 * (start as f64 + 0.5 * (end - start) as f64) / n as f64;
        for &i in &order[start..end] {
            out[i] = pct;
        }
        start = end;
    }
    out
}

/// Summary of one ingestion run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IngestStats {
    pub state: String,
    pub year: i64,
    pub school_year: String,
    pub source: String,
    pub source_file: String,
    pub rows_read: usize,
    pub rows_used: usize,
    pub suppressed_cells: usize,
    #[serde(default)]
    pub range_cells: usize,
    #[serde(default)]
    pub bounded_cells: usize,
    /// School entities (types 7, 9, 10) with any All Students row.
    pub caaspp_schools: usize,
    /// Schools with at least one grade where ELA and math are both reported.
    pub schools_with_metric: usize,
    /// Schools with a metric whose CDS code maps to an NCES ID.
    pub matched_nces: usize,
    pub matched_by_directory: usize,
    pub matched_by_dashboard: usize,
    /// `matched_nces ÷ schools_with_metric`, percent.
    pub match_rate_pct: f64,
    /// Schools with a metric but no known level (not ranked, not stored).
    pub without_level: usize,
    pub by_level: BTreeMap<String, usize>,
    pub rows_written: usize,
    pub directory_rows: usize,
    pub dashboard_rows: usize,
    /// Up to 15 "CDS code — name" of schools with a metric but no NCES ID.
    pub unmatched_examples: Vec<String>,
    pub warnings: Vec<String>,
}

/// Turns parsed CAASPP results and the crosswalks into `school_metrics` rows: one per school with a
/// metric and a level, ranked within its level. The directory wins over the Dashboard layer.
pub fn build_ca_metric_rows(
    parsed: &CaasppParse,
    directory: &HashMap<String, CrosswalkEntry>,
    dashboard: &HashMap<String, CrosswalkEntry>,
    year: i64,
    updated_at: &str,
) -> (Vec<SchoolMetricRow>, IngestStats) {
    let source = ca_metric_source(year);
    let mut stats = IngestStats {
        state: CA_STATE.to_string(),
        year,
        school_year: metric_year(year),
        source: source.clone(),
        rows_read: parsed.rows_read,
        rows_used: parsed.rows_used,
        suppressed_cells: parsed.suppressed_cells,
        caaspp_schools: parsed.schools.len(),
        directory_rows: directory.len(),
        dashboard_rows: dashboard.len(),
        ..Default::default()
    };

    struct Pending<'a> {
        cds: &'a str,
        nces: Option<String>,
        level: SchoolLevel,
        value: SchoolValue,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for (cds, school) in &parsed.schools {
        let Some(value) = school.value() else { continue };
        stats.schools_with_metric += 1;
        let d = directory.get(cds);
        let b = dashboard.get(cds);
        let nces = match (d.and_then(|e| e.nces_id.clone()), b.and_then(|e| e.nces_id.clone())) {
            (Some(n), _) => {
                stats.matched_by_directory += 1;
                Some(n)
            }
            (None, Some(n)) => {
                stats.matched_by_dashboard += 1;
                Some(n)
            }
            (None, None) => {
                if stats.unmatched_examples.len() < 15 {
                    stats.unmatched_examples.push(format!("{cds} — {}", school.name.as_deref().unwrap_or("?")));
                }
                None
            }
        };
        let Some(level) = d.and_then(|e| e.level).or_else(|| b.and_then(|e| e.level)) else {
            stats.without_level += 1;
            continue;
        };
        pending.push(Pending { cds, nces, level, value });
    }
    stats.matched_nces = stats.matched_by_directory + stats.matched_by_dashboard;
    stats.match_rate_pct = if stats.schools_with_metric == 0 {
        0.0
    } else {
        round_n(100.0 * stats.matched_nces as f64 / stats.schools_with_metric as f64, 2)
    };

    let mut by_level: BTreeMap<SchoolLevel, Vec<usize>> = BTreeMap::new();
    for (i, p) in pending.iter().enumerate() {
        by_level.entry(p.level).or_default().push(i);
    }
    let mut percentile = vec![0.0; pending.len()];
    for (level, idx) in &by_level {
        let values: Vec<f64> = idx.iter().map(|&i| pending[i].value.combined_pct).collect();
        for (k, p) in percentile_ranks(&values).into_iter().enumerate() {
            percentile[idx[k]] = p;
        }
        stats.by_level.insert(level.as_str().to_string(), idx.len());
    }

    let rows = pending
        .iter()
        .zip(percentile)
        .map(|(p, pct)| SchoolMetricRow {
            id: format!("{CA_STATE}-{year}-{}", p.cds),
            state: CA_STATE.to_string(),
            nces_id: p.nces.clone(),
            cds_code: Some(p.cds.to_string()),
            level: p.level.as_str().to_string(),
            year,
            ela_pct: Some(p.value.ela_pct),
            math_pct: Some(p.value.math_pct),
            combined_pct: p.value.combined_pct,
            percentile: round_n(pct, 1),
            source: source.clone(),
            updated_at: updated_at.to_string(),
        })
        .collect();
    (rows, stats)
}

// ---------------------------------------------------------------- ZIP and text helpers

const ZIP_MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

struct ZipEntry {
    name: String,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    local_offset: u64,
}

fn le16(b: &[u8], o: usize) -> Result<u16> {
    b.get(o..o + 2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .ok_or_else(|| anyhow!("truncated ZIP archive"))
}

fn le32(b: &[u8], o: usize) -> Result<u32> {
    b.get(o..o + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        .ok_or_else(|| anyhow!("truncated ZIP archive"))
}

fn zip_entries(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    if bytes.len() < 22 {
        bail!("not a ZIP archive");
    }
    let lowest = bytes.len().saturating_sub(22 + 65_535);
    let mut eocd = None;
    let mut i = bytes.len() - 22;
    loop {
        if bytes[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            eocd = Some(i);
            break;
        }
        if i == lowest {
            break;
        }
        i -= 1;
    }
    let e = eocd.ok_or_else(|| anyhow!("not a ZIP archive (no end-of-central-directory record)"))?;
    let count = le16(bytes, e + 10)?;
    let cd_offset = le32(bytes, e + 16)?;
    if count == u16::MAX || cd_offset == u32::MAX {
        bail!("ZIP64 archives are not supported");
    }
    let mut p = cd_offset as usize;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if le32(bytes, p)? != 0x0201_4b50 {
            bail!("corrupt ZIP central directory");
        }
        let (compressed, size, local) = (le32(bytes, p + 20)?, le32(bytes, p + 24)?, le32(bytes, p + 42)?);
        if compressed == u32::MAX || size == u32::MAX || local == u32::MAX {
            bail!("ZIP64 archives are not supported");
        }
        let name_len = le16(bytes, p + 28)? as usize;
        let extra_len = le16(bytes, p + 30)? as usize;
        let comment_len = le16(bytes, p + 32)? as usize;
        let name = bytes
            .get(p + 46..p + 46 + name_len)
            .ok_or_else(|| anyhow!("truncated ZIP archive"))?;
        out.push(ZipEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            method: le16(bytes, p + 10)?,
            crc: le32(bytes, p + 16)?,
            compressed: compressed as u64,
            size: size as u64,
            local_offset: local as u64,
        });
        p += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// File names in a ZIP archive held in memory.
pub fn zip_entry_names(bytes: &[u8]) -> Result<Vec<String>> {
    Ok(zip_entries(bytes)?.into_iter().map(|e| e.name).collect())
}

/// Extracts the first entry whose name satisfies `pick` from a ZIP archive held in memory (stored or
/// deflate, no ZIP64), checking size and CRC-32. Returns the entry name and its bytes.
pub fn zip_extract(bytes: &[u8], pick: impl Fn(&str) -> bool) -> Result<(String, Vec<u8>)> {
    let entries = zip_entries(bytes)?;
    let e = entries.iter().find(|e| pick(&e.name)).ok_or_else(|| {
        anyhow!(
            "no matching entry in ZIP archive (entries: {})",
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", ")
        )
    })?;
    if e.size > ZIP_MAX_ENTRY_BYTES {
        bail!("ZIP entry {} is too large ({} bytes)", e.name, e.size);
    }
    let lo = e.local_offset as usize;
    if le32(bytes, lo)? != 0x0403_4b50 {
        bail!("corrupt ZIP local header for {}", e.name);
    }
    let start = lo
        .checked_add(30)
        .and_then(|x| x.checked_add(le16(bytes, lo + 26).ok()? as usize))
        .and_then(|x| x.checked_add(le16(bytes, lo + 28).ok()? as usize))
        .ok_or_else(|| anyhow!("corrupt ZIP entry offset {}", e.name))?;
    let end = start
        .checked_add(e.compressed as usize)
        .ok_or_else(|| anyhow!("corrupt ZIP entry size {}", e.name))?;
    let data = bytes
        .get(start..end)
        .ok_or_else(|| anyhow!("truncated ZIP entry {}", e.name))?;
    let out = match e.method {
        0 => data.to_vec(),
        8 => {
            let mut v = Vec::with_capacity(e.size.min(64 * 1024 * 1024) as usize);
            flate2::read::DeflateDecoder::new(data)
                .take(ZIP_MAX_ENTRY_BYTES + 1)
                .read_to_end(&mut v)
                .with_context(|| format!("inflate {}", e.name))?;
            v
        }
        m => bail!("unsupported ZIP compression method {m} for {}", e.name),
    };
    if out.len() as u64 != e.size {
        bail!("ZIP entry {}: expected {} bytes, got {}", e.name, e.size, out.len());
    }
    let mut crc = flate2::Crc::new();
    crc.update(&out);
    if crc.sum() != e.crc {
        bail!("ZIP entry {}: CRC-32 mismatch", e.name);
    }
    Ok((e.name.clone(), out))
}

/// UTF-8 when valid, otherwise Latin-1 (CDE text files are Windows-1252/ISO-8859-1).
pub fn decode_text(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
    }
}

/// The research-file text from a downloaded or local file: a ZIP (first `.txt` entry that is not the
/// entities list) or the extracted text file itself.
pub fn caaspp_text_from_bytes(bytes: Vec<u8>) -> Result<(Option<String>, String)> {
    if bytes.starts_with(b"PK\x03\x04") {
        let (name, data) = zip_extract(&bytes, |n| {
            let l = n.to_ascii_lowercase();
            l.ends_with(".txt") && !l.contains("entities")
        })?;
        Ok((Some(name), decode_text(data)))
    } else {
        Ok((None, decode_text(bytes)))
    }
}

// ---------------------------------------------------------------- Ingestion / refresh

/// Options for refreshing `school_metrics` from official state/federal files.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub state: String,
    /// Spring test year, e.g. 2026 for 2025–26.
    pub year: i64,
    /// Local research file (ZIP or extracted caret-delimited text) instead of downloading it.
    pub file: Option<PathBuf>,
    /// Local copy of the CDE public schools directory instead of downloading it.
    pub directory_file: Option<PathBuf>,
    /// Also query the CDE School Dashboard layer for codes the directory cannot map.
    pub use_dashboard: bool,
}

async fn get_bytes(http: &HttpClient, url: &str, timeout_secs: u64) -> Result<Vec<u8>> {
    Ok(http
        .scrape_get(url)
        .timeout(Duration::from_secs(timeout_secs))
        .bytes()
        .await
        .with_context(|| format!("GET {url}"))?
        .to_vec())
}

async fn caaspp_file_url(http: &HttpClient, year: i64) -> String {
    let list = format!("{CAASPP_LIST_URL}?ps=true&lstTestYear={year}&lstTestType=B&lstCounty=00&lstDistrict=00000");
    if let Ok(html) = get_bytes(http, &list, 60).await {
        if let Some(url) = find_caaspp_all_students_file(&decode_text(html), year) {
            return url;
        }
    }
    format!("{CAASPP_FILES_BASE}sb_ca{year}_1_csv_v1.zip")
}

async fn fetch_dashboard_rows(http: &HttpClient) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let j = http
            .arcgis_query(
                URL_CA_DASHBOARD,
                &[
                    ("where", "1=1".to_string()),
                    // Codes and level only: never the layer's demographic columns.
                    ("outFields", "CDSCode,FedID,SchoolLevel".to_string()),
                    ("returnGeometry", "false".to_string()),
                    ("orderByFields", "OBJECTID".to_string()),
                    ("resultOffset", offset.to_string()),
                    ("resultRecordCount", "2000".to_string()),
                ],
                60_000,
            )
            .await?;
        let feats = j.get("features").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let n = feats.len();
        out.extend(feats);
        if n == 0 || !j.get("exceededTransferLimit").and_then(|v| v.as_bool()).unwrap_or(false) || offset > 100_000 {
            break;
        }
        offset += n;
    }
    Ok(out)
}

/// Downloads (or reads) the official files, computes the metrics and replaces the state's year in
/// `school_metrics`. Never called while serving a web request.
pub async fn ingest(db: &DbStore, http: &HttpClient, opts: &IngestOptions) -> Result<IngestStats> {
    let raw_state = opts.state.trim().to_ascii_uppercase();
    if !(2015..=2100).contains(&opts.year) {
        bail!("--year is the spring test year, e.g. 2026 for the 2025–26 results");
    }
    if raw_state == CA_STATE {
        return ingest_ca(db, http, opts).await;
    }
    let target_state = if raw_state == "EDFACTS" || raw_state == "ALL" {
        None
    } else if let Some(st) = normalize_us_state(&raw_state) {
        Some(st)
    } else {
        bail!("unsupported --state {raw_state:?}; expected a 2-letter US state code, \"EDFACTS\", or \"ALL\"");
    };
    ingest_edfacts(db, http, opts, &raw_state, target_state).await
}

async fn ingest_ca(db: &DbStore, http: &HttpClient, opts: &IngestOptions) -> Result<IngestStats> {
    let state = CA_STATE.to_string();
    let mut warnings = Vec::new();

    let (source_file, bytes) = match &opts.file {
        Some(p) => (
            p.display().to_string(),
            std::fs::read(p).with_context(|| format!("read {}", p.display()))?,
        ),
        None => {
            let url = caaspp_file_url(http, opts.year).await;
            let bytes = get_bytes(http, &url, 600).await?;
            (url, bytes)
        }
    };
    let (entry, text) = caaspp_text_from_bytes(bytes)?;
    let parsed = parse_caaspp_research_file(&text)?;
    if let Some(y) = parsed.test_year {
        if y != opts.year {
            bail!("{source_file} holds test year {y}, not --year {}", opts.year);
        }
    }

    let directory = match &opts.directory_file {
        Some(p) => parse_cde_directory(&decode_text(
            std::fs::read(p).with_context(|| format!("read {}", p.display()))?,
        ))?,
        None => match get_bytes(http, CDE_DIRECTORY_URL, 300)
            .await
            .and_then(|b| parse_cde_directory(&decode_text(b)))
        {
            Ok(m) => m,
            Err(e) => {
                warnings.push(format!("CDE public schools directory unavailable: {e}"));
                HashMap::new()
            }
        },
    };
    let dashboard = if opts.use_dashboard {
        match fetch_dashboard_rows(http).await {
            Ok(rows) => parse_dashboard_crosswalk(&rows),
            Err(e) => {
                warnings.push(format!("CDE School Dashboard layer unavailable: {e}"));
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };
    if directory.is_empty() && dashboard.is_empty() {
        bail!("no school-code crosswalk available: {}", warnings.join("; "));
    }

    let (rows, mut stats) = build_ca_metric_rows(&parsed, &directory, &dashboard, opts.year, &now_iso());
    if rows.is_empty() {
        bail!("no school metrics could be computed from {source_file}");
    }
    stats.rows_written = db.replace_school_metrics(&state, opts.year, &rows).await?;
    stats.source_file = match entry {
        Some(e) => format!("{source_file} ({e})"),
        None => source_file,
    };
    stats.warnings = warnings;
    Ok(stats)
}

// ---------------------------------------------------------------- EDFacts multi-state ingestion

/// Kind of numeric/bounded cell value in an EDFacts achievement file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdfactsValueKind {
    Exact,
    RangeMidpoint,
    BoundedEstimate,
}

/// A parsed EDFacts proficiency percentage and how it was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EdfactsPct {
    pub value: f64,
    pub kind: EdfactsValueKind,
}

#[derive(Debug, Clone, Copy)]
enum EdfactsCellOutcome {
    Value(EdfactsPct),
    Suppressed,
    Missing,
}

fn parse_edfacts_pct_outcome(raw: &str) -> EdfactsCellOutcome {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return EdfactsCellOutcome::Missing;
    }
    let upper = trimmed.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "PS" | "S" | "*" | "**" | "SUPPRESSED" | "PRIVACY" | "DS"
    ) {
        return EdfactsCellOutcome::Suppressed;
    }
    if matches!(
        upper.as_str(),
        "." | "-" | "–" | "—" | "N/A" | "NA" | "N.A." | "NULL" | "NONE" | "NO DATA" | "NODATA"
            | "ND" | "NE" | "NR" | "M" | "-1" | "-2" | "-9"
    ) {
        return EdfactsCellOutcome::Missing;
    }

    fn strip_pct(s: &str) -> &str {
        s.trim().trim_end_matches('%').trim()
    }
    let bounded_mid = |lo: f64, hi: f64| -> EdfactsCellOutcome {
        if !lo.is_finite() || !hi.is_finite() || lo < 0.0 || hi > 100.0 || lo > hi {
            return EdfactsCellOutcome::Missing;
        }
        EdfactsCellOutcome::Value(EdfactsPct {
            value: round_n((lo + hi) / 2.0, 2),
            kind: EdfactsValueKind::BoundedEstimate,
        })
    };

    // Upper-bounded: GE80 / >=80 / ≥80 / 80%+ / 80+
    for prefix in ["GE", ">=", "≥"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            if let Ok(x) = strip_pct(rest).parse::<f64>() {
                if (0.0..=100.0).contains(&x) {
                    let hi = if x <= 99.0 { 99.0 } else { 100.0 };
                    return bounded_mid(x, hi);
                }
            }
            return EdfactsCellOutcome::Missing;
        }
    }
    if let Some(rest) = upper.strip_suffix('+') {
        if let Ok(x) = strip_pct(rest).parse::<f64>() {
            if (0.0..=100.0).contains(&x) {
                let hi = if x <= 99.0 { 99.0 } else { 100.0 };
                return bounded_mid(x, hi);
            }
        }
        return EdfactsCellOutcome::Missing;
    }
    // Strictly greater: GT90 / >90
    for prefix in ["GT", ">"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            if let Ok(x) = strip_pct(rest).parse::<f64>() {
                if (0.0..=100.0).contains(&x) {
                    let lo = (x + 1.0).min(100.0);
                    let hi = if x < 99.0 { 99.0 } else { 100.0 };
                    return bounded_mid(lo, hi);
                }
            }
            return EdfactsCellOutcome::Missing;
        }
    }
    // Lower-bounded inclusive: LE10 / <=10 / ≤10
    for prefix in ["LE", "<=", "≤"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            if let Ok(x) = strip_pct(rest).parse::<f64>() {
                if (0.0..=100.0).contains(&x) {
                    return bounded_mid(0.0, x);
                }
            }
            return EdfactsCellOutcome::Missing;
        }
    }
    // Strictly less: LT50 / <50
    for prefix in ["LT", "<"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            if let Ok(x) = strip_pct(rest).parse::<f64>() {
                if (0.0..=100.0).contains(&x) {
                    let hi = (x - 1.0).max(0.0);
                    return bounded_mid(0.0, hi);
                }
            }
            return EdfactsCellOutcome::Missing;
        }
    }

    // Closed range: "40-44", "40–44", "40—44", "40 to 44"
    let normalized_range = upper
        .replace('\u{2013}', "-")
        .replace('\u{2014}', "-")
        .replace(" TO ", "-");
    if let Some((a, b)) = normalized_range.split_once('-') {
        if !a.trim().is_empty() && !b.trim().is_empty() {
            if let (Ok(lo), Ok(hi)) = (strip_pct(a).parse::<f64>(), strip_pct(b).parse::<f64>()) {
                if lo.is_finite() && hi.is_finite() && lo >= 0.0 && hi <= 100.0 && lo <= hi {
                    return EdfactsCellOutcome::Value(EdfactsPct {
                        value: round_n((lo + hi) / 2.0, 2),
                        kind: EdfactsValueKind::RangeMidpoint,
                    });
                }
            }
        }
    }

    // Exact numeric percentage
    if let Ok(v) = strip_pct(&upper).parse::<f64>() {
        if v.is_finite() && (0.0..=100.0).contains(&v) {
            return EdfactsCellOutcome::Value(EdfactsPct {
                value: round_n(v, 2),
                kind: EdfactsValueKind::Exact,
            });
        }
    }
    EdfactsCellOutcome::Missing
}

/// Parses an EDFacts cell percentage, closed range (`"40-44"` / `"40–44"` -> `42.0`), or bounded
/// estimate (`"GE80"`/`">=80"` -> `89.5`, `"GT90"` -> `95.0`, `"LT50"` -> `24.5`, `"<=10"` -> `5.0`).
/// Privacy tags (`"PS"`, `"S"`, `"*"`, `"N/A"`) return `None` safely.
pub fn parse_edfacts_pct(raw: &str) -> Option<EdfactsPct> {
    match parse_edfacts_pct_outcome(raw) {
        EdfactsCellOutcome::Value(v) => Some(v),
        EdfactsCellOutcome::Suppressed | EdfactsCellOutcome::Missing => None,
    }
}

/// Entry from an NCES CCD Public School Directory file (`ccd_sch_029_*`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CcdCrosswalkEntry {
    pub nces_id: Option<String>,
    pub state: Option<String>,
    pub level: Option<SchoolLevel>,
}

fn detect_delimiter(header: &str) -> u8 {
    let counts = [
        (b'\t', header.matches('\t').count()),
        (b'^', header.matches('^').count()),
        (b'|', header.matches('|').count()),
        (b',', header.matches(',').count()),
    ];
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .filter(|(_, c)| *c > 0)
        .map(|(d, _)| d)
        .unwrap_or(b',')
}

fn build_csv_reader(text: &str, delimiter: u8) -> csv::Reader<&[u8]> {
    let cleaned = text.trim_start_matches('\u{feff}');
    csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(cleaned.as_bytes())
}

fn find_any_col(headers: &[String], candidates: &[&str]) -> Option<usize> {
    for c in candidates {
        if let Some(idx) = headers.iter().position(|h| h.eq_ignore_ascii_case(c)) {
            return Some(idx);
        }
    }
    None
}

fn clean_id_cell(raw: &str) -> Option<String> {
    let t = raw.trim();
    let up = t.to_ascii_uppercase();
    if t.is_empty()
        || matches!(
            up.as_str(),
            "NO DATA" | "NODATA" | "N/A" | "NA" | "NULL" | "NONE" | "-1" | "-2" | "-9" | "." | "-"
        )
    {
        return None;
    }
    Some(t.to_string())
}

fn compact_state_school_id(raw: &str) -> String {
    raw.trim()
        .to_ascii_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn insert_ccd_keys(
    map: &mut HashMap<String, CcdCrosswalkEntry>,
    state: Option<&str>,
    nces_id: Option<&str>,
    st_schid: Option<&str>,
    entry: CcdCrosswalkEntry,
) {
    if let Some(id) = nces_id {
        map.insert(format!("nces:{id}"), entry.clone());
        map.entry(id.to_string()).or_insert_with(|| entry.clone());
    }
    if let Some(st_id) = st_schid {
        let up = st_id.trim().to_ascii_uppercase();
        if let Some(st) = state {
            map.insert(format!("st:{st}:{up}"), entry.clone());
            let compact = compact_state_school_id(&up);
            if !compact.is_empty() {
                map.entry(format!("stc:{st}:{compact}"))
                    .or_insert_with(|| entry.clone());
                let stripped = compact.strip_prefix(st).unwrap_or(&compact);
                if !stripped.is_empty() {
                    map.entry(format!("stc:{st}:{stripped}"))
                        .or_insert_with(|| entry.clone());
                }
            }
        }
        map.entry(format!("st_raw:{up}")).or_insert(entry);
    }
}

/// Parses an NCES CCD Public School Directory CSV/TSV file (`NCESSCH`, `ST_SCHID`, `ST`/`STABR`,
/// `SCHOOL_LEVEL`/`LEVEL`, `GSLO`, `GSHI`). Reads only school identifiers, state, and level.
pub fn parse_ccd_directory(text: &str) -> Result<HashMap<String, CcdCrosswalkEntry>> {
    let first_line = lines_of(text)
        .next()
        .ok_or_else(|| anyhow!("empty NCES CCD school directory"))?;
    let delim = detect_delimiter(first_line);
    let mut rdr = build_csv_reader(text, delim);
    let headers: Vec<String> = rdr
        .headers()
        .context("read CCD directory header")?
        .iter()
        .map(|h| h.trim().trim_start_matches('\u{feff}').to_string())
        .collect();

    let i_nces = find_any_col(&headers, &["NCESSCH", "NCES_ID", "NCES_SCHOOL_ID", "FED_ID", "FedID"]);
    let i_st_schid = find_any_col(
        &headers,
        &["ST_SCHID", "STATE_SCHOOL_ID", "STSCHID", "STATE_SCH_ID", "SCHID", "CDSCode"],
    );
    if i_nces.is_none() && i_st_schid.is_none() {
        bail!("NCES CCD directory: missing NCESSCH or ST_SCHID column");
    }
    let i_state = find_any_col(&headers, &["ST", "STABR", "STATE", "LSTATE", "MSTATE", "STATENAME", "FIPST"]);
    let i_level = find_any_col(&headers, &["SCHOOL_LEVEL", "LEVEL", "SCH_LEVEL", "EILCode"]);
    let i_gslo = find_any_col(&headers, &["GSLO", "LOW_GRADE", "GRADE_LOW"]);
    let i_gshi = find_any_col(&headers, &["GSHI", "HIGH_GRADE", "GRADE_HIGH"]);

    let mut out = HashMap::new();
    for rec in rdr.records() {
        let rec = match rec {
            Ok(r) => r,
            Err(_) => continue,
        };
        let get = |idx: Option<usize>| idx.and_then(|i| rec.get(i)).and_then(clean_id_cell);
        let nces_id = get(i_nces).and_then(|s| normalize_nces_id(&s));
        let st_schid = get(i_st_schid);
        if nces_id.is_none() && st_schid.is_none() {
            continue;
        }
        let state = get(i_state)
            .and_then(|s| normalize_us_state(&s))
            .or_else(|| nces_id.as_deref().and_then(state_from_nces_id))
            .map(str::to_string);
        let level_raw = i_level.and_then(|i| rec.get(i));
        let gslo = i_gslo.and_then(|i| rec.get(i));
        let gshi = i_gshi.and_then(|i| rec.get(i));
        let level = SchoolLevel::from_ccd(level_raw, gslo, gshi)
            .or_else(|| level_raw.and_then(SchoolLevel::from_cde_eil));

        let entry = CcdCrosswalkEntry {
            nces_id: nces_id.clone(),
            state: state.clone(),
            level,
        };
        insert_ccd_keys(
            &mut out,
            state.as_deref(),
            nces_id.as_deref(),
            st_schid.as_deref(),
            entry,
        );
    }
    if out.is_empty() {
        bail!("NCES CCD directory contains no valid school rows");
    }
    Ok(out)
}

/// Parsed All-Students EDFacts achievement record for one school within one state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdfactsSchool {
    pub state: String,
    pub school_key: String,
    pub nces_id: Option<String>,
    pub st_schid: Option<String>,
    pub name: Option<String>,
    pub level: Option<SchoolLevel>,
    /// School-wide ("00" / "All Grades") `(ELA, Math)` proficiency.
    pub all_grades: (Option<EdfactsPct>, Option<EdfactsPct>),
    /// Grade-level `(ELA, Math)` proficiency for tested grades `3..=12`.
    pub grades: BTreeMap<u8, (Option<EdfactsPct>, Option<EdfactsPct>)>,
}

impl EdfactsSchool {
    /// Mean `(ela, math, combined)` preferring school-wide `all_grades` when both subjects are
    /// reported, otherwise averaging over tested grades where both ELA and Math are reported.
    pub fn value(&self) -> Option<SchoolValue> {
        if let (Some(e), Some(m)) = self.all_grades {
            let ela = e.value;
            let math = m.value;
            return Some(SchoolValue {
                ela_pct: round_n(ela, 2),
                math_pct: round_n(math, 2),
                combined_pct: round_n((ela + math) / 2.0, 2),
                grades_used: 1,
            });
        }
        let both: Vec<(f64, f64)> = self
            .grades
            .values()
            .filter_map(|(e, m)| Some(((*e)?.value, (*m)?.value)))
            .collect();
        if both.is_empty() {
            return None;
        }
        let n = both.len() as f64;
        let ela = both.iter().map(|x| x.0).sum::<f64>() / n;
        let math = both.iter().map(|x| x.1).sum::<f64>() / n;
        let combined = both.iter().map(|(e, m)| (e + m) / 2.0).sum::<f64>() / n;
        Some(SchoolValue {
            ela_pct: round_n(ela, 2),
            math_pct: round_n(math, 2),
            combined_pct: round_n(combined, 2),
            grades_used: both.len(),
        })
    }

    fn infer_level_from_grades(&self) -> Option<SchoolLevel> {
        if self.grades.is_empty() {
            return None;
        }
        let min_g = *self.grades.keys().next()?;
        let max_g = *self.grades.keys().next_back()?;
        if max_g <= 5 || (min_g <= 3 && max_g == 6) {
            Some(SchoolLevel::Elementary)
        } else if min_g >= 9 {
            Some(SchoolLevel::High)
        } else if min_g >= 5 && max_g <= 9 {
            Some(SchoolLevel::Middle)
        } else {
            Some(SchoolLevel::Combined)
        }
    }
}

/// Result of parsing one or more EDFacts CSV/TSV school achievement files.
#[derive(Debug, Clone, Default)]
pub struct EdfactsParse {
    pub test_year: Option<i64>,
    /// Keyed by `(state_abbr, school_key)`.
    pub schools: BTreeMap<(String, String), EdfactsSchool>,
    pub rows_read: usize,
    pub rows_used: usize,
    pub suppressed_cells: usize,
    pub missing_cells: usize,
    pub range_cells: usize,
    pub bounded_cells: usize,
}

static SY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:sy\s*)?(20\d{2})\s*[-–/]\s*(\d{2,4})").unwrap());
static SY_COMPACT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^sy\s*(\d{2})(\d{2})$").unwrap());

fn parse_edfacts_school_year(raw: &str) -> Option<i64> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(y) = t.parse::<i64>() {
        if (2000..=2100).contains(&y) {
            return Some(y);
        }
    }
    if let Some(c) = SY_RE.captures(t) {
        let start = c[1].parse::<i64>().ok()?;
        let end_raw = c[2].parse::<i64>().ok()?;
        let end = if end_raw < 100 {
            (start / 100) * 100 + end_raw
        } else {
            end_raw
        };
        if (2000..=2100).contains(&end) {
            return Some(end);
        }
    }
    if let Some(c) = SY_COMPACT_RE.captures(t) {
        let end = 2000 + c[2].parse::<i64>().ok()?;
        if (2000..=2100).contains(&end) {
            return Some(end);
        }
    }
    None
}

fn parse_edfacts_grade(raw: &str) -> Option<u8> {
    let u = raw.trim().to_ascii_uppercase();
    if matches!(u.as_str(), "" | "00" | "0" | "ALL" | "ALL GRADES" | "TOTAL" | "ALL_GRADES") {
        return Some(0);
    }
    if matches!(u.as_str(), "HS" | "HIGH" | "HIGH SCHOOL") {
        return Some(11);
    }
    u.parse::<u8>().ok().filter(|g| (3..=12).contains(g))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdfactsSubject {
    Ela,
    Math,
}

fn parse_edfacts_subject(raw: &str) -> Option<EdfactsSubject> {
    let u = raw.trim().to_ascii_uppercase();
    if matches!(
        u.as_str(),
        "ELA" | "RLA" | "READ" | "READING" | "ENGLISH" | "ENGLISH LANGUAGE ARTS"
            | "READING/LANGUAGE ARTS" | "ELA/RLA" | "RLA/ELA"
    ) {
        return Some(EdfactsSubject::Ela);
    }
    if matches!(u.as_str(), "MATH" | "MTH" | "MATHEMATICS") {
        return Some(EdfactsSubject::Math);
    }
    None
}

fn is_all_students_subgroup(raw: &str) -> bool {
    let l = raw.trim().to_ascii_lowercase();
    matches!(
        l.as_str(),
        "" | "all" | "all students" | "all_students" | "total" | "1"
    )
}

static WIDE_EDFACTS_COL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:ALL_)?(RLA|ELA|MTH|MATH)(\d{2}|HS)?_?PCTPROF$").unwrap()
});

fn classify_wide_col(header: &str) -> Option<(EdfactsSubject, u8)> {
    let h = header.trim();
    let u = h.to_ascii_uppercase();
    if let Some(c) = WIDE_EDFACTS_COL_RE.captures(h) {
        let subj = match &c[1].to_ascii_uppercase()[..] {
            "RLA" | "ELA" => EdfactsSubject::Ela,
            _ => EdfactsSubject::Math,
        };
        let grade = c
            .get(2)
            .and_then(|g| parse_edfacts_grade(g.as_str()))
            .unwrap_or(0);
        return Some((subj, grade));
    }
    match u.as_str() {
        "ELA_PCT" | "RLA_PCT" | "ELA_PROF_PCT" | "RLA_PROF_PCT" | "ELA_PROFICIENT_PCT"
        | "RLA_PROFICIENT_PCT" => Some((EdfactsSubject::Ela, 0)),
        "MATH_PCT" | "MTH_PCT" | "MATH_PROF_PCT" | "MTH_PROF_PCT" | "MATH_PROFICIENT_PCT"
        | "MTH_PROFICIENT_PCT" => Some((EdfactsSubject::Math, 0)),
        _ => None,
    }
}

#[derive(Default)]
struct EdfactsCellCounts {
    suppressed: usize,
    missing: usize,
    range: usize,
    bounded: usize,
}

fn record_cell(
    counts: &mut EdfactsCellCounts,
    school: &mut EdfactsSchool,
    subj: EdfactsSubject,
    grade: u8,
    raw: &str,
) -> bool {
    match parse_edfacts_pct_outcome(raw) {
        EdfactsCellOutcome::Value(v) => {
            match v.kind {
                EdfactsValueKind::Exact => {}
                EdfactsValueKind::RangeMidpoint => counts.range += 1,
                EdfactsValueKind::BoundedEstimate => counts.bounded += 1,
            }
            let slot = if grade == 0 {
                &mut school.all_grades
            } else {
                school.grades.entry(grade).or_insert((None, None))
            };
            match subj {
                EdfactsSubject::Ela => slot.0 = Some(v),
                EdfactsSubject::Math => slot.1 = Some(v),
            }
            true
        }
        EdfactsCellOutcome::Suppressed => {
            counts.suppressed += 1;
            false
        }
        EdfactsCellOutcome::Missing => {
            counts.missing += 1;
            false
        }
    }
}

/// Parses one EDFacts CSV/TSV school achievement file into `EdfactsParse` (appending/merging into
/// `acc` if provided). Supports wide (`ALL_RLA00PCTPROF`/`ALL_MTH00PCTPROF`) and long (`SUBJECT`,
/// `PCTPROF`, optional `GRADE` and `SUBGROUP`) layouts. Only All-Students rows/columns are read.
pub fn parse_edfacts_csv_into(
    acc: &mut EdfactsParse,
    text: &str,
    target_state: Option<&str>,
    target_year: Option<i64>,
) -> Result<()> {
    let first_line = lines_of(text)
        .next()
        .ok_or_else(|| anyhow!("empty EDFacts CSV/TSV file"))?;
    let delim = detect_delimiter(first_line);
    let mut rdr = build_csv_reader(text, delim);
    let headers: Vec<String> = rdr
        .headers()
        .context("read EDFacts header")?
        .iter()
        .map(|h| h.trim().trim_start_matches('\u{feff}').to_string())
        .collect();

    let i_nces = find_any_col(&headers, &["NCESSCH", "NCES_ID", "NCES_SCHOOL_ID", "FED_ID", "FedID"]);
    let i_st_schid = find_any_col(
        &headers,
        &["ST_SCHID", "STATE_SCHOOL_ID", "STSCHID", "STATE_SCH_ID", "SCHID", "SCHOOL_ID"],
    );
    if i_nces.is_none() && i_st_schid.is_none() {
        bail!("not an EDFacts school achievement file: missing NCESSCH or ST_SCHID column");
    }
    let i_state = find_any_col(
        &headers,
        &["ST", "STABR", "STATE", "STATE_ABBR", "FIPST", "STNAM", "STATE_NAME"],
    );
    let i_name = find_any_col(&headers, &["SCHNAM", "SCHOOL_NAME", "SCH_NAME", "SCHOOL"]);
    let i_level = find_any_col(&headers, &["SCHOOL_LEVEL", "LEVEL", "SCH_LEVEL"]);
    let i_gslo = find_any_col(&headers, &["GSLO", "LOW_GRADE", "GRADE_LOW"]);
    let i_gshi = find_any_col(&headers, &["GSHI", "HIGH_GRADE", "GRADE_HIGH"]);
    let i_year = find_any_col(&headers, &["SCHOOL_YEAR", "SY", "YEAR", "TEST_YEAR"]);
    let i_entity = find_any_col(&headers, &["SUMMARY_LEVEL", "DATA_LEVEL", "ENTITY_TYPE"]);

    let wide_cols: Vec<(usize, EdfactsSubject, u8)> = headers
        .iter()
        .enumerate()
        .filter_map(|(i, h)| classify_wide_col(h).map(|(s, g)| (i, s, g)))
        .collect();

    let i_subj = find_any_col(
        &headers,
        &["SUBJECT", "ASSESSMENT_SUBJECT", "CONTENT_AREA", "TEST_SUBJECT", "TEST"],
    );
    let i_pct = find_any_col(
        &headers,
        &[
            "PCTPROF",
            "PCT_PROFICIENT",
            "PERCENT_PROFICIENT",
            "PROFICIENCY_PCT",
            "PERCENTAGE_PROFICIENT",
            "PCT_MET_AND_ABOVE",
            "PCT",
            "VALUE",
            "RATE",
        ],
    );
    let i_grade = find_any_col(&headers, &["GRADE", "GRADE_LEVEL", "TEST_GRADE"]);
    let i_group = find_any_col(
        &headers,
        &["SUBGROUP", "STUDENT_GROUP", "STUDENT_GROUP_ID", "GROUP", "POPULATION"],
    );

    if wide_cols.is_empty() && (i_subj.is_none() || i_pct.is_none()) {
        bail!(
            "not an EDFacts achievement file: missing wide (ALL_RLA00PCTPROF/ALL_MTH00PCTPROF) or long (SUBJECT/PCTPROF) proficiency columns"
        );
    }

    let mut seen_years: BTreeSet<i64> = BTreeSet::new();
    let mut rows_before_year_match = 0usize;
    let mut rows_matched = 0usize;

    for rec in rdr.records() {
        let rec = match rec {
            Ok(r) => r,
            Err(_) => continue,
        };
        acc.rows_read += 1;

        if let Some(ie) = i_entity {
            let ent = rec.get(ie).unwrap_or("").trim().to_ascii_lowercase();
            if !ent.is_empty() && !ent.contains("school") {
                continue;
            }
        }
        if let Some(ig) = i_group {
            if !is_all_students_subgroup(rec.get(ig).unwrap_or("")) {
                continue;
            }
        }

        if let Some(iy) = i_year {
            if let Some(yr) = rec.get(iy).and_then(parse_edfacts_school_year) {
                seen_years.insert(yr);
                if let Some(want) = target_year {
                    if yr != want {
                        continue;
                    }
                }
                if acc.test_year.is_none() {
                    acc.test_year = Some(yr);
                }
            }
        }

        let nces_raw = i_nces.and_then(|i| rec.get(i)).and_then(clean_id_cell);
        let nces_id = nces_raw.as_deref().and_then(normalize_nces_id);
        let st_schid = i_st_schid.and_then(|i| rec.get(i)).and_then(clean_id_cell);
        if nces_id.is_none() && st_schid.is_none() {
            continue;
        }
        if nces_id.as_deref() == Some("000000000000") && st_schid.is_none() {
            continue;
        }

        let row_state = i_state
            .and_then(|i| rec.get(i))
            .and_then(normalize_us_state)
            .or_else(|| nces_id.as_deref().and_then(state_from_nces_id))
            .or(target_state);
        let Some(state) = row_state else {
            continue;
        };
        if let Some(want_st) = target_state {
            if !state.eq_ignore_ascii_case(want_st) {
                continue;
            }
        }
        rows_before_year_match += 1;

        let school_key = nces_id
            .clone()
            .or_else(|| st_schid.as_deref().map(|s| s.trim().to_ascii_uppercase()))
            .unwrap_or_default();
        if school_key.is_empty() {
            continue;
        }

        let name = i_name
            .and_then(|i| rec.get(i))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let row_level = SchoolLevel::from_ccd(
            i_level.and_then(|i| rec.get(i)),
            i_gslo.and_then(|i| rec.get(i)),
            i_gshi.and_then(|i| rec.get(i)),
        );

        let entry = acc
            .schools
            .entry((state.to_string(), school_key.clone()))
            .or_insert_with(|| EdfactsSchool {
                state: state.to_string(),
                school_key,
                nces_id: nces_id.clone(),
                st_schid: st_schid.clone(),
                name: name.clone(),
                level: row_level,
                all_grades: (None, None),
                grades: BTreeMap::new(),
            });
        if entry.nces_id.is_none() {
            entry.nces_id = nces_id;
        }
        if entry.st_schid.is_none() {
            entry.st_schid = st_schid;
        }
        if entry.name.is_none() {
            entry.name = name;
        }
        if entry.level.is_none() {
            entry.level = row_level;
        }

        let mut counts = EdfactsCellCounts::default();
        let mut used_row = false;
        if !wide_cols.is_empty() {
            for &(col_idx, subj, grade) in &wide_cols {
                if let Some(raw) = rec.get(col_idx) {
                    if record_cell(&mut counts, entry, subj, grade, raw) {
                        used_row = true;
                    }
                }
            }
        }
        if let (Some(is), Some(ip)) = (i_subj, i_pct) {
            if let Some(subj) = rec.get(is).and_then(parse_edfacts_subject) {
                let grade = i_grade
                    .and_then(|ig| rec.get(ig))
                    .map(parse_edfacts_grade)
                    .unwrap_or(Some(0));
                if let Some(g) = grade {
                    if let Some(raw) = rec.get(ip) {
                        if record_cell(&mut counts, entry, subj, g, raw) {
                            used_row = true;
                        }
                    }
                }
            }
        }
        acc.suppressed_cells += counts.suppressed;
        acc.missing_cells += counts.missing;
        acc.range_cells += counts.range;
        acc.bounded_cells += counts.bounded;
        if used_row {
            acc.rows_used += 1;
            rows_matched += 1;
        }
    }

    if rows_matched == 0 && rows_before_year_match == 0 {
        if let (Some(want), Some(&only_yr)) = (target_year, seen_years.iter().next()) {
            if seen_years.len() == 1 && only_yr != want {
                bail!("EDFacts file holds test year {only_yr}, not --year {want}");
            }
        }
    }
    Ok(())
}

/// Parses a single EDFacts CSV/TSV file.
pub fn parse_edfacts_csv(
    text: &str,
    target_state: Option<&str>,
    target_year: Option<i64>,
) -> Result<EdfactsParse> {
    let mut out = EdfactsParse::default();
    parse_edfacts_csv_into(&mut out, text, target_state, target_year)?;
    if out.schools.is_empty() {
        bail!("no school-level All Students rows found in the EDFacts file");
    }
    Ok(out)
}

/// Parses EDFacts bytes: either a single CSV/TSV/TXT file or a ZIP archive containing one or more
/// `.csv`/`.tsv`/`.txt` achievement files (for example separate RLA and Math files).
pub fn parse_edfacts_bytes(
    bytes: Vec<u8>,
    target_state: Option<&str>,
    target_year: Option<i64>,
) -> Result<(Option<String>, EdfactsParse)> {
    if bytes.starts_with(b"PK\x03\x04") {
        let entries = zip_entries(&bytes)?;
        let data_names: Vec<String> = entries
            .into_iter()
            .map(|e| e.name)
            .filter(|n| {
                let l = n.to_ascii_lowercase();
                !l.starts_with("__macosx/")
                    && !l.contains("readme")
                    && !l.contains("layout")
                    && (l.ends_with(".csv") || l.ends_with(".tsv") || l.ends_with(".txt"))
            })
            .collect();
        if data_names.is_empty() {
            bail!("no .csv/.tsv/.txt achievement files found inside ZIP archive");
        }
        let mut acc = EdfactsParse::default();
        let mut parsed_files = Vec::new();
        for name in &data_names {
            let (_, data) = zip_extract(&bytes, |n| n == name)?;
            let txt = decode_text(data);
            if parse_edfacts_csv_into(&mut acc, &txt, target_state, target_year).is_ok() {
                parsed_files.push(name.clone());
            }
        }
        if parsed_files.is_empty() || acc.schools.is_empty() {
            bail!("no valid EDFacts school achievement rows found in ZIP archive");
        }
        Ok((Some(parsed_files.join(", ")), acc))
    } else {
        let parsed = parse_edfacts_csv(&decode_text(bytes), target_state, target_year)?;
        Ok((None, parsed))
    }
}

fn lookup_ccd<'a>(
    directory: &'a HashMap<String, CcdCrosswalkEntry>,
    state: &str,
    nces_id: Option<&str>,
    st_schid: Option<&str>,
) -> Option<&'a CcdCrosswalkEntry> {
    if let Some(id) = nces_id {
        if let Some(hit) = directory.get(&format!("nces:{id}")).or_else(|| directory.get(id)) {
            return Some(hit);
        }
    }
    if let Some(st_id) = st_schid {
        let up = st_id.trim().to_ascii_uppercase();
        if let Some(hit) = directory.get(&format!("st:{state}:{up}")) {
            return Some(hit);
        }
        let compact = compact_state_school_id(&up);
        if let Some(hit) = directory.get(&format!("stc:{state}:{compact}")) {
            return Some(hit);
        }
        let stripped = compact.strip_prefix(state).unwrap_or(&compact);
        if let Some(hit) = directory.get(&format!("stc:{state}:{stripped}")) {
            return Some(hit);
        }
        if let Some(hit) = directory.get(&format!("st_raw:{up}")) {
            return Some(hit);
        }
    }
    None
}

fn stable_metric_id(state: &str, year: i64, key: &str) -> String {
    let raw = format!("{state}-{year}-{key}");
    if raw.len() <= 64 {
        raw
    } else {
        let mut h = Sha256::new();
        h.update(raw.as_bytes());
        let digest = hex::encode(h.finalize());
        format!("{state}-{year}-{}", &digest[..32])
    }
}

/// Converts parsed EDFacts schools and optional NCES CCD crosswalk into `school_metrics` rows.
/// Within-state percentiles are strictly isolated per `(state, level)` pool.
pub fn build_edfacts_metric_rows(
    parsed: &EdfactsParse,
    directory: &HashMap<String, CcdCrosswalkEntry>,
    year: i64,
    updated_at: &str,
) -> (Vec<SchoolMetricRow>, IngestStats) {
    let source = edfacts_metric_source(year);
    let states_seen: BTreeSet<&str> = parsed.schools.keys().map(|(s, _)| s.as_str()).collect();
    let stats_state = if states_seen.len() == 1 {
        states_seen.iter().next().copied().unwrap_or("EDFACTS").to_string()
    } else {
        "EDFACTS".to_string()
    };

    let mut stats = IngestStats {
        state: stats_state,
        year,
        school_year: metric_year(year),
        source: source.clone(),
        rows_read: parsed.rows_read,
        rows_used: parsed.rows_used,
        suppressed_cells: parsed.suppressed_cells,
        range_cells: parsed.range_cells,
        bounded_cells: parsed.bounded_cells,
        caaspp_schools: parsed.schools.len(),
        directory_rows: directory.len(),
        ..Default::default()
    };

    struct Pending<'a> {
        school: &'a EdfactsSchool,
        nces: Option<String>,
        level: SchoolLevel,
        value: SchoolValue,
    }

    let mut pending: Vec<Pending> = Vec::new();
    for school in parsed.schools.values() {
        let Some(value) = school.value() else { continue };
        stats.schools_with_metric += 1;

        let dir_hit = lookup_ccd(
            directory,
            &school.state,
            school.nces_id.as_deref(),
            school.st_schid.as_deref(),
        );
        let nces = match (
            school.nces_id.clone(),
            dir_hit.and_then(|e| e.nces_id.clone()),
        ) {
            (Some(n), _) => {
                stats.matched_nces += 1;
                Some(n)
            }
            (None, Some(n)) => {
                stats.matched_by_directory += 1;
                stats.matched_nces += 1;
                Some(n)
            }
            (None, None) => {
                if stats.unmatched_examples.len() < 15 {
                    stats.unmatched_examples.push(format!(
                        "{}:{} — {}",
                        school.state,
                        school.school_key,
                        school.name.as_deref().unwrap_or("?")
                    ));
                }
                None
            }
        };

        let Some(level) = dir_hit
            .and_then(|e| e.level)
            .or(school.level)
            .or_else(|| school.infer_level_from_grades())
        else {
            stats.without_level += 1;
            continue;
        };

        pending.push(Pending {
            school,
            nces,
            level,
            value,
        });
    }

    stats.match_rate_pct = if stats.schools_with_metric == 0 {
        0.0
    } else {
        round_n(100.0 * stats.matched_nces as f64 / stats.schools_with_metric as f64, 2)
    };

    // Exact state isolation: group strictly by (state, level) so TX is never ranked against CA.
    let mut by_state_level: BTreeMap<(&str, SchoolLevel), Vec<usize>> = BTreeMap::new();
    for (i, p) in pending.iter().enumerate() {
        by_state_level
            .entry((p.school.state.as_str(), p.level))
            .or_default()
            .push(i);
    }

    let mut percentile = vec![0.0; pending.len()];
    for ((_, level), idx) in &by_state_level {
        let values: Vec<f64> = idx.iter().map(|&i| pending[i].value.combined_pct).collect();
        for (k, p) in percentile_ranks(&values).into_iter().enumerate() {
            percentile[idx[k]] = p;
        }
        *stats.by_level.entry(level.as_str().to_string()).or_insert(0) += idx.len();
    }

    let rows = pending
        .iter()
        .zip(percentile)
        .map(|(p, pct)| {
            let key = p
                .nces
                .as_deref()
                .unwrap_or(p.school.school_key.as_str());
            let cds_code = p.school.st_schid.as_ref().map(|s| {
                let t = s.trim();
                t.chars().take(24).collect::<String>()
            });
            SchoolMetricRow {
                id: stable_metric_id(&p.school.state, year, key),
                state: p.school.state.clone(),
                nces_id: p.nces.clone(),
                cds_code,
                level: p.level.as_str().to_string(),
                year,
                ela_pct: Some(p.value.ela_pct),
                math_pct: Some(p.value.math_pct),
                combined_pct: p.value.combined_pct,
                percentile: round_n(pct, 1),
                source: source.clone(),
                updated_at: updated_at.to_string(),
            }
        })
        .collect();

    (rows, stats)
}

async fn ingest_edfacts(
    db: &DbStore,
    http: &HttpClient,
    opts: &IngestOptions,
    requested_state: &str,
    target_state: Option<&str>,
) -> Result<IngestStats> {
    let mut warnings = Vec::new();
    let (source_file, bytes) = match &opts.file {
        Some(p) => (
            p.display().to_string(),
            std::fs::read(p).with_context(|| format!("read {}", p.display()))?,
        ),
        None => {
            if let Some(url) = http.env().get(ENV_EDFACTS_FILE_URL) {
                let bytes = get_bytes(http, url, 600).await?;
                (url.to_string(), bytes)
            } else {
                bail!(
                    "EDFacts ingestion for state {requested_state} requires --file <path> or {ENV_EDFACTS_FILE_URL} \
                     (download federal school achievement CSV/TSV/ZIP from {EDFACTS_INFO_URL})"
                );
            }
        }
    };

    let (entry, parsed) = parse_edfacts_bytes(bytes, target_state, Some(opts.year))?;
    let directory = match &opts.directory_file {
        Some(p) => parse_ccd_directory(&decode_text(
            std::fs::read(p).with_context(|| format!("read {}", p.display()))?,
        ))?,
        None => HashMap::new(),
    };

    let (rows, mut stats) = build_edfacts_metric_rows(&parsed, &directory, opts.year, &now_iso());
    if rows.is_empty() {
        bail!("no school metrics could be computed from {source_file} for state {requested_state}");
    }

    let mut by_state: BTreeMap<String, Vec<SchoolMetricRow>> = BTreeMap::new();
    for r in rows {
        by_state.entry(r.state.clone()).or_default().push(r);
    }
    let mut written = 0usize;
    for (st, st_rows) in &by_state {
        written += db.replace_school_metrics(st, opts.year, st_rows).await?;
    }
    if target_state.is_none() && by_state.len() > 1 {
        stats.state = requested_state.to_string();
    }
    if directory.is_empty() && stats.matched_nces < stats.schools_with_metric {
        warnings.push(
            "No --directory-file supplied; schools without NCESSCH in the EDFacts file could not be mapped"
                .to_string(),
        );
    }
    stats.rows_written = written;
    stats.source_file = match entry {
        Some(e) => format!("{source_file} ({e})"),
        None => source_file,
    };
    stats.warnings = warnings;
    Ok(stats)
}

/// Refreshes California CAASPP metrics from official CDE endpoints for `year`.
pub async fn refresh_ca_metrics(
    db: &DbStore,
    http: &HttpClient,
    year: i64,
    use_dashboard: bool,
) -> Result<IngestStats> {
    ingest(
        db,
        http,
        &IngestOptions {
            state: CA_STATE.to_string(),
            year,
            file: None,
            directory_file: None,
            use_dashboard,
        },
    )
    .await
}

// ---------------------------------------------------------------- Report-time lookup

static WARNED_EMPTY: AtomicBool = AtomicBool::new(false);
static STORE_POOL_CACHE: OnceCell<Mutex<HashMap<String, Option<DbStore>>>> = OnceCell::const_new();

fn warn_empty_once() {
    if !WARNED_EMPTY.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "school_metrics has no California rows: school scores fall back to California School Dashboard tiers. \
             Ingest CAASPP data via `school_metrics::ingest` to load CAASPP percentiles."
        );
    }
}

/// Stable cache key for `EnvConfig`'s database target; `None` for in-memory SQLite or when no
/// explicit database is configured and no default SQLite file exists yet on disk.
pub(crate) fn db_target_key(env: &EnvConfig) -> Option<String> {
    let raw_provider = env
        .get("DATABASE_PROVIDER")
        .or_else(|| env.get("DB_PROVIDER"))
        .or_else(|| env.get("DB_DIALECT"))
        .unwrap_or("");
    let mysql_url = env.get("MYSQL_URL");
    let db_url = env.get("DATABASE_URL");
    let provider = if !raw_provider.trim().is_empty() {
        raw_provider.trim().to_ascii_lowercase()
    } else if mysql_url.is_some()
        || db_url
            .map(|u| u.trim().to_ascii_lowercase().starts_with("mysql://"))
            .unwrap_or(false)
    {
        "mysql".to_string()
    } else {
        "sqlite".to_string()
    };
    if provider == "mysql" || provider == "heatwave" {
        let url = mysql_url.or(db_url)?;
        return Some(format!("{provider}:{url}"));
    }
    if let Some(p) = env.get("SQLITE_DB_PATH").or_else(|| env.get("SQLITE_PATH")) {
        let trimmed = p.trim();
        if trimmed.is_empty() || trimmed == ":memory:" || trimmed.contains("mode=memory") {
            return None;
        }
        return Some(format!("sqlite:{trimmed}"));
    }
    if let Some(u) = db_url {
        let trimmed = u.trim();
        if trimmed.is_empty() || trimmed == ":memory:" || trimmed.contains("mode=memory") {
            return None;
        }
        return Some(format!("sqlite:{trimmed}"));
    }
    if env.get("SQLITE_MEMORY") == Some("1") {
        return None;
    }
    let default_path = crate::db::default_sqlite_path();
    if Path::new(&default_path).exists() {
        Some(format!("sqlite:{default_path}"))
    } else {
        None
    }
}

/// Opens and caches a `DbStore` handle per configured target when the caller does not pass one.
async fn shared_store(env: &EnvConfig) -> Option<DbStore> {
    let key = db_target_key(env)?;
    let cache = STORE_POOL_CACHE
        .get_or_init(|| async { Mutex::new(HashMap::new()) })
        .await;
    {
        let guard = cache.lock().await;
        if let Some(cached) = guard.get(&key) {
            return cached.clone();
        }
    }
    let connected = match DbStore::connect(env).await {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!("school_metrics: database unavailable ({e}); school scores use Dashboard tiers");
            None
        }
    };
    let mut guard = cache.lock().await;
    guard.insert(key, connected.clone());
    connected
}

/// School metrics for the given NCES IDs (latest year per school), optionally filtered to one
/// state (e.g. `"CA"` or `"TX"`), for report building. Reads the database only; never downloads.
pub async fn state_metrics_for_report(
    db: Option<&DbStore>,
    env: &EnvConfig,
    state: Option<&str>,
    nces_ids: &[String],
) -> HashMap<String, SchoolMetricRow> {
    if nces_ids.is_empty() {
        return HashMap::new();
    }
    let norm_ids: Vec<String> = nces_ids
        .iter()
        .filter_map(|id| normalize_nces_id(id))
        .collect();
    if norm_ids.is_empty() {
        return HashMap::new();
    }
    let opened;
    let store = match db {
        Some(d) => d,
        None => {
            opened = shared_store(env).await;
            match opened.as_ref() {
                Some(d) => d,
                None => return HashMap::new(),
            }
        }
    };
    let want_state = state
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| normalize_us_state(s).or(Some(s)));
    match store.get_school_metrics_by_nces(&norm_ids).await {
        Ok(found) if !found.is_empty() => match want_state {
            Some(st) => found
                .into_iter()
                .filter(|(_, m)| m.state.eq_ignore_ascii_case(st))
                .collect(),
            None => found,
        },
        Ok(_) => {
            if want_state.map(|s| s.eq_ignore_ascii_case(CA_STATE)).unwrap_or(false)
                && matches!(store.count_school_metrics(Some(CA_STATE)).await, Ok(0))
            {
                warn_empty_once();
            }
            HashMap::new()
        }
        Err(e) => {
            tracing::warn!("school_metrics lookup failed ({e}); school scores unavailable");
            HashMap::new()
        }
    }
}

/// California metrics for the given NCES IDs (latest year per school), for report building. Reads the
/// database only; never downloads. Empty when nothing is stored (the empty table is logged once).
pub async fn ca_metrics_for_report(
    db: Option<&DbStore>,
    env: &EnvConfig,
    nces_ids: &[String],
) -> HashMap<String, SchoolMetricRow> {
    state_metrics_for_report(db, env, Some(CA_STATE), nces_ids).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_level_and_label_conversions() {
        assert_eq!(SchoolLevel::from_cde_eil("ELEM"), Some(SchoolLevel::Elementary));
        assert_eq!(SchoolLevel::from_cde_eil("intmidjr"), Some(SchoolLevel::Middle));
        assert_eq!(SchoolLevel::from_cde_eil("HS"), Some(SchoolLevel::High));
        assert_eq!(SchoolLevel::from_cde_eil("ELEMHIGH"), Some(SchoolLevel::Combined));
        assert_eq!(SchoolLevel::from_cde_eil("UG"), None);

        assert_eq!(SchoolLevel::from_dashboard("Primary"), Some(SchoolLevel::Elementary));
        assert_eq!(SchoolLevel::from_dashboard("Secondary"), Some(SchoolLevel::High));
        assert_eq!(SchoolLevel::from_dashboard("Other"), Some(SchoolLevel::Combined));
        assert_eq!(SchoolLevel::parse("combined"), Some(SchoolLevel::Combined));

        assert!(is_nces_school_id("060720000663"));
        assert!(!is_nces_school_id("06072000066"));
        assert!(!is_nces_school_id("06072000066a"));

        assert_eq!(school_year_label(2026), "2025–26");
        assert_eq!(metric_year(2026), "2025-26");
        assert_eq!(
            ca_metric_source(2026),
            "CAASPP 2025–26, California Department of Education"
        );
    }

    #[test]
    fn unit_db_target_key_respects_memory_and_explicit_paths() {
        let mem = EnvConfig::from_map(HashMap::from([("SQLITE_MEMORY".into(), "1".into())]));
        assert_eq!(db_target_key(&mem), None);

        let mem_path = EnvConfig::from_map(HashMap::from([("SQLITE_DB_PATH".into(), ":memory:".into())]));
        assert_eq!(db_target_key(&mem_path), None);

        let file_env = EnvConfig::from_map(HashMap::from([(
            "SQLITE_DB_PATH".into(),
            "/tmp/test-schools.sqlite".into(),
        )]));
        assert_eq!(
            db_target_key(&file_env).as_deref(),
            Some("sqlite:/tmp/test-schools.sqlite")
        );
    }

    #[test]
    fn unit_percentile_ranks_midrank_ties() {
        assert_eq!(percentile_ranks(&[]), Vec::<f64>::new());
        assert_eq!(percentile_ranks(&[55.0]), vec![50.0]);
        assert_eq!(
            percentile_ranks(&[10.0, 20.0, 20.0, 30.0]),
            vec![12.5, 50.0, 50.0, 87.5]
        );
    }
}
