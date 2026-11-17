// Commercial Land (Residual Land Value, Buildable Envelope & NNN Ground-Lease Pad Valuation)
// and Agricultural / Farmland Valuation Engine (USDA NRCS SSURGO Soil Data Access + 50-State
// USDA NASS Public-Domain Land Values & Cash Rents, 4-Mode Farm Operating P&L, Ag Tax Exemptions,
// USDA FSA Direct/Guaranteed Farm Ownership Loans & Farm Credit System 65% LTV Ag Financing).
//
// Upstream Compliance:
// - Uses ONLY keyless US Federal Public-Domain endpoints (17 U.S.C. § 105):
//     * USDA NRCS Soil Data Access REST API (`https://SDMDataAccess.sc.egov.usda.gov/Tabular/post.rest`)
//     * Canonical USDA NASS State Agricultural Land Values & Cash Rents Survey tables
// - Never scrapes any commercial listing portal.
// - Offline/test safe: `fetch_usda_soil_profile` respects `HttpClient` cache and offline loopback mode,
//   returning `None` gracefully on timeout/offline so `infer_baseline_soil_profile` provides a
//   transparently labelled `modeled_regional_baseline`.

use crate::shared::{round_n, HttpClient, ModuleOutput, SourceItem};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

pub const USDA_SDA_ENDPOINT: &str = "https://SDMDataAccess.sc.egov.usda.gov/Tabular/post.rest";
const SQFT_PER_ACRE: f64 = 43_560.0;

pub const LAND_FARM_DISCLAIMER: &str = "Automated commercial land development feasibility, residual land value (RLV), NNN pad ground-lease modeling, and USDA NRCS/NASS farmland screening — NOT a certified USPAP real-estate appraisal, civil engineering survey, municipal zoning entitlement determination, water-rights adjudication, or ASTM E1527-21 Phase I ESA. Verify local zoning code setbacks/FAR, utility capacity, soil borings, irrigation water rights, agricultural tax-exemption eligibility, and lender terms with licensed professionals.";

/// Soil profile resolved either live from USDA NRCS Soil Data Access (SSURGO) or from the
/// deterministic state/terrain regional baseline when offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsdaSoilProfile {
    /// `"usda_nrcs_ssurgo_live"` or `"modeled_regional_baseline"`
    pub data_source: String,
    pub mukey: Option<String>,
    pub map_unit_name: String,
    pub prime_farmland_class: String,
    pub is_prime_farmland: bool,
    pub irrigated_capability_class: Option<String>,
    /// USDA Land Capability Class Roman numeral (`"I"` through `"VIII"`).
    pub non_irrigated_capability_class: String,
    /// Numeric capability class (`1` through `8`).
    pub capability_class_num: u8,
    /// National Commodity Crop Productivity Index (`0.0` to `1.0`).
    pub nccpi_overall: f64,
    pub drainage_class: String,
    pub hydric_rating: String,
    pub slope_pct: f64,
}

/// Optional caller overrides for Commercial Land & Agricultural/Farmland evaluation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LandFarmEvalOptions {
    #[serde(alias = "acres", alias = "lot_acres", alias = "lotAcres")]
    pub acreage: Option<f64>,
    #[serde(alias = "allowable_far", alias = "far")]
    pub allowable_far: Option<f64>,
    #[serde(alias = "farming_mode", alias = "ag_mode", alias = "agMode")]
    pub farming_mode: Option<String>,
    #[serde(
        alias = "target_use",
        alias = "highest_and_best_use",
        alias = "highestAndBestUse"
    )]
    pub target_use: Option<String>,
    #[serde(
        alias = "asking_price",
        alias = "purchase_price",
        alias = "purchasePrice",
        alias = "price"
    )]
    pub asking_price: Option<f64>,
}

/// Canonical 50-state (+ US fallback) USDA NASS *Land Values* & *Cash Rents* public-domain benchmark.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NassStateAgBenchmark {
    pub state_code: &'static str,
    pub state_name: &'static str,
    pub region: &'static str,
    pub irrigated_cropland_per_acre: f64,
    pub non_irrigated_cropland_per_acre: f64,
    pub pasture_rangeland_per_acre: f64,
    pub specialty_orchard_vineyard_per_acre: f64,
    pub irrigated_cash_rent_per_acre_yr: f64,
    pub non_irrigated_cash_rent_per_acre_yr: f64,
    pub pasture_cash_rent_per_acre_yr: f64,
    pub ag_cap_rate_pct: f64,
    pub baseline_nccpi: f64,
}

macro_rules! nass_row {
    (
        $code:expr, $name:expr, $region:expr,
        $irr_val:expr, $nirr_val:expr, $past_val:expr, $orch_val:expr,
        $irr_rent:expr, $nirr_rent:expr, $past_rent:expr,
        $cap:expr, $nccpi:expr
    ) => {
        NassStateAgBenchmark {
            state_code: $code,
            state_name: $name,
            region: $region,
            irrigated_cropland_per_acre: $irr_val,
            non_irrigated_cropland_per_acre: $nirr_val,
            pasture_rangeland_per_acre: $past_val,
            specialty_orchard_vineyard_per_acre: $orch_val,
            irrigated_cash_rent_per_acre_yr: $irr_rent,
            non_irrigated_cash_rent_per_acre_yr: $nirr_rent,
            pasture_cash_rent_per_acre_yr: $past_rent,
            ag_cap_rate_pct: $cap,
            baseline_nccpi: $nccpi,
        }
    };
}

static NASS_US_FALLBACK: NassStateAgBenchmark = nass_row!(
    "US",
    "United States",
    "National Benchmark",
    6_850.0,
    5_250.0,
    1_820.0,
    12_400.0,
    248.0,
    148.0,
    16.0,
    4.05,
    0.55
);

/// 50-State + DC USDA NASS *Agricultural Land Values* & *Cash Rents* public-domain registry.
static NASS_STATE_BENCHMARKS: &[NassStateAgBenchmark] = &[
    // Corn Belt
    nass_row!(
        "IA",
        "Iowa",
        "Corn Belt",
        12_400.0,
        11_700.0,
        3_950.0,
        13_500.0,
        290.0,
        279.0,
        64.0,
        3.55,
        0.82
    ),
    nass_row!(
        "IL",
        "Illinois",
        "Corn Belt",
        10_800.0,
        9_850.0,
        3_850.0,
        12_200.0,
        282.0,
        269.0,
        50.0,
        3.60,
        0.81
    ),
    nass_row!(
        "IN",
        "Indiana",
        "Corn Belt",
        10_200.0,
        9_200.0,
        3_400.0,
        11_500.0,
        278.0,
        245.0,
        48.0,
        3.65,
        0.76
    ),
    nass_row!(
        "OH",
        "Ohio",
        "Corn Belt",
        9_400.0,
        8_700.0,
        3_900.0,
        11_200.0,
        235.0,
        195.0,
        38.0,
        3.70,
        0.71
    ),
    nass_row!(
        "MO",
        "Missouri",
        "Corn Belt",
        6_800.0,
        4_950.0,
        2_750.0,
        8_600.0,
        215.0,
        158.0,
        46.0,
        4.05,
        0.64
    ),
    // Lake States
    nass_row!(
        "MN",
        "Minnesota",
        "Lake States",
        8_200.0,
        7_150.0,
        2_450.0,
        10_200.0,
        245.0,
        206.0,
        36.0,
        3.85,
        0.72
    ),
    nass_row!(
        "WI",
        "Wisconsin",
        "Lake States",
        7_900.0,
        6_600.0,
        3_250.0,
        10_400.0,
        255.0,
        172.0,
        42.0,
        3.90,
        0.66
    ),
    nass_row!(
        "MI",
        "Michigan",
        "Lake States",
        7_600.0,
        6_150.0,
        3_100.0,
        10_900.0,
        248.0,
        162.0,
        35.0,
        3.95,
        0.63
    ),
    // Northern Plains
    nass_row!(
        "NE",
        "Nebraska",
        "Northern Plains",
        8_950.0,
        5_650.0,
        1_420.0,
        9_800.0,
        295.0,
        178.0,
        29.0,
        4.10,
        0.62
    ),
    nass_row!(
        "KS",
        "Kansas",
        "Northern Plains",
        4_550.0,
        3_250.0,
        1_920.0,
        6_800.0,
        158.0,
        74.0,
        23.0,
        4.25,
        0.54
    ),
    nass_row!(
        "SD",
        "South Dakota",
        "Northern Plains",
        5_800.0,
        3_850.0,
        1_450.0,
        6_900.0,
        218.0,
        138.0,
        31.0,
        4.20,
        0.56
    ),
    nass_row!(
        "ND",
        "North Dakota",
        "Northern Plains",
        4_400.0,
        2_780.0,
        1_180.0,
        5_600.0,
        175.0,
        82.0,
        22.0,
        4.35,
        0.53
    ),
    // Pacific
    nass_row!(
        "CA",
        "California",
        "Pacific",
        18_200.0,
        7_400.0,
        3_600.0,
        28_500.0,
        490.0,
        85.0,
        22.0,
        4.25,
        0.48
    ),
    nass_row!(
        "WA",
        "Washington",
        "Pacific",
        8_450.0,
        2_150.0,
        980.0,
        17_600.0,
        395.0,
        82.0,
        14.0,
        4.45,
        0.46
    ),
    nass_row!(
        "OR", "Oregon", "Pacific", 7_250.0, 2_950.0, 1_050.0, 16_800.0, 285.0, 105.0, 15.0, 4.35,
        0.47
    ),
    nass_row!(
        "HI", "Hawaii", "Pacific", 14_800.0, 9_600.0, 4_400.0, 22_400.0, 360.0, 140.0, 32.0, 3.80,
        0.52
    ),
    nass_row!(
        "AK", "Alaska", "Pacific", 4_200.0, 2_900.0, 1_150.0, 5_500.0, 110.0, 65.0, 15.0, 4.50,
        0.32
    ),
    // Southern Plains
    nass_row!(
        "TX",
        "Texas",
        "Southern Plains",
        4_650.0,
        3_100.0,
        2_450.0,
        9_500.0,
        128.0,
        52.0,
        14.0,
        3.85,
        0.44
    ),
    nass_row!(
        "OK",
        "Oklahoma",
        "Southern Plains",
        3_850.0,
        2_650.0,
        2_150.0,
        6_400.0,
        112.0,
        46.0,
        17.0,
        4.15,
        0.46
    ),
    // Delta
    nass_row!(
        "AR", "Arkansas", "Delta", 5_150.0, 3_550.0, 3_150.0, 7_800.0, 215.0, 108.0, 25.0, 4.55,
        0.58
    ),
    nass_row!(
        "LA",
        "Louisiana",
        "Delta",
        4_550.0,
        3_750.0,
        3_300.0,
        7_600.0,
        178.0,
        115.0,
        24.0,
        4.60,
        0.59
    ),
    nass_row!(
        "MS",
        "Mississippi",
        "Delta",
        4_850.0,
        3_450.0,
        2_950.0,
        7_200.0,
        205.0,
        112.0,
        23.0,
        4.65,
        0.57
    ),
    // Southeast
    nass_row!(
        "FL",
        "Florida",
        "Southeast",
        9_850.0,
        8_200.0,
        6_800.0,
        15_400.0,
        295.0,
        118.0,
        28.0,
        3.75,
        0.45
    ),
    nass_row!(
        "GA",
        "Georgia",
        "Southeast",
        6_250.0,
        4_650.0,
        4_550.0,
        10_800.0,
        238.0,
        105.0,
        32.0,
        4.25,
        0.52
    ),
    nass_row!(
        "AL",
        "Alabama",
        "Southeast",
        5_100.0,
        3_950.0,
        3_350.0,
        7_900.0,
        172.0,
        86.0,
        26.0,
        4.30,
        0.53
    ),
    nass_row!(
        "SC",
        "South Carolina",
        "Southeast",
        5_400.0,
        4_150.0,
        3_950.0,
        8_600.0,
        178.0,
        78.0,
        26.0,
        4.15,
        0.50
    ),
    // Appalachian
    nass_row!(
        "NC",
        "North Carolina",
        "Appalachian",
        6_600.0,
        5_750.0,
        5_350.0,
        10_600.0,
        168.0,
        108.0,
        32.0,
        3.95,
        0.56
    ),
    nass_row!(
        "VA",
        "Virginia",
        "Appalachian",
        6_800.0,
        5_650.0,
        4_650.0,
        11_800.0,
        162.0,
        86.0,
        27.0,
        3.75,
        0.55
    ),
    nass_row!(
        "TN",
        "Tennessee",
        "Appalachian",
        6_300.0,
        5_350.0,
        4_750.0,
        9_200.0,
        205.0,
        128.0,
        28.0,
        4.05,
        0.58
    ),
    nass_row!(
        "KY",
        "Kentucky",
        "Appalachian",
        6_500.0,
        5_550.0,
        3_950.0,
        9_100.0,
        225.0,
        175.0,
        34.0,
        4.15,
        0.62
    ),
    nass_row!(
        "WV",
        "West Virginia",
        "Appalachian",
        4_500.0,
        3_750.0,
        2_550.0,
        6_800.0,
        110.0,
        58.0,
        18.0,
        3.80,
        0.44
    ),
    // Mountain
    nass_row!(
        "ID", "Idaho", "Mountain", 7_950.0, 2_450.0, 1_120.0, 11_800.0, 285.0, 74.0, 16.0, 4.35,
        0.42
    ),
    nass_row!(
        "CO", "Colorado", "Mountain", 6_450.0, 1_680.0, 1_150.0, 11_200.0, 195.0, 42.0, 11.0, 4.10,
        0.34
    ),
    nass_row!(
        "AZ", "Arizona", "Mountain", 10_900.0, 2_800.0, 850.0, 15_800.0, 350.0, 45.0, 8.0, 4.20,
        0.26
    ),
    nass_row!(
        "MT", "Montana", "Mountain", 4_250.0, 1_380.0, 980.0, 7_400.0, 138.0, 42.0, 10.0, 4.00,
        0.33
    ),
    nass_row!(
        "WY", "Wyoming", "Mountain", 3_450.0, 1_180.0, 780.0, 5_800.0, 118.0, 32.0, 8.0, 3.95, 0.28
    ),
    nass_row!(
        "UT", "Utah", "Mountain", 7_800.0, 2_250.0, 1_280.0, 11_400.0, 165.0, 48.0, 11.0, 3.85,
        0.30
    ),
    nass_row!(
        "NV", "Nevada", "Mountain", 4_950.0, 1_650.0, 680.0, 8_200.0, 168.0, 38.0, 7.0, 4.05, 0.24
    ),
    nass_row!(
        "NM",
        "New Mexico",
        "Mountain",
        5_600.0,
        980.0,
        520.0,
        11_500.0,
        185.0,
        28.0,
        5.0,
        4.10,
        0.22
    ),
    // Northeast
    nass_row!(
        "PA",
        "Pennsylvania",
        "Northeast",
        9_800.0,
        8_450.0,
        4_650.0,
        12_800.0,
        185.0,
        118.0,
        36.0,
        3.45,
        0.58
    ),
    nass_row!(
        "NY",
        "New York",
        "Northeast",
        6_200.0,
        4_450.0,
        2_450.0,
        11_600.0,
        165.0,
        86.0,
        28.0,
        3.75,
        0.54
    ),
    nass_row!(
        "NJ",
        "New Jersey",
        "Northeast",
        17_400.0,
        15_900.0,
        12_800.0,
        22_500.0,
        195.0,
        108.0,
        42.0,
        3.25,
        0.57
    ),
    nass_row!(
        "MD",
        "Maryland",
        "Northeast",
        10_600.0,
        9_450.0,
        7_200.0,
        14_200.0,
        215.0,
        138.0,
        42.0,
        3.45,
        0.61
    ),
    nass_row!(
        "DE",
        "Delaware",
        "Northeast",
        11_800.0,
        10_400.0,
        7_800.0,
        14_800.0,
        225.0,
        142.0,
        44.0,
        3.45,
        0.62
    ),
    nass_row!(
        "CT",
        "Connecticut",
        "Northeast",
        15_200.0,
        13_800.0,
        11_200.0,
        19_500.0,
        175.0,
        98.0,
        38.0,
        3.30,
        0.51
    ),
    nass_row!(
        "MA",
        "Massachusetts",
        "Northeast",
        16_400.0,
        14_600.0,
        11_800.0,
        21_200.0,
        185.0,
        95.0,
        38.0,
        3.25,
        0.49
    ),
    nass_row!(
        "RI",
        "Rhode Island",
        "Northeast",
        19_800.0,
        18_500.0,
        14_500.0,
        24_000.0,
        195.0,
        105.0,
        42.0,
        3.20,
        0.50
    ),
    nass_row!(
        "VT",
        "Vermont",
        "Northeast",
        5_400.0,
        4_350.0,
        2_850.0,
        8_900.0,
        135.0,
        74.0,
        26.0,
        3.60,
        0.48
    ),
    nass_row!(
        "NH",
        "New Hampshire",
        "Northeast",
        6_800.0,
        5_650.0,
        4_100.0,
        10_400.0,
        138.0,
        72.0,
        26.0,
        3.45,
        0.45
    ),
    nass_row!(
        "ME",
        "Maine",
        "Northeast",
        4_600.0,
        3_450.0,
        2_450.0,
        7_800.0,
        132.0,
        68.0,
        22.0,
        3.70,
        0.46
    ),
    nass_row!(
        "DC",
        "District of Columbia",
        "Northeast",
        11_500.0,
        9_800.0,
        7_400.0,
        15_000.0,
        210.0,
        135.0,
        40.0,
        3.40,
        0.56
    ),
];

/// Looks up the canonical USDA NASS state agricultural benchmark (falling back to `"US"`).
pub fn lookup_nass_state_benchmark(state: &str) -> NassStateAgBenchmark {
    let code = state.trim().to_ascii_uppercase();
    NASS_STATE_BENCHMARKS
        .iter()
        .copied()
        .find(|b| b.state_code == code)
        .unwrap_or(NASS_US_FALLBACK)
}

fn num_at(v: &Value, ptr: &str) -> Option<f64> {
    let x = v.pointer(ptr)?;
    if let Some(f) = x.as_f64() {
        if f.is_finite() {
            return Some(f);
        }
    }
    if let Some(i) = x.as_i64() {
        return Some(i as f64);
    }
    if let Some(s) = x.as_str() {
        let clean: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if let Ok(parsed) = clean.parse::<f64>() {
            if parsed.is_finite() {
                return Some(parsed);
            }
        }
    }
    None
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn extract_state_code(report: &Value) -> String {
    str_at(report, "/geo/state")
        .or_else(|| str_at(report, "/facts/state"))
        .or_else(|| str_at(report, "/geo/stateAbbr"))
        .unwrap_or("US")
        .trim()
        .to_ascii_uppercase()
}

fn extract_coords(report: &Value) -> Option<(f64, f64)> {
    let lat = num_at(report, "/geo/lat")
        .or_else(|| num_at(report, "/facts/lat"))
        .or_else(|| num_at(report, "/facts/latitude"))?;
    let lon = num_at(report, "/geo/lon")
        .or_else(|| num_at(report, "/geo/lng"))
        .or_else(|| num_at(report, "/facts/lon"))
        .or_else(|| num_at(report, "/facts/longitude"))?;
    if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
        Some((lat, lon))
    } else {
        None
    }
}

fn capability_num_and_roman(raw: Option<&str>, fallback_num: u8) -> (u8, String) {
    let n = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| {
            let up = s.to_ascii_uppercase();
            let tok = up
                .trim_start_matches("CLASS")
                .trim()
                .trim_end_matches(['E', 'W', 'S', 'C'])
                .trim();
            match tok {
                "1" | "I" => Some(1),
                "2" | "II" => Some(2),
                "3" | "III" => Some(3),
                "4" | "IV" => Some(4),
                "5" | "V" => Some(5),
                "6" | "VI" => Some(6),
                "7" | "VII" => Some(7),
                "8" | "VIII" => Some(8),
                _ => tok
                    .chars()
                    .next()
                    .and_then(|c| c.to_digit(10))
                    .map(|d| d as u8),
            }
        })
        .unwrap_or(fallback_num)
        .clamp(1, 8);
    let roman = match n {
        1 => "I",
        2 => "II",
        3 => "III",
        4 => "IV",
        5 => "V",
        6 => "VI",
        7 => "VII",
        _ => "VIII",
    };
    (n, roman.to_string())
}

fn capability_class_description(class_num: u8) -> &'static str {
    match class_num {
        1 => "Class I — Slight limitations; elite deep, well-drained arable soils",
        2 => "Class II — Moderate limitations; prime arable soils suited to intensive cultivation",
        3 => "Class III — Severe limitations requiring conservation tillage, drainage, or irrigation",
        4 => "Class IV — Very severe limitations; suited to careful rotation, orchards, or hay/forage",
        5 => "Class V — Wetness/flooding or stony floor limits tillage; suited to pasture or bottomland timber",
        6 => "Class VI — Steep or shallow soils unsuited to row tillage; suited to grazing, orchard, or forestry",
        7 => "Class VII — Very steep, arid, or shallow rangeland; grazing or woodland management only",
        _ => "Class VIII — Non-arable rock outcrop, marsh, or steep terrain; conservation/wildlife use only",
    }
}

fn is_prime_farmland_label(farmlndcl: &str) -> bool {
    let low = farmlndcl.to_ascii_lowercase();
    (low.contains("prime farmland") || low.contains("all areas are prime"))
        && !low.contains("not prime")
}

fn usda_sda_cache_key(lat: f64, lon: f64) -> String {
    format!("USDA_SDA:{lat:.5},{lon:.5}")
}

/// Seeds the `HttpClient` cache for `(lat, lon)` with a USDA NRCS SDA JSON payload (useful for
/// deterministic offline integration tests without external network calls).
pub async fn seed_usda_soil_cache(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    sda_response_json: &Value,
) {
    if let Ok(serialized) = serde_json::to_string(sda_response_json) {
        http.write_cache(&usda_sda_cache_key(lat, lon), &serialized, 86_400 * 30)
            .await;
    }
}

/// Parses a USDA NRCS Soil Data Access (`JSON+COLUMNNAME` or object-table) response into a [`UsdaSoilProfile`].
pub fn parse_usda_sda_response(j: &Value) -> Option<UsdaSoilProfile> {
    let table = j.get("Table").and_then(|v| v.as_array())?;
    if table.is_empty() {
        return None;
    }

    let get_cell_str = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() || t.eq_ignore_ascii_case("null") {
                    None
                } else {
                    Some(t.to_string())
                }
            }
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };

    // Format A: `JSON+COLUMNNAME` -> `Table[0]` is column names array, `Table[1]` (or `Table[2]` if metadata row) is values array
    if let Some(cols_arr) = table[0].as_array() {
        if table.len() < 2 {
            return None;
        }
        let col_names: Vec<String> = cols_arr
            .iter()
            .map(|c| {
                c.as_str()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
                    .replace('_', "")
            })
            .collect();

        // Skip optional `JSON+COLUMNNAME+METADATA` descriptor row (`"ColumnOrdinal=0..."`)
        let data_row = table.iter().skip(1).find_map(|r| {
            let arr = r.as_array()?;
            let is_meta = arr
                .first()
                .and_then(|c| c.as_str())
                .map(|s| s.starts_with("ColumnOrdinal="))
                .unwrap_or(false);
            if is_meta {
                None
            } else {
                Some(arr)
            }
        })?;

        let lookup = |aliases: &[&str]| -> Option<String> {
            for target in aliases {
                let norm = target.to_ascii_lowercase().replace('_', "");
                if let Some(idx) = col_names.iter().position(|c| c == &norm) {
                    if let Some(cell) = data_row.get(idx) {
                        if let Some(val) = get_cell_str(cell) {
                            return Some(val);
                        }
                    }
                }
            }
            None
        };

        let mukey = lookup(&["mukey"]);
        let muname = lookup(&["muname", "mapunitname"])?;
        let farmlndcl = lookup(&["farmlndcl", "primefarmlandclass"])
            .unwrap_or_else(|| "Not prime farmland".to_string());
        let irrcapcl_raw = lookup(&["irrcapcl", "iccdcd", "irrigatedcapabilityclass"]);
        let nirrcapcl_raw = lookup(&["nirrcapcl", "niccdcd", "nonirrigatedcapabilityclass"]);
        let nccpi_raw = lookup(&["nccpioverall", "nccpi3all", "nccpi"]);
        let drclass = lookup(&["drclassdcd", "drainageclass"])
            .unwrap_or_else(|| "Moderately well drained".to_string());
        let hydric = lookup(&["hydricrating", "hydclprs"]).unwrap_or_else(|| "No".to_string());
        let slope_raw = lookup(&["slopegraddcp", "sloper", "slopepct"]);

        let is_prime = is_prime_farmland_label(&farmlndcl);
        let default_cap = if is_prime { 2 } else { 3 };
        let (cap_num, nirrcap_roman) =
            capability_num_and_roman(nirrcapcl_raw.as_deref(), default_cap);
        let irrcap_roman = irrcapcl_raw
            .as_deref()
            .map(|r| capability_num_and_roman(Some(r), cap_num).1);

        let default_nccpi = match cap_num {
            1 => 0.84,
            2 => 0.72,
            3 => 0.56,
            4 => 0.42,
            5 => 0.34,
            6 => 0.24,
            7 => 0.15,
            _ => 0.06,
        };
        let nccpi = nccpi_raw
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .map(|v| if v > 1.0 && v <= 100.0 { v / 100.0 } else { v })
            .unwrap_or(default_nccpi)
            .clamp(0.02, 0.99);

        let slope_pct = slope_raw
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(if cap_num <= 2 { 1.8 } else { 4.5 })
            .clamp(0.0, 75.0);

        return Some(UsdaSoilProfile {
            data_source: "usda_nrcs_ssurgo_live".to_string(),
            mukey,
            map_unit_name: muname,
            prime_farmland_class: farmlndcl,
            is_prime_farmland: is_prime,
            irrigated_capability_class: irrcap_roman,
            non_irrigated_capability_class: nirrcap_roman,
            capability_class_num: cap_num,
            nccpi_overall: round_n(nccpi, 3),
            drainage_class: drclass,
            hydric_rating: hydric,
            slope_pct: round_n(slope_pct, 1),
        });
    }

    // Format B: Object rows `Table[0]` = `{"mukey": "...", "muname": "...", ...}`
    let obj = table[0].as_object()?;
    let lookup_obj = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(v) = obj.get(*k).and_then(get_cell_str) {
                return Some(v);
            }
        }
        None
    };

    let mukey = lookup_obj(&["mukey", "MUKEY"]);
    let muname = lookup_obj(&["muname", "mapUnitName", "MUNAME"])?;
    let farmlndcl = lookup_obj(&["farmlndcl", "primeFarmlandClass"])
        .unwrap_or_else(|| "Not prime farmland".to_string());
    let irrcapcl_raw = lookup_obj(&["irrcapcl", "iccdcd", "irrigatedCapabilityClass"]);
    let nirrcapcl_raw = lookup_obj(&["nirrcapcl", "niccdcd", "nonIrrigatedCapabilityClass"]);
    let is_prime = is_prime_farmland_label(&farmlndcl);
    let (cap_num, nirrcap_roman) =
        capability_num_and_roman(nirrcapcl_raw.as_deref(), if is_prime { 2 } else { 3 });
    let irrcap_roman = irrcapcl_raw
        .as_deref()
        .map(|r| capability_num_and_roman(Some(r), cap_num).1);
    let nccpi = lookup_obj(&["nccpi_overall", "nccpiOverall", "nccpi3all"])
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(|v| if v > 1.0 && v <= 100.0 { v / 100.0 } else { v })
        .unwrap_or(if is_prime { 0.72 } else { 0.50 })
        .clamp(0.02, 0.99);
    let slope_pct = lookup_obj(&["slopegraddcp", "slope_r", "slopePct"])
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(2.0)
        .clamp(0.0, 75.0);

    Some(UsdaSoilProfile {
        data_source: "usda_nrcs_ssurgo_live".to_string(),
        mukey,
        map_unit_name: muname,
        prime_farmland_class: farmlndcl,
        is_prime_farmland: is_prime,
        irrigated_capability_class: irrcap_roman,
        non_irrigated_capability_class: nirrcap_roman,
        capability_class_num: cap_num,
        nccpi_overall: round_n(nccpi, 3),
        drainage_class: lookup_obj(&["drclassdcd", "drainageClass"])
            .unwrap_or_else(|| "Moderately well drained".to_string()),
        hydric_rating: lookup_obj(&["hydric_rating", "hydricRating", "hydclprs"])
            .unwrap_or_else(|| "No".to_string()),
        slope_pct: round_n(slope_pct, 1),
    })
}

/// Queries the keyless public-domain USDA NRCS Soil Data Access REST endpoint
/// (`https://SDMDataAccess.sc.egov.usda.gov/Tabular/post.rest`) for the SSURGO soil map unit at
/// `(lat, lon)`. Returns `None` gracefully on offline/error without panicking or blocking tests.
pub async fn fetch_usda_soil_profile(
    http: &HttpClient,
    lat: f64,
    lon: f64,
) -> Option<UsdaSoilProfile> {
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return None;
    }

    let cache_key = usda_sda_cache_key(lat, lon);
    if !http.is_bypassing_cache_reads() {
        if let Some(hit) = http.read_cache(&cache_key, 86_400 * 30).await {
            if let Ok(j) = serde_json::from_str::<Value>(&hit) {
                if let Some(parsed) = parse_usda_sda_response(&j) {
                    return Some(parsed);
                }
            }
        }
    }

    let endpoint = http
        .env()
        .get("USDA_SDA_API_URL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(USDA_SDA_ENDPOINT);

    if http.is_offline_blocked() {
        let is_loopback = Url::parse(endpoint)
            .ok()
            .and_then(|u| u.host_str().map(HttpClient::is_loopback_host))
            .unwrap_or(false);
        if !is_loopback {
            return None;
        }
    }

    let sda_sql = format!(
        "SELECT TOP 1 \
            mu.mukey, \
            mu.muname, \
            mu.farmlndcl, \
            COALESCE(c.irrcapcl, ma.iccdcd) AS irrcapcl, \
            COALESCE(c.nirrcapcl, ma.niccdcd) AS nirrcapcl, \
            COALESCE(c.nccpi3all, c.nccpi3corn, c.nccpi3soy, c.nccpi3cot, c.nccpi3sg) AS nccpi_overall, \
            ma.drclassdcd, \
            COALESCE(c.hydricrating, ma.hydclprs) AS hydric_rating, \
            COALESCE(ma.slopegraddcp, c.slope_r) AS slopegraddcp \
         FROM SDA_Get_Mukey_from_intersection_with_WktWgs84('point({lon:.6} {lat:.6})') AS p \
         INNER JOIN mapunit AS mu ON mu.mukey = p.mukey \
         LEFT JOIN muaggatt AS ma ON ma.mukey = mu.mukey \
         LEFT JOIN component AS c ON c.mukey = mu.mukey \
         ORDER BY CASE WHEN c.majcompflag = 'Yes' THEN 0 ELSE 1 END, c.comppct_r DESC"
    );

    let payload = json!({
        "query": sda_sql,
        "format": "JSON+COLUMNNAME",
    });

    let resp = http
        .scrape_post(endpoint)
        .timeout_ms(5_000)
        .retries(0)
        .json_value_body(&payload)
        .json_value()
        .await
        .ok()?;

    let profile = parse_usda_sda_response(&resp)?;
    if let Ok(serialized) = serde_json::to_string(&resp) {
        http.write_cache(&cache_key, &serialized, 86_400 * 30).await;
    }
    Some(profile)
}

/// Deterministic, transparently labelled regional soil baseline (`modeled_regional_baseline`) used
/// when live USDA NRCS Soil Data Access is unavailable or when running offline tests.
pub fn infer_baseline_soil_profile(report: &Value, state: &str) -> UsdaSoilProfile {
    let bench = lookup_nass_state_benchmark(state);
    let terrain_slope = num_at(report, "/modules/fengshui/data/terrain/slopePct")
        .or_else(|| num_at(report, "/modules/house3d/data/lot/slopePct"))
        .unwrap_or_else(|| match bench.region {
            "Corn Belt" | "Delta" | "Southeast" => 1.8,
            "Northern Plains" | "Southern Plains" | "Lake States" => 2.6,
            "Pacific" | "Appalachian" | "Northeast" => 4.2,
            "Mountain" => 5.8,
            _ => 2.5,
        })
        .clamp(0.2, 45.0);

    let in_sfha = report
        .pointer("/modules/hazards/data/flood/sfha")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut cap_num: u8 = if terrain_slope > 15.0 {
        6
    } else if terrain_slope > 8.0 {
        4
    } else if bench.baseline_nccpi >= 0.70 {
        2
    } else if bench.baseline_nccpi >= 0.45 {
        3
    } else {
        4
    };
    if in_sfha && cap_num < 4 {
        cap_num = 4;
    }

    let (_, cap_roman) = capability_num_and_roman(None, cap_num);
    let irr_cap = if matches!(
        bench.region,
        "Pacific" | "Mountain" | "Northern Plains" | "Delta"
    ) && terrain_slope <= 8.0
    {
        Some(capability_num_and_roman(None, (cap_num.saturating_sub(1)).max(1)).1)
    } else {
        None
    };

    let slope_factor = if terrain_slope <= 3.0 {
        1.0
    } else if terrain_slope <= 8.0 {
        0.92
    } else if terrain_slope <= 15.0 {
        0.78
    } else {
        0.58
    };
    let nccpi = round_n((bench.baseline_nccpi * slope_factor).clamp(0.12, 0.92), 3);

    let is_prime = cap_num <= 2 && !in_sfha && terrain_slope <= 6.0;
    let prime_label = if is_prime {
        "All areas are prime farmland (modeled regional baseline)".to_string()
    } else if irr_cap.as_deref() == Some("I") || irr_cap.as_deref() == Some("II") {
        "Prime farmland if irrigated (modeled regional baseline)".to_string()
    } else if cap_num == 3 {
        "Farmland of statewide importance (modeled regional baseline)".to_string()
    } else {
        "Not prime farmland — grazing, specialty contour, or dryland class (modeled baseline)"
            .to_string()
    };

    let map_unit_name = format!(
        "{} ({}) regional loam/silt baseline, {:.1}% modeled slope",
        bench.state_name, bench.region, terrain_slope
    );

    let drainage = if in_sfha {
        "Somewhat poorly drained (floodplain / alluvial terrace)".to_string()
    } else if terrain_slope >= 6.0 {
        "Well drained".to_string()
    } else {
        "Moderately well drained".to_string()
    };

    let hydric = if in_sfha {
        "Partially hydric (alluvial floodplain)".to_string()
    } else {
        "Non-hydric".to_string()
    };

    UsdaSoilProfile {
        data_source: "modeled_regional_baseline".to_string(),
        mukey: None,
        map_unit_name,
        prime_farmland_class: prime_label,
        is_prime_farmland: is_prime,
        irrigated_capability_class: irr_cap,
        non_irrigated_capability_class: cap_roman,
        capability_class_num: cap_num,
        nccpi_overall: nccpi,
        drainage_class: drainage,
        hydric_rating: hydric,
        slope_pct: round_n(terrain_slope, 1),
    }
}

fn normalize_farming_mode(
    raw: Option<&str>,
    report: &Value,
    state: &str,
    soil: &UsdaSoilProfile,
) -> &'static str {
    if let Some(m) = raw.map(str::trim).filter(|s| !s.is_empty()) {
        let low = m.to_ascii_lowercase();
        if low.contains("orchard")
            || low.contains("vineyard")
            || low.contains("grove")
            || low.contains("nut")
            || low.contains("permanent")
            || low.contains("wine")
        {
            return "orchard_vineyard";
        }
        if low.contains("irrigat")
            || low.contains("specialty")
            || low.contains("veg")
            || low.contains("produce")
        {
            return "irrigated_specialty";
        }
        if low.contains("pasture")
            || low.contains("ranch")
            || low.contains("cattle")
            || low.contains("graz")
            || low.contains("livestock")
            || low.contains("hay")
        {
            return "pasture_ranch";
        }
        if low.contains("row")
            || low.contains("crop")
            || low.contains("grain")
            || low.contains("corn")
            || low.contains("soy")
        {
            return "row_crops";
        }
    }

    // Check parcel description hints
    let facts = report.get("facts").unwrap_or(&Value::Null);
    let desc = [
        str_at(facts, "/propertyType"),
        str_at(facts, "/propertySubType"),
        str_at(facts, "/useDescription"),
        str_at(facts, "/landUseDescription"),
        str_at(facts, "/landUse"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();

    if desc.contains("orchard") || desc.contains("vineyard") || desc.contains("grove") {
        return "orchard_vineyard";
    }
    if desc.contains("ranch") || desc.contains("pasture") || desc.contains("grazing") {
        return "pasture_ranch";
    }
    if desc.contains("irrigated") || desc.contains("vegetable") {
        return "irrigated_specialty";
    }

    if soil.capability_class_num >= 5 || soil.slope_pct > 14.0 {
        return "pasture_ranch";
    }
    if matches!(state, "CA" | "WA" | "OR" | "FL" | "HI") && soil.capability_class_num <= 3 {
        if soil.irrigated_capability_class.is_some() {
            return "irrigated_specialty";
        }
        return "orchard_vineyard";
    }
    "row_crops"
}

fn state_ag_tax_program_details(state: &str) -> (&'static str, f64, &'static str, &'static str) {
    // Returns (program_name, effective_ag_tax_rate_pct_of_ag_val, rollback_note, summary)
    match state {
        "CA" => (
            "California Williamson Act (Land Conservation Act of 1965)",
            0.52,
            "10-year rolling contract; non-renewal phases taxes up over 9 years or 12.5% cancellation fee on unrestricted FMV.",
            "Assesses contracted agricultural & open-space land on capitalized agricultural income rather than Prop 13 market purchase price, reducing annual property tax by 45%–70%.",
        ),
        "TX" => (
            "Texas Tax Code Ch. 23 Subch. D (1-d-1 Open-Space Agricultural Appraisal)",
            0.18,
            "3-year change-of-use rollback recapture plus 5% per annum interest on the difference between market and productivity tax.",
            "Taxes qualifying cropland, orchard, grazing, or wildlife-management acreage (5-of-7-year ag history) on net-to-land agricultural productivity value instead of market value.",
        ),
        "FL" => (
            "Florida Statute § 193.461 Agricultural Greenbelt Classification",
            0.35,
            "Annual bona-fide commercial agricultural use verification by County Property Appraiser (March 1 filing deadline).",
            "Classifies bona-fide commercial farm, grove, timber, and ranch land on capitalized agricultural use value, shielding peri-urban acreage from speculative ad valorem assessments.",
        ),
        "IA" | "IL" | "IN" | "OH" | "MN" | "WI" | "MI" | "NE" | "KS" | "SD" | "ND" | "MO" => (
            "Midwest / Plains Soil-Productivity Agricultural Use-Value Assessment",
            0.68,
            "Automatic classification for active agricultural parcels; conversion to commercial/residential triggers market reassessment.",
            "Assesses farmland via statutory formula capitalizing 5-year rolling net crop/pasture income indexed by USDA soil productivity ratings (CSR2 / PI / NCCPI).",
        ),
        "NY" => (
            "New York Agricultural Districts Law (AML Art. 25-AA)",
            0.55,
            "5-year conversion payment (5x last year's tax savings + 6% interest) if converted to non-ag use within an Ag District.",
            "Caps taxable soil valuation using NYS Dept. of Taxation & Finance mineral/organic soil group base agricultural assessment values.",
        ),
        "OR" | "WA" => (
            "Pacific Northwest Exclusive Farm Use (EFU) / Open-Space Farm & Ag Tax Deferral",
            0.42,
            "7-to-10-year back-tax rollback plus interest/penalty upon withdrawal from farm/agricultural classification.",
            "Defers market-value tax differential so active orchards, vineyards, irrigated row crops, and rangeland pay property tax strictly on farm use value.",
        ),
        _ => (
            "State Agricultural Use-Value / Differential Property Tax Assessment",
            0.48,
            "Change of use to non-agricultural development typically triggers a 3-to-7-year rollback recapture of deferred ad valorem taxes.",
            "All 50 US states provide statutory use-value or current-use property tax assessment for bona-fide agricultural acreage based on productive income capacity rather than speculative development value.",
        ),
    }
}

fn calc_annual_debt_service(principal: f64, annual_rate_pct: f64, years: u32) -> f64 {
    if principal <= 0.0 || years == 0 {
        return 0.0;
    }
    let r = (annual_rate_pct / 100.0) / 12.0;
    let n = (years * 12) as f64;
    if r <= 1e-7 {
        return principal / (years as f64);
    }
    let pmt = principal * (r * (1.0 + r).powf(n)) / ((1.0 + r).powf(n) - 1.0);
    pmt * 12.0
}

fn resolve_farmland_acreage(
    report: &Value,
    opts: Option<&LandFarmEvalOptions>,
) -> Option<(f64, &'static str)> {
    if let Some(ac) = opts
        .and_then(|o| o.acreage)
        .filter(|&a| a.is_finite() && a > 0.0)
    {
        return Some((ac.clamp(0.25, 100_000.0), "user_override_acreage"));
    }
    let facts = report.get("facts").unwrap_or(&Value::Null);
    if let Some(ac) = num_at(facts, "/lotAcres")
        .or_else(|| num_at(facts, "/acres"))
        .filter(|&a| a.is_finite() && a > 0.0)
    {
        return Some((ac.clamp(0.1, 100_000.0), "parcel_assessor_acres"));
    }
    if let Some(lsf) = num_at(facts, "/lotSqft")
        .or_else(|| num_at(facts, "/landSqft"))
        .filter(|&s| s.is_finite() && s > 0.0)
    {
        return Some((
            (lsf / SQFT_PER_ACRE).clamp(0.05, 100_000.0),
            "parcel_lot_sqft",
        ));
    }
    None
}

/// Evaluates Agricultural / Farmland value, soil & agronomy suitability, 4-mode farm operating
/// P&L + landlord NNN cash rent, peri-urban transition option, ag tax exemptions, and USDA FSA /
/// Farm Credit System financing.
pub fn evaluate_farmland(
    report: &Value,
    soil: UsdaSoilProfile,
    opts: Option<&LandFarmEvalOptions>,
) -> Value {
    let state = extract_state_code(report);
    let bench = lookup_nass_state_benchmark(&state);
    let selected_mode = normalize_farming_mode(
        opts.and_then(|o| o.farming_mode.as_deref()),
        report,
        &state,
        &soil,
    );

    let Some((lot_acres, acreage_basis)) = resolve_farmland_acreage(report, opts) else {
        return json!({
            "status": "missing_parcel_area",
            "state": state,
            "lotAcres": Value::Null,
            "lotSqft": Value::Null,
            "acreageBasis": "missing_parcel_area",
            "selectedFarmingMode": selected_mode,
            "soilAndAgronomy": {
                "dataSource": soil.data_source,
                "mukey": soil.mukey,
                "mapUnitName": soil.map_unit_name,
                "primeFarmlandClass": soil.prime_farmland_class,
                "isPrimeFarmland": soil.is_prime_farmland,
                "irrigatedCapabilityClass": soil.irrigated_capability_class,
                "nonIrrigatedCapabilityClass": soil.non_irrigated_capability_class,
                "capabilityClassNum": soil.capability_class_num,
                "capabilityLabel": capability_class_description(soil.capability_class_num),
                "nccpiOverall": soil.nccpi_overall,
                "drainageClass": soil.drainage_class,
                "hydricRating": soil.hydric_rating,
                "slopePct": soil.slope_pct,
            },
            "valuation": Value::Null,
            "operatingModes": [],
            "agTaxAndFinancing": Value::Null,
        });
    };
    let lot_sqft = lot_acres * SQFT_PER_ACRE;

    let asking_price = opts
        .and_then(|o| o.asking_price)
        .filter(|&p| p > 0.0)
        .or_else(|| num_at(report, "/facts/listPrice").filter(|&p| p > 0.0))
        .or_else(|| num_at(report, "/facts/price").filter(|&p| p > 0.0));

    // Hazard & environmental signals
    let in_sfha = report
        .pointer("/modules/hazards/data/flood/sfha")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let flood_zone = str_at(report, "/modules/hazards/data/flood/zone").unwrap_or("X");
    let drought_rating = str_at(report, "/modules/hazards/data/nri/hazards/DRGT/rating")
        .or_else(|| str_at(report, "/modules/hazards/data/drought/rating"))
        .unwrap_or("Moderate");
    let high_drought = {
        let dl = drought_rating.to_ascii_lowercase();
        dl.contains("very high") || dl.contains("relatively high") || dl.contains("severe")
    };

    // Soil quality productivity multiplier relative to state NASS cropland baseline
    let nccpi_ratio = (soil.nccpi_overall / bench.baseline_nccpi.max(0.20)).clamp(0.62, 1.45);
    let prime_adj = if soil.is_prime_farmland { 1.08 } else { 1.0 };
    let cap_adj = match soil.capability_class_num {
        1 => 1.12,
        2 => 1.04,
        3 => 0.96,
        4 => 0.85,
        5 => 0.74,
        6 => 0.65,
        7 => 0.55,
        _ => 0.42,
    };
    let soil_quality_mult =
        round_n((0.55 * nccpi_ratio + 0.45 * cap_adj) * prime_adj, 3).clamp(0.45, 1.55);

    // Hazard / terrain adjustment (%)
    let mut haz_adj_pct: f64 = 0.0;
    if in_sfha {
        haz_adj_pct -= 8.5; // crop flood inundation / crop-insurance deductibles
    }
    if high_drought && soil.irrigated_capability_class.is_none() {
        haz_adj_pct -= 6.5; // dryland drought yield volatility
    }
    if soil.slope_pct > 12.0 && selected_mode != "pasture_ranch" {
        haz_adj_pct -= 7.0;
    }

    // Base benchmark $/acre by farming mode
    let base_bench_per_acre = match selected_mode {
        "irrigated_specialty" => bench.irrigated_cropland_per_acre,
        "orchard_vineyard" => bench.specialty_orchard_vineyard_per_acre,
        "pasture_ranch" => bench.pasture_rangeland_per_acre,
        _ => bench.non_irrigated_cropland_per_acre,
    };

    let mode_soil_mult = if selected_mode == "pasture_ranch" {
        // Rangeland is less sensitive to row-crop NCCPI than tillable cropland
        (0.75 + 0.25 * soil_quality_mult).clamp(0.68, 1.25)
    } else {
        soil_quality_mult
    };

    let soil_adj_per_acre = round_n(
        (base_bench_per_acre * mode_soil_mult * (1.0 + haz_adj_pct / 100.0)).max(250.0),
        0,
    );
    let comp_mid = round_n(soil_adj_per_acre * lot_acres, 0);
    let comp_low = round_n(comp_mid * 0.88, 0);
    let comp_high = round_n(comp_mid * 1.14, 0);

    // Cash-rent capitalized valuation
    let base_cash_rent_ac = match selected_mode {
        "irrigated_specialty" => bench.irrigated_cash_rent_per_acre_yr,
        "orchard_vineyard" => (bench.irrigated_cash_rent_per_acre_yr * 1.55)
            .max(bench.non_irrigated_cash_rent_per_acre_yr * 2.1),
        "pasture_ranch" => bench.pasture_cash_rent_per_acre_yr,
        _ => bench.non_irrigated_cash_rent_per_acre_yr,
    };
    let adj_cash_rent_ac = round_n((base_cash_rent_ac * mode_soil_mult).max(6.0), 2);
    let gross_cash_rent = round_n(adj_cash_rent_ac * lot_acres, 0);

    let (_, eff_ag_tax_pct, _, _) = state_ag_tax_program_details(&state);
    let landlord_tax_ins_per_acre =
        round_n((soil_adj_per_acre * (eff_ag_tax_pct / 100.0)) + 4.50, 2);
    let net_landlord_noi_per_acre =
        (adj_cash_rent_ac - landlord_tax_ins_per_acre * 0.65).max(adj_cash_rent_ac * 0.76);
    let net_landlord_noi = round_n(net_landlord_noi_per_acre * lot_acres, 0);
    let ag_cap_rate = bench.ag_cap_rate_pct.clamp(3.2, 5.2);
    let cash_rent_cap_val = round_n(net_landlord_noi / (ag_cap_rate / 100.0), 0);
    let cash_rent_cap_per_acre = round_n(cash_rent_cap_val / lot_acres.max(0.01), 0);

    // Peri-urban / Path-of-Growth transition option check
    let amen = report
        .pointer("/modules/amenities/data")
        .unwrap_or(&Value::Null);
    let pois_one_mi = amen
        .pointer("/counts/shopping/oneMile")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        + amen
            .pointer("/counts/grocery/oneMile")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
        + amen
            .pointer("/counts/restaurant/oneMile")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
    let modeled_aadt =
        num_at(report, "/cre/tradeArea/frontageTraffic/modeledAadtMid").unwrap_or(0.0);
    let tract_pop = num_at(report, "/modules/market/data/acs/tract/population").unwrap_or(0.0);
    let zip_zhvi = num_at(report, "/modules/market/data/zhvi/value")
        .or_else(|| num_at(report, "/modules/market/data/acs/zip/medianHomeValue"))
        .unwrap_or(0.0);

    let is_peri_urban = pois_one_mi >= 4
        || modeled_aadt >= 8_500.0
        || (tract_pop >= 3_200.0 && zip_zhvi >= 420_000.0);
    let transition_premium_pct = if is_peri_urban {
        let mut prem: f64 = 18.0;
        if pois_one_mi >= 12 || modeled_aadt >= 16_000.0 {
            prem += 14.0;
        }
        if zip_zhvi >= 650_000.0 {
            prem += 10.0;
        }
        prem.clamp(15.0, 45.0)
    } else {
        0.0
    };
    let transition_option_per_acre =
        round_n(soil_adj_per_acre * (transition_premium_pct / 100.0), 0);
    let transition_adjusted_total = round_n(
        (soil_adj_per_acre + transition_option_per_acre) * lot_acres,
        0,
    );
    let transition_rationale = if is_peri_urban {
        format!(
            "Edge-of-metro transitional upside (+{transition_premium_pct:.0}%): parcel sits near commercial corridor / suburban boundary ({pois_one_mi} retail/dining POIs within 1 mi{}), creating long-term residential/commercial entitlement optionality above pure agricultural value.",
            if modeled_aadt > 0.0 {
                format!(", ~{modeled_aadt:.0} AADT frontage")
            } else {
                String::new()
            }
        )
    } else {
        "Pure rural agricultural trade area — valued primarily on soil productivity, USDA NASS farmland comparables, and cash-rent income without speculative suburban conversion premium.".to_string()
    };

    // Blended Farmland Estimate (65% Soil-Adjusted USDA NASS Comparable + 35% Cash-Rent Income Capitalization + 50% risk-weighted peri-urban option)
    let core_ag_val = 0.65 * comp_mid + 0.35 * cash_rent_cap_val;
    let blended_mid = round_n(
        core_ag_val + 0.50 * (transition_option_per_acre * lot_acres),
        0,
    );
    let blended_low = round_n(blended_mid * 0.87, 0);
    let blended_high = round_n(blended_mid * 1.15, 0);
    let blended_per_acre = round_n(blended_mid / lot_acres.max(0.01), 0);
    let blended_per_sqft = round_n(blended_mid / lot_sqft.max(1.0), 2);

    let confidence = if soil.data_source == "usda_nrcs_ssurgo_live" {
        "high"
    } else {
        "medium"
    };

    let price_vs_est_pct =
        asking_price.map(|ask| round_n(((ask - blended_mid) / blended_mid.max(1.0)) * 100.0, 1));

    // 4-Mode Farm Operating Profit & Landlord NNN Cash-Lease Comparison
    let tillable_ratio = match soil.capability_class_num {
        1 | 2 => 0.92,
        3 => 0.86,
        4 => 0.76,
        _ => 0.55,
    };
    let grazing_ratio = 0.94_f64;

    let build_mode = |code: &'static str,
                      label: &'static str,
                      usable_ratio: f64,
                      base_gross_ac: f64,
                      base_opex_ac: f64,
                      cash_rent_ac: f64,
                      estab_capex_ac: f64,
                      suitability: &'static str|
     -> Value {
        let usable_acres = round_n(lot_acres * usable_ratio, 2);
        let yield_mult = if code == "pasture_ranch" {
            (0.80 + 0.25 * soil_quality_mult).clamp(0.70, 1.20)
        } else {
            soil_quality_mult.clamp(0.55, 1.45)
        };
        let gross_ac = round_n(base_gross_ac * yield_mult, 0);
        let opex_ac = round_n(base_opex_ac * (0.88 + 0.12 * yield_mult), 0);
        let net_op_ac = round_n(gross_ac - opex_ac, 0);
        let total_gross = round_n(gross_ac * usable_acres, 0);
        let total_opex = round_n(opex_ac * usable_acres, 0);
        let total_profit = round_n(net_op_ac * usable_acres, 0);
        let margin_pct = if gross_ac > 0.0 {
            round_n((net_op_ac / gross_ac) * 100.0, 1)
        } else {
            0.0
        };
        let nnn_rent_ac = round_n(cash_rent_ac * yield_mult, 2);
        let nnn_annual_total = round_n(nnn_rent_ac * usable_acres, 0);

        json!({
            "modeCode": code,
            "modeLabel": label,
            "isSelected": code == selected_mode,
            "suitabilityRating": suitability,
            "tillableOrUsableAcres": usable_acres,
            "grossRevenuePerAcre": gross_ac,
            "operatingCostPerAcre": opex_ac,
            "operatorNetProfitPerAcre": net_op_ac,
            "totalAnnualGrossRevenue": total_gross,
            "totalAnnualOperatingCost": total_opex,
            "totalAnnualFarmProfit": total_profit,
            "operatorProfitMarginPct": margin_pct,
            "landlordNnnCashRentPerAcre": nnn_rent_ac,
            "landlordAnnualNnnCashIncome": nnn_annual_total,
            "typicalCapexEstablishmentPerAcre": estab_capex_ac,
        })
    };

    let row_suit = if soil.capability_class_num <= 2 && soil.slope_pct <= 6.0 {
        "optimal"
    } else if soil.capability_class_num <= 4 && soil.slope_pct <= 10.0 {
        "viable"
    } else {
        "marginal"
    };
    let irr_suit = if soil.irrigated_capability_class.is_some() || soil.capability_class_num <= 2 {
        "optimal"
    } else if soil.capability_class_num <= 3 && soil.slope_pct <= 8.0 {
        "viable"
    } else {
        "marginal"
    };
    let orch_suit = if matches!(
        state.as_str(),
        "CA" | "WA" | "OR" | "FL" | "MI" | "NY" | "TX" | "GA" | "HI"
    ) && soil.capability_class_num <= 3
        && !in_sfha
    {
        "optimal"
    } else if soil.capability_class_num <= 4 && !in_sfha {
        "viable"
    } else {
        "marginal"
    };
    let past_suit = if soil.capability_class_num >= 4 {
        "optimal"
    } else {
        "viable"
    };

    // Calibrate regional crop gross/cost to state NASS productivity & water profile
    let reg_crop_scale = (bench.non_irrigated_cash_rent_per_acre_yr / 165.0).clamp(0.65, 1.45);
    let reg_irr_scale = (bench.irrigated_cash_rent_per_acre_yr / 260.0).clamp(0.75, 1.65);

    let operating_modes = vec![
        build_mode(
            "row_crops",
            "Row Crops (Corn / Soybeans / Wheat / Cotton)",
            tillable_ratio,
            940.0 * reg_crop_scale,
            655.0 * reg_crop_scale,
            bench.non_irrigated_cash_rent_per_acre_yr,
            180.0,
            row_suit,
        ),
        build_mode(
            "irrigated_specialty",
            "Irrigated Specialty & Fresh-Market Produce",
            tillable_ratio,
            4_650.0 * reg_irr_scale,
            3_420.0 * reg_irr_scale,
            bench.irrigated_cash_rent_per_acre_yr,
            1_850.0,
            irr_suit,
        ),
        build_mode(
            "orchard_vineyard",
            "Permanent Orchard / Vineyard (Nut, Tree Fruit, Winegrape)",
            (tillable_ratio * 0.95).clamp(0.50, 0.90),
            7_400.0 * reg_irr_scale,
            5_150.0 * reg_irr_scale,
            (bench.irrigated_cash_rent_per_acre_yr * 1.55).max(320.0),
            14_500.0,
            orch_suit,
        ),
        build_mode(
            "pasture_ranch",
            "Pasture, Hay Forage & Cow-Calf Ranching",
            grazing_ratio,
            245.0,
            162.0,
            bench.pasture_cash_rent_per_acre_yr,
            95.0,
            past_suit,
        ),
    ];

    let selected_operator_profit = operating_modes
        .iter()
        .find(|m| m.get("isSelected").and_then(|v| v.as_bool()) == Some(true))
        .and_then(|m| m.get("totalAnnualFarmProfit").and_then(|v| v.as_f64()))
        .unwrap_or(net_landlord_noi);

    // Terrain & Water / Irrigation notes
    let terrain_suitability = if soil.slope_pct <= 3.0 {
        format!(
            "Flat to Nearly Level ({:.1}% slope): Ideal for precision mechanized row crops, laser-leveled basin/furrow, or center-pivot irrigation.",
            soil.slope_pct
        )
    } else if soil.slope_pct <= 8.0 {
        format!(
            "Gentle Roll ({:.1}% slope): Well suited for no-till/contour row crops, drip-irrigated orchards, vineyards, and hayfields.",
            soil.slope_pct
        )
    } else if soil.slope_pct <= 15.0 {
        format!(
            "Moderate Slope ({:.1}% slope): Terraced vineyard/tree orchard or rotational pasture recommended; elevated sheet/rill erosion risk under conventional tillage.",
            soil.slope_pct
        )
    } else {
        format!(
            "Steep Terrain ({:.1}% slope): Suited to livestock grazing, rangeland, or forestry; unsuitable for conventional mechanized row tillage.",
            soil.slope_pct
        )
    };

    let water_irrigation_note = {
        let irr_str = match soil.irrigated_capability_class.as_deref() {
            Some(ic) => format!("USDA Irrigated Capability Class {ic} rated"),
            None => "Primarily non-irrigated / rain-fed soil rating in SSURGO".to_string(),
        };
        let state_water_regime = match state.as_str() {
            "CA" => "verify Sustainable Groundwater Management Act (SGMA) Groundwater Sustainability Agency (GSA) pumping allocations and surface irrigation district shares",
            "TX" | "KS" | "NE" | "OK" | "CO" | "NM" => "verify Groundwater Conservation District / Ogallala or alluvial well permits, static water depth, and adjudicated surface rights",
            "AZ" | "NV" | "UT" | "ID" | "WA" | "OR" | "MT" | "WY" => "verify prior-appropriation water rights priority date, Bureau of Reclamation / irrigation district delivery shares, and well logs",
            _ => "verify agricultural well capacity (GPM), tile-drainage outlets, and state riparian/consumptive-use water permits",
        };
        let flood_drought = if in_sfha {
            format!("FEMA Flood Zone {flood_zone} (SFHA floodplain present — verify crop insurance & levee protection)")
        } else if high_drought {
            format!("Elevated regional drought exposure ({drought_rating}) — supplemental irrigation strongly accretive")
        } else {
            format!("Outside FEMA 100-year SFHA (Zone {flood_zone}); drought exposure: {drought_rating}")
        };
        format!("{irr_str}; {flood_drought}; {state_water_regime}.")
    };

    // Ag Tax Exemption & USDA FSA / Farm Credit System Financing
    let purchase_basis = asking_price.unwrap_or(blended_mid).max(10_000.0);
    let (ag_prog_name, ag_eff_tax_pct, rollback_note, ag_prog_summary) =
        state_ag_tax_program_details(&state);
    let est_ag_tax_usd = round_n(core_ag_val * (ag_eff_tax_pct / 100.0), 0);
    let est_unrestricted_tax_usd = round_n(purchase_basis * 0.0115, 0);
    let est_tax_savings_usd = round_n((est_unrestricted_tax_usd - est_ag_tax_usd).max(0.0), 0);

    // 1) USDA FSA Direct Farm Ownership Loan (up to $600,000 statutory cap, up to 100% financing / 40-yr term)
    let fsa_direct_cap = 600_000.0_f64;
    let fsa_direct_loan = round_n(purchase_basis.min(fsa_direct_cap), 0);
    let fsa_direct_rate = 5.375_f64;
    let fsa_direct_ds = round_n(
        calc_annual_debt_service(fsa_direct_loan, fsa_direct_rate, 40),
        0,
    );

    // 2) USDA FSA Guaranteed Farm Ownership Loan (up to $2,250,000 statutory cap, 85% LTV, up to 40-yr term)
    let fsa_guar_cap = 2_250_000.0_f64;
    let fsa_guar_loan = round_n((purchase_basis * 0.85).min(fsa_guar_cap), 0);
    let fsa_guar_rate = 6.50_f64;
    let fsa_guar_ds = round_n(
        calc_annual_debt_service(fsa_guar_loan, fsa_guar_rate, 35),
        0,
    );

    // 3) Farm Credit System Institutional Ag Mortgage (65% LTV, 25-yr amortization)
    let fcs_ltv_pct = 65.0_f64;
    let fcs_loan = round_n(purchase_basis * (fcs_ltv_pct / 100.0), 0);
    let fcs_down = round_n(purchase_basis - fcs_loan, 0);
    let mort30 = num_at(report, "/modules/market/data/rates/mort30").unwrap_or(6.75);
    let fcs_rate = round_n((mort30 + 0.35).clamp(5.50, 8.75), 2);
    let fcs_ds = round_n(calc_annual_debt_service(fcs_loan, fcs_rate, 25), 0);
    let cash_lease_dscr = if fcs_ds > 0.0 {
        round_n(net_landlord_noi / fcs_ds, 2)
    } else {
        0.0
    };
    let operator_dscr = if fcs_ds > 0.0 {
        round_n(selected_operator_profit / fcs_ds, 2)
    } else {
        0.0
    };

    json!({
        "status": "ok",
        "state": state,
        "lotAcres": round_n(lot_acres, 2),
        "lotSqft": round_n(lot_sqft, 0),
        "acreageBasis": acreage_basis,
        "selectedFarmingMode": selected_mode,
        "soilAndAgronomy": {
            "dataSource": soil.data_source,
            "mukey": soil.mukey,
            "mapUnitName": soil.map_unit_name,
            "primeFarmlandClass": soil.prime_farmland_class,
            "isPrimeFarmland": soil.is_prime_farmland,
            "irrigatedCapabilityClass": soil.irrigated_capability_class,
            "nonIrrigatedCapabilityClass": soil.non_irrigated_capability_class,
            "capabilityClassNum": soil.capability_class_num,
            "capabilityLabel": capability_class_description(soil.capability_class_num),
            "nccpiOverall": soil.nccpi_overall,
            "soilQualityMultiplier": soil_quality_mult,
            "drainageClass": soil.drainage_class,
            "hydricRating": soil.hydric_rating,
            "slopePct": soil.slope_pct,
            "terrainSlopeSuitability": terrain_suitability,
            "waterAndIrrigationNote": water_irrigation_note,
        },
        "valuation": {
            "nassStateBenchmarks": bench,
            "soilAdjustedCompValue": {
                "baseBenchmarkPerAcre": base_bench_per_acre,
                "soilQualityMultiplier": mode_soil_mult,
                "hazardEnvironmentAdjustmentPct": haz_adj_pct,
                "perAcre": soil_adj_per_acre,
                "totalLow": comp_low,
                "totalMid": comp_mid,
                "totalHigh": comp_high,
            },
            "cashRentCapitalizedValue": {
                "annualCashRentPerAcre": adj_cash_rent_ac,
                "grossAnnualCashRent": gross_cash_rent,
                "landlordPropertyTaxAndInsPerAcre": landlord_tax_ins_per_acre,
                "netLandlordNoi": net_landlord_noi,
                "agCapRatePct": ag_cap_rate,
                "capitalizedValue": cash_rent_cap_val,
                "capitalizedPerAcre": cash_rent_cap_per_acre,
            },
            "periUrbanTransitionOption": {
                "isPeriUrbanTransitional": is_peri_urban,
                "transitionPremiumPct": transition_premium_pct,
                "transitionOptionPerAcre": transition_option_per_acre,
                "transitionAdjustedTotalValue": transition_adjusted_total,
                "rationale": transition_rationale,
            },
            "blendedEstimate": {
                "low": blended_low,
                "value": blended_mid,
                "high": blended_high,
                "perAcre": blended_per_acre,
                "perLotSqft": blended_per_sqft,
                "askingPrice": asking_price,
                "priceVsEstimatePct": price_vs_est_pct,
                "confidence": confidence,
                "methodology": "65% USDA NASS state cropland/pasture/orchard benchmark adjusted by NRCS SSURGO NCCPI & Land Capability Class + 35% capitalized NNN cash-rent income + risk-weighted peri-urban transition option",
            },
        },
        "operatingModes": operating_modes,
        "agTaxAndFinancing": {
            "purchaseBasisUsd": round_n(purchase_basis, 0),
            "stateAgTaxProgram": {
                "programName": ag_prog_name,
                "effectiveAgTaxRatePct": ag_eff_tax_pct,
                "estimatedAnnualAgTaxUsd": est_ag_tax_usd,
                "unrestrictedAdValoremTaxUsd": est_unrestricted_tax_usd,
                "estimatedAnnualTaxSavingsUsd": est_tax_savings_usd,
                "rollbackClawbackNote": rollback_note,
                "summary": ag_prog_summary,
            },
            "usdaFsaDirectOwnershipLoan": {
                "programLabel": "USDA FSA Direct Farm Ownership Loan",
                "statutoryMaxLoanUsd": fsa_direct_cap,
                "sizedLoanAmountUsd": fsa_direct_loan,
                "remainingEquityOrBridgeUsd": round_n((purchase_basis - fsa_direct_loan).max(0.0), 0),
                "maxLtvPct": 100.0,
                "illustrativeRatePct": fsa_direct_rate,
                "termYears": 40,
                "annualDebtServiceUsd": fsa_direct_ds,
                "eligibilityNote": "Up to $600k direct federal financing (up to 100% LTV, 40-yr term; or 5% buyer down via FSA Beginning Farmer Down Payment Program) for family-size owner-operators with >=3 yrs farm management experience.",
            },
            "usdaFsaGuaranteedOwnershipLoan": {
                "programLabel": "USDA FSA Guaranteed Farm Ownership Loan",
                "statutoryMaxLoanUsd": fsa_guar_cap,
                "sizedLoanAmountUsd": fsa_guar_loan,
                "maxLtvPct": 85.0,
                "illustrativeRatePct": fsa_guar_rate,
                "termYears": 35,
                "annualDebtServiceUsd": fsa_guar_ds,
                "eligibilityNote": "Up to $2.25M USDA-backed commercial/ag lender guarantee (up to 85%–90% LTV) for acquisition or permanent crop improvement.",
            },
            "farmCreditSystemMortgage": {
                "programLabel": "Farm Credit System (FCS) 65% LTV Institutional Ag Mortgage",
                "maxLtvPct": fcs_ltv_pct,
                "sizedLoanAmountUsd": fcs_loan,
                "requiredDownPaymentUsd": fcs_down,
                "illustrativeRatePct": fcs_rate,
                "amortYears": 25,
                "annualDebtServiceUsd": fcs_ds,
                "cashLeaseDscr": cash_lease_dscr,
                "operatorDscr": operator_dscr,
                "patronageDividendNote": "Cooperative Farm Credit associations typically return 0.50%–1.00% annual cash patronage dividends, lowering effective net borrowing cost.",
            },
        },
    })
}

fn infer_metro_tier_and_cost_factor(report: &Value, state: &str) -> (String, f64) {
    let cbsa = str_at(report, "/geo/cbsaName")
        .or_else(|| str_at(report, "/geo/metro"))
        .unwrap_or("")
        .to_ascii_lowercase();
    let zhvi = num_at(report, "/modules/market/data/zhvi/value")
        .or_else(|| num_at(report, "/modules/market/data/acs/zip/medianHomeValue"))
        .unwrap_or(0.0);
    let mhi = num_at(
        report,
        "/modules/market/data/acs/tract/medianHouseholdIncome",
    )
    .or_else(|| num_at(report, "/modules/market/data/acs/zip/medianHouseholdIncome"))
    .unwrap_or(0.0);

    let is_tier1 = cbsa.contains("new york")
        || cbsa.contains("san francisco")
        || cbsa.contains("san jose")
        || cbsa.contains("los angeles")
        || cbsa.contains("seattle")
        || cbsa.contains("boston")
        || cbsa.contains("washington")
        || cbsa.contains("miami")
        || cbsa.contains("san diego")
        || zhvi >= 800_000.0
        || mhi >= 125_000.0;

    if is_tier1 {
        let factor = if matches!(state, "CA" | "NY" | "MA" | "HI" | "WA") {
            1.34
        } else {
            1.22
        };
        return ("Tier 1 (Gateway / High-Barrier Metro)".to_string(), factor);
    }

    let is_tier2 = cbsa.contains("dallas")
        || cbsa.contains("houston")
        || cbsa.contains("austin")
        || cbsa.contains("atlanta")
        || cbsa.contains("phoenix")
        || cbsa.contains("denver")
        || cbsa.contains("chicago")
        || cbsa.contains("charlotte")
        || cbsa.contains("nashville")
        || cbsa.contains("tampa")
        || cbsa.contains("orlando")
        || cbsa.contains("portland")
        || cbsa.contains("minneapolis")
        || matches!(state, "CA" | "WA" | "MA" | "CO" | "NJ" | "NY" | "FL" | "TX")
        || zhvi >= 450_000.0
        || mhi >= 80_000.0;

    if is_tier2 {
        let factor = if matches!(state, "CA" | "WA" | "CO" | "NJ" | "NY" | "IL") {
            1.14
        } else {
            1.02
        };
        ("Tier 2 (Primary Growth Metro)".to_string(), factor)
    } else {
        ("Tier 3 (Secondary / Yield Metro)".to_string(), 0.90)
    }
}

fn resolve_commercial_lot_sqft(
    report: &Value,
    opts: Option<&LandFarmEvalOptions>,
) -> Option<(f64, &'static str)> {
    if let Some(ac) = opts
        .and_then(|o| o.acreage)
        .filter(|&a| a.is_finite() && a > 0.0)
    {
        return Some((
            (ac * SQFT_PER_ACRE).clamp(1_000.0, 500_000_000.0),
            "user_override_acreage",
        ));
    }
    let facts = report.get("facts").unwrap_or(&Value::Null);
    if let Some(lsf) = num_at(facts, "/lotSqft")
        .or_else(|| num_at(facts, "/landSqft"))
        .filter(|&s| s.is_finite() && s > 0.0)
    {
        return Some((lsf.clamp(500.0, 500_000_000.0), "parcel_lot_sqft"));
    }
    if let Some(ac) = num_at(facts, "/lotAcres")
        .or_else(|| num_at(facts, "/acres"))
        .filter(|&a| a.is_finite() && a > 0.0)
    {
        return Some((
            (ac * SQFT_PER_ACRE).clamp(500.0, 500_000_000.0),
            "parcel_assessor_acres",
        ));
    }
    None
}

#[derive(Clone, Copy)]
struct ZoningEnvelopeSpec {
    category: &'static str,
    default_far: f64,
    max_stories: u32,
    max_lot_coverage_pct: f64,
    default_hbu: &'static str,
}

fn infer_zoning_envelope(
    zoning_raw: &str,
    lot_sqft: f64,
    frontage_aadt: f64,
    corner_lot: bool,
) -> ZoningEnvelopeSpec {
    let z = zoning_raw.trim().to_ascii_uppercase();
    if z.contains("CBD") || z.contains("DMU") || z.contains("C-4") || z.contains("C-5") {
        return ZoningEnvelopeSpec {
            category: "Central Business / High-Density Mixed-Use (CBD/DMU)",
            default_far: 3.20,
            max_stories: 8,
            max_lot_coverage_pct: 80.0,
            default_hbu: "Mixed-Use Mid-Rise",
        };
    }
    if z.starts_with("MU") || z.starts_with("MX") || z.contains("CMU") || z.starts_with("NC") {
        return ZoningEnvelopeSpec {
            category: "Urban / Corridor Mixed-Use (MU/MXD)",
            default_far: 2.00,
            max_stories: 5,
            max_lot_coverage_pct: 72.0,
            default_hbu: "Mixed-Use Mid-Rise",
        };
    }
    if z.starts_with("M-")
        || z.starts_with("M1")
        || z.starts_with("M2")
        || z.starts_with("I-")
        || z.starts_with("IL")
        || z.starts_with("IH")
        || z.starts_with("LI")
        || z.starts_with("GI")
        || z.starts_with("IND")
        || z.starts_with("BP")
    {
        return ZoningEnvelopeSpec {
            category: "Light Industrial / Logistics / Flex (M-1/I-L)",
            default_far: 0.50,
            max_stories: 2,
            max_lot_coverage_pct: 50.0,
            default_hbu: "Light Industrial / Flex",
        };
    }
    if z.starts_with("CO")
        || z.starts_with("C-O")
        || z.starts_with("OP")
        || z.starts_with("PO")
        || z.starts_with("O-")
    {
        return ZoningEnvelopeSpec {
            category: "Commercial / Medical Office District (C-O/OP)",
            default_far: 0.90,
            max_stories: 3,
            max_lot_coverage_pct: 48.0,
            default_hbu: "Medical/Office",
        };
    }
    if z.contains("PAD")
        || ((lot_sqft >= 14_000.0 && lot_sqft <= 65_000.0)
            && (frontage_aadt >= 18_000.0 || (frontage_aadt >= 12_000.0 && corner_lot)))
    {
        return ZoningEnvelopeSpec {
            category: "High-Visibility Arterial Outparcel / Pad (C-2/CG)",
            default_far: 0.30,
            max_stories: 1,
            max_lot_coverage_pct: 32.0,
            default_hbu: "Retail Pad / QSR Drive-Thru",
        };
    }
    if z.starts_with("C-1") || z.starts_with("C1") || z.starts_with("CN") || z.starts_with("B-1") {
        return ZoningEnvelopeSpec {
            category: "Neighborhood Commercial (C-1/CN)",
            default_far: 0.45,
            max_stories: 2,
            max_lot_coverage_pct: 42.0,
            default_hbu: "Neighborhood Strip Center",
        };
    }
    // Default General Commercial (`C-2`, `CG`, `CR`, `B-2`, etc.)
    ZoningEnvelopeSpec {
        category: "General / Highway Commercial (C-2/CG)",
        default_far: 0.70,
        max_stories: 3,
        max_lot_coverage_pct: 50.0,
        default_hbu: "Neighborhood Strip Center",
    }
}

fn normalize_hbu_candidate(raw: Option<&str>, fallback: &'static str) -> &'static str {
    let Some(s) = raw.map(str::trim).filter(|x| !x.is_empty()) else {
        return fallback;
    };
    let low = s.to_ascii_lowercase();
    if low.contains("qsr") || low.contains("pad") || low.contains("drive") {
        "Retail Pad / QSR Drive-Thru"
    } else if low.contains("strip") || low.contains("retail") || low.contains("shopping") {
        "Neighborhood Strip Center"
    } else if low.contains("mixed") || low.contains("mid-rise") || low.contains("multifamily") {
        "Mixed-Use Mid-Rise"
    } else if low.contains("industrial") || low.contains("flex") || low.contains("warehouse") {
        "Light Industrial / Flex"
    } else if low.contains("medical") || low.contains("office") || low.contains("clinic") {
        "Medical/Office"
    } else {
        fallback
    }
}

/// Evaluates Commercial Land & Pad-Site feasibility: buildable envelope (`FAR`, stories, surface vs
/// structured parking, highest-and-best-use), institutional Residual Land Valuation (RLV waterfall),
/// and NNN Ground-Lease / QSR Drive-Thru Pad capitalization.
pub fn evaluate_commercial_land(report: &Value, opts: Option<&LandFarmEvalOptions>) -> Value {
    let facts = report.get("facts").unwrap_or(&Value::Null);
    let state = extract_state_code(report);
    let (metro_tier, regional_cost_factor) = infer_metro_tier_and_cost_factor(report, &state);
    let zoning = str_at(facts, "/zoning")
        .or_else(|| str_at(facts, "/zoningCode"))
        .or_else(|| str_at(report, "/cre/classification/detectedZoning"))
        .unwrap_or("C-2")
        .to_string();

    let Some((lot_sqft, acreage_basis)) = resolve_commercial_lot_sqft(report, opts) else {
        let in_sfha = report
            .pointer("/modules/hazards/data/flood/sfha")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let flood_zone = str_at(report, "/modules/hazards/data/flood/zone").unwrap_or("X");
        return json!({
            "status": "missing_parcel_area",
            "siteSummary": {
                "state": state,
                "metroTier": metro_tier,
                "lotSqft": Value::Null,
                "lotAcres": Value::Null,
                "acreageBasis": "missing_parcel_area",
                "zoning": zoning,
                "inFemaSfha": in_sfha,
                "femaFloodZone": flood_zone,
            },
            "buildableEnvelope": Value::Null,
            "residualLandValuation": Value::Null,
            "groundLeaseValuation": Value::Null,
            "blendedEstimate": Value::Null,
        });
    };
    let lot_acres = lot_sqft / SQFT_PER_ACRE;

    // Frontage AADT & Corner Lot from `cre.tradeArea` or street hierarchy
    let frontage_aadt = num_at(report, "/cre/tradeArea/frontageTraffic/modeledAadtMid")
        .unwrap_or_else(|| {
            let hw = str_at(report, "/modules/fengshui/data/orientation/street/highway")
                .unwrap_or("primary");
            match hw {
                "motorway" | "trunk" => 44_000.0,
                "primary" => 24_500.0,
                "secondary" => 14_500.0,
                "tertiary" => 6_500.0,
                _ => 11_500.0,
            }
        });
    let corner_lot = report
        .pointer("/cre/tradeArea/frontageTraffic/cornerLotLikely")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let asking_price = opts
        .and_then(|o| o.asking_price)
        .filter(|&p| p > 0.0)
        .or_else(|| num_at(facts, "/listPrice").filter(|&p| p > 0.0))
        .or_else(|| num_at(facts, "/price").filter(|&p| p > 0.0));

    // Hazard & Environmental Deductions (FEMA SFHA floodplain & EPA Brownfield / LUST proximity)
    let in_sfha = report
        .pointer("/modules/hazards/data/flood/sfha")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let flood_zone = str_at(report, "/modules/hazards/data/flood/zone").unwrap_or("X");

    let mut env_flag = false;
    let mut env_remediation_reserve_usd = 0.0_f64;
    let mut env_note = "No on-parcel or immediate (<0.15 mi) EPA NPL/Brownfield/LUST remediation sites flagged in public records.".to_string();

    if let Some(env_data) = report.pointer("/modules/environment/data") {
        let inside_npl = env_data
            .pointer("/sites/insideNplBoundary")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut closest_site: Option<(String, String, f64)> = None;
        if let Some(items) = env_data
            .pointer("/sites/items")
            .and_then(|v| v.as_array())
            .or_else(|| {
                env_data
                    .pointer("/sites/nearest")
                    .and_then(|v| v.as_array())
            })
        {
            for it in items {
                let d_mi = it
                    .get("distanceMi")
                    .or_else(|| it.get("distMi"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(9.9);
                if d_mi <= 0.15 {
                    let prog = it
                        .get("programLabel")
                        .or_else(|| it.get("program"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Regulated Site")
                        .to_string();
                    let nm = it
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Nearby Facility")
                        .to_string();
                    if closest_site.as_ref().map(|c| d_mi < c.2).unwrap_or(true) {
                        closest_site = Some((prog, nm, d_mi));
                    }
                }
            }
        }
        if inside_npl {
            env_flag = true;
            env_remediation_reserve_usd =
                (125_000.0 + lot_acres * 65_000.0).clamp(125_000.0, 850_000.0);
            env_note = "Parcel intersects a mapped Superfund NPL boundary — Phase II subsurface investigation & vapor/soil remediation escrow deducted in RLV.".to_string();
        } else if let Some((prog, nm, d_mi)) = closest_site {
            env_flag = true;
            env_remediation_reserve_usd = if d_mi <= 0.05 { 85_000.0 } else { 42_000.0 };
            env_note = format!(
                "Nearby {prog} case ({nm}, {d_mi:.2} mi) — Phase I/II soil-gas screening & environmental contingency reserve (${env_remediation_reserve_usd:.0}) deducted in RLV."
            );
        }
    }

    // 1. Buildable Envelope & Highest-and-Best-Use (HBU)
    let env_spec = infer_zoning_envelope(&zoning, lot_sqft, frontage_aadt, corner_lot);
    let (allowable_far, far_basis) =
        if let Some(ov_far) = opts.and_then(|o| o.allowable_far).filter(|&f| f > 0.0) {
            (ov_far.clamp(0.05, 15.0), "user_override_far")
        } else {
            (env_spec.default_far, "inferred_from_zoning")
        };

    let hbu = normalize_hbu_candidate(
        opts.and_then(|o| o.target_use.as_deref()),
        env_spec.default_hbu,
    );

    let (parking_ratio_per_1000, parking_strategy) =
        if allowable_far >= 1.50 || hbu == "Mixed-Use Mid-Rise" {
            (1.65, "podium_or_structured_parking")
        } else {
            let ratio = match hbu {
                "Retail Pad / QSR Drive-Thru" => 8.5,
                "Neighborhood Strip Center" => 4.5,
                "Medical/Office" => 3.8,
                "Light Industrial / Flex" => 1.4,
                _ => 3.5,
            };
            (ratio, "surface_parking")
        };

    let unconstrained_max_gfa = round_n(lot_sqft * allowable_far, 0);
    // Surface parking geometry constraint: 1 stall needs ~330 sq ft incl. drive aisles, leaving ~78% of lot after landscaping/setbacks
    let parking_constrained_gfa = if parking_strategy == "surface_parking" {
        let net_site_sqft = lot_sqft * 0.78;
        let footprint_per_gfa = 1.0 / (env_spec.max_stories.max(1) as f64);
        let parking_sqft_per_gfa = (parking_ratio_per_1000 / 1_000.0) * 330.0;
        round_n(
            net_site_sqft / (footprint_per_gfa + parking_sqft_per_gfa),
            0,
        )
    } else {
        round_n(
            lot_sqft
                * (env_spec.max_lot_coverage_pct / 100.0)
                * (env_spec.max_stories as f64)
                * 0.85,
            0,
        )
    };
    let max_buildable_gfa = round_n(
        unconstrained_max_gfa
            .min(parking_constrained_gfa)
            .max(1_200.0),
        0,
    );

    let candidate_uses = vec![
        json!({
            "use": "Retail Pad / QSR Drive-Thru",
            "selected": hbu == "Retail Pad / QSR Drive-Thru",
            "fitNote": if frontage_aadt >= 14_000.0 && lot_sqft >= 15_000.0 {
                "Strong fit — arterial AADT & >=0.35 ac stacking geometry support national credit QSR/coffee drive-thru"
            } else {
                "Moderate/limited — QSR drive-thru typically requires >=15k SF lot and >=14k AADT corridor visibility"
            }
        }),
        json!({
            "use": "Neighborhood Strip Center",
            "selected": hbu == "Neighborhood Strip Center",
            "fitNote": "Multi-tenant inline retail/service bays (NNN leases, 4.5/1,000 surface parking)"
        }),
        json!({
            "use": "Mixed-Use Mid-Rise",
            "selected": hbu == "Mixed-Use Mid-Rise",
            "fitNote": "Ground-floor retail + 3–6 stories residential over podium where MU/CBD zoning allows FAR >= 1.5"
        }),
        json!({
            "use": "Light Industrial / Flex",
            "selected": hbu == "Light Industrial / Flex",
            "fitNote": "Shallow-bay flex/showroom/warehouse (18'–24' clear height, low TI, strong NNN industrial cap rates)"
        }),
        json!({
            "use": "Medical/Office",
            "selected": hbu == "Medical/Office",
            "fitNote": "Outpatient medical/dental/urgent-care clinical shell with high-convenience surface parking"
        }),
    ];

    // 2. Residual Land Valuation (RLV) Waterfall
    let tier_idx = if metro_tier.starts_with("Tier 1") {
        0
    } else if metro_tier.starts_with("Tier 2") {
        1
    } else {
        2
    };

    let (base_nnn_rent_psf, exit_cap_pct, base_hard_cost_psf, occ_pct) = match (hbu, tier_idx) {
        ("Retail Pad / QSR Drive-Thru", 0) => (64.0, 5.65, 295.0, 96.0),
        ("Retail Pad / QSR Drive-Thru", 1) => (48.0, 6.00, 265.0, 95.0),
        ("Retail Pad / QSR Drive-Thru", _) => (38.0, 6.45, 240.0, 94.0),

        ("Neighborhood Strip Center", 0) => (46.0, 6.10, 225.0, 94.0),
        ("Neighborhood Strip Center", 1) => (34.0, 6.50, 198.0, 93.0),
        ("Neighborhood Strip Center", _) => (26.5, 7.00, 180.0, 92.0),

        ("Mixed-Use Mid-Rise", 0) => (48.0, 5.45, 285.0, 94.5),
        ("Mixed-Use Mid-Rise", 1) => (35.5, 5.85, 245.0, 93.5),
        ("Mixed-Use Mid-Rise", _) => (27.5, 6.35, 218.0, 93.0),

        ("Light Industrial / Flex", 0) => (23.5, 5.55, 142.0, 95.0),
        ("Light Industrial / Flex", 1) => (16.8, 6.00, 124.0, 94.0),
        ("Light Industrial / Flex", _) => (12.8, 6.55, 110.0, 93.0),

        ("Medical/Office", 0) => (46.0, 6.35, 255.0, 93.0),
        ("Medical/Office", 1) => (34.5, 6.80, 222.0, 92.0),
        (_, _) => (27.0, 7.30, 198.0, 91.0),
    };

    // Traffic & corner visibility rent premium on retail/pad uses
    let visibility_rent_mult = if matches!(
        hbu,
        "Retail Pad / QSR Drive-Thru" | "Neighborhood Strip Center"
    ) {
        let aadt_boost = ((frontage_aadt - 15_000.0) / 100_000.0).clamp(-0.06, 0.14);
        let corner_boost = if corner_lot { 0.06 } else { 0.0 };
        (1.0 + aadt_boost + corner_boost).clamp(0.92, 1.22)
    } else {
        1.0
    };

    let stabilized_rent_psf = round_n(base_nnn_rent_psf * visibility_rent_mult, 2);
    // NNN / Modified-Gross effective NOI retention (~91% on NNN retail/pad/industrial after non-reimbursable structural/mgmt leakage, ~70% on mixed-use)
    let noi_retention = if hbu == "Mixed-Use Mid-Rise" {
        0.72
    } else {
        0.91
    };
    let stabilized_noi = round_n(
        max_buildable_gfa * stabilized_rent_psf * (occ_pct / 100.0) * noi_retention,
        0,
    );
    let finished_value = round_n(stabilized_noi / (exit_cap_pct / 100.0), 0);

    let hard_cost_psf = round_n(base_hard_cost_psf * regional_cost_factor, 2);
    let hard_cost_total = round_n(max_buildable_gfa * hard_cost_psf, 0);

    // Exact 16.0% Soft Costs & Municipal Impact Fees per spec
    let soft_costs_pct = 16.0_f64;
    let soft_costs_total = round_n(hard_cost_total * (soft_costs_pct / 100.0), 0);

    // Site work, grading, wet/dry utilities, detention & SFHA floodplain fill if applicable
    let base_sitework_per_lot_sf = if parking_strategy == "podium_or_structured_parking" {
        14.0
    } else {
        9.5
    };
    let sfha_fill_per_lot_sf = if in_sfha { 4.25 } else { 0.0 };
    let site_work_total = round_n(
        lot_sqft * (base_sitework_per_lot_sf + sfha_fill_per_lot_sf) * regional_cost_factor,
        0,
    );

    // Construction loan interest carry (~65% LTC, ~55% average draw factor over 14-mo build/lease-up @ 7.75%)
    let subtotal_hard_soft_site =
        hard_cost_total + soft_costs_total + site_work_total + env_remediation_reserve_usd;
    let construction_carry_total = round_n(subtotal_hard_soft_site * 0.036, 0);

    // Exact 18.0% Target Developer Profit on development cost / GDV margin
    let dev_profit_pct = 18.0_f64;
    let dev_profit_total = round_n(
        (subtotal_hard_soft_site + construction_carry_total) * (dev_profit_pct / 100.0),
        0,
    );

    let total_dev_cost_ex_land = round_n(
        subtotal_hard_soft_site + construction_carry_total + dev_profit_total,
        0,
    );

    let flood_penalty_pct = if in_sfha { 12.0 } else { 0.0 };
    let raw_unfloored_rlv = round_n(finished_value - total_dev_cost_ex_land, 0);
    let raw_post_flood_rlv = round_n(raw_unfloored_rlv * (1.0 - flood_penalty_pct / 100.0), 0);

    // Minimum unimproved land-bank / surface storage / pad floor ($/lot SF by metro tier) when vertical construction costs compress speculative build-to-suit RLV
    let land_bank_floor_psf = match tier_idx {
        0 => 22.0,
        1 => 12.0,
        _ => 6.5,
    } * (1.0 - flood_penalty_pct / 100.0);
    let rlv_mid = round_n(raw_post_flood_rlv.max(lot_sqft * land_bank_floor_psf), 0);
    let rlv_low = round_n(rlv_mid * 0.84, 0);
    let rlv_high = round_n(rlv_mid * 1.16, 0);
    let rlv_per_lot_sqft = round_n(rlv_mid / lot_sqft.max(1.0), 2);
    let rlv_per_buildable_sqft = round_n(rlv_mid / max_buildable_gfa.max(1.0), 2);
    let rlv_per_acre = round_n(rlv_mid / lot_acres.max(0.01), 0);

    // 3. NNN Ground-Lease & QSR Drive-Thru Pad Valuation
    let is_industrial_zone = env_spec.default_hbu == "Light Industrial / Flex";
    let qsr_drive_thru_eligible = !is_industrial_zone
        && lot_sqft >= 15_000.0
        && lot_sqft <= 110_000.0
        && (frontage_aadt >= 12_000.0 || hbu == "Retail Pad / QSR Drive-Thru");

    let (pad_tier_label, base_ground_rent_lot_psf_yr, ground_cap_rate_pct) =
        if qsr_drive_thru_eligible {
            let base_gr = match tier_idx {
                0 => 4.85,
                1 => 3.35,
                _ => 2.35,
            };
            let cap = match tier_idx {
                0 => 5.25,
                1 => 5.65,
                _ => 6.10,
            };
            (
                "National Credit QSR / Coffee / Bank Drive-Thru Outparcel Pad",
                base_gr,
                cap,
            )
        } else if !is_industrial_zone && frontage_aadt >= 10_000.0 {
            let base_gr = match tier_idx {
                0 => 3.20,
                1 => 2.15,
                _ => 1.45,
            };
            let cap = match tier_idx {
                0 => 5.75,
                1 => 6.15,
                _ => 6.60,
            };
            ("Arterial Commercial NNN Ground Lease", base_gr, cap)
        } else {
            let base_gr = match tier_idx {
                0 => 2.10,
                1 => 1.35,
                _ => 0.85,
            };
            let cap = match tier_idx {
                0 => 6.00,
                1 => 6.40,
                _ => 6.85,
            };
            (
                "Standard Commercial / Industrial Yard NNN Ground Lease",
                base_gr,
                cap,
            )
        };

    // Scale ground rent per SF gently for large multi-acre tracts (outparcel pads command higher $/SF than 10-acre raw tracts)
    let size_curve = if lot_acres <= 1.25 {
        1.0
    } else {
        (1.25 / lot_acres).powf(0.28).clamp(0.42, 1.0)
    };
    let corner_ground_mult = if corner_lot { 1.10 } else { 1.0 };
    let flood_ground_mult = if in_sfha { 0.88 } else { 1.0 };

    let ground_rent_psf_yr = round_n(
        base_ground_rent_lot_psf_yr * size_curve * corner_ground_mult * flood_ground_mult,
        2,
    )
    .max(0.35);
    let annual_ground_rent = round_n(lot_sqft * ground_rent_psf_yr, 0);
    let capitalized_pad_val = round_n(
        (annual_ground_rent / (ground_cap_rate_pct / 100.0) - env_remediation_reserve_usd)
            .max(lot_sqft * 4.0),
        0,
    );
    let cap_pad_per_lot_sqft = round_n(capitalized_pad_val / lot_sqft.max(1.0), 2);
    let cap_pad_per_acre = round_n(capitalized_pad_val / lot_acres.max(0.01), 0);

    // 4. Blended Commercial Land Estimate
    let (rlv_weight, gl_weight) = if qsr_drive_thru_eligible {
        (0.45, 0.55)
    } else {
        (0.65, 0.35)
    };
    let blended_val = round_n(rlv_weight * rlv_mid + gl_weight * capitalized_pad_val, 0);
    let blended_low = round_n(blended_val * 0.85, 0);
    let blended_high = round_n(blended_val * 1.16, 0);
    let blended_per_lot_sqft = round_n(blended_val / lot_sqft.max(1.0), 2);
    let blended_per_acre = round_n(blended_val / lot_acres.max(0.01), 0);

    let confidence = if far_basis == "user_override_far" || str_at(facts, "/zoning").is_some() {
        "high"
    } else {
        "medium"
    };

    let price_vs_est_pct =
        asking_price.map(|ask| round_n(((ask - blended_val) / blended_val.max(1.0)) * 100.0, 1));

    json!({
        "status": "ok",
        "siteSummary": {
            "state": state,
            "metroTier": metro_tier,
            "lotSqft": round_n(lot_sqft, 0),
            "lotAcres": round_n(lot_acres, 3),
            "acreageBasis": acreage_basis,
            "zoning": zoning,
            "frontageAadt": round_n(frontage_aadt, 0),
            "cornerLot": corner_lot,
            "inFemaSfha": in_sfha,
            "femaFloodZone": flood_zone,
            "environmentalRemediationFlag": env_flag,
            "environmentalRemediationNote": env_note,
        },
        "buildableEnvelope": {
            "zoningCode": zoning,
            "zoningCategory": env_spec.category,
            "allowableFar": allowable_far,
            "farBasis": far_basis,
            "maxStories": env_spec.max_stories,
            "maxLotCoveragePct": env_spec.max_lot_coverage_pct,
            "unconstrainedMaxGfaSqft": unconstrained_max_gfa,
            "parkingStrategy": parking_strategy,
            "parkingRatioPer1000Sqft": parking_ratio_per_1000,
            "parkingConstrainedGfaSqft": parking_constrained_gfa,
            "maxBuildableGfaSqft": max_buildable_gfa,
            "highestAndBestUse": hbu,
            "candidateUses": candidate_uses,
        },
        "residualLandValuation": {
            "prototypeUse": hbu,
            "buildableGfaSqft": max_buildable_gfa,
            "stabilizedRentPerSqftYr": stabilized_rent_psf,
            "stabilizedOccupancyPct": occ_pct,
            "stabilizedAnnualNoi": stabilized_noi,
            "exitCapRatePct": exit_cap_pct,
            "stabilizedFinishedValue": finished_value,
            "hardConstructionCostPerSqft": hard_cost_psf,
            "regionalCostFactor": regional_cost_factor,
            "hardConstructionCostTotal": hard_cost_total,
            "softCostsAndImpactFeesPct": soft_costs_pct,
            "softCostsAndImpactFeesTotal": soft_costs_total,
            "siteWorkAndUtilitiesTotal": site_work_total,
            "environmentalRemediationReserveUsd": round_n(env_remediation_reserve_usd, 0),
            "constructionCarryInterestTotal": construction_carry_total,
            "developerProfitTargetPct": dev_profit_pct,
            "developerProfitTotal": dev_profit_total,
            "totalDevelopmentCostExLand": total_dev_cost_ex_land,
            "floodSfhaPenaltyPct": flood_penalty_pct,
            "rawUnflooredResidualUsd": raw_post_flood_rlv,
            "low": rlv_low,
            "value": rlv_mid,
            "high": rlv_high,
            "perLotSqft": rlv_per_lot_sqft,
            "perBuildableSqft": rlv_per_buildable_sqft,
            "perAcre": rlv_per_acre,
        },
        "groundLeaseValuation": {
            "padTierLabel": pad_tier_label,
            "qsrDriveThruEligible": qsr_drive_thru_eligible,
            "perLotSqftYr": ground_rent_psf_yr,
            "annualGroundRent": annual_ground_rent,
            "groundCapRatePct": ground_cap_rate_pct,
            "capitalizedPadValue": capitalized_pad_val,
            "capitalizedPerLotSqft": cap_pad_per_lot_sqft,
            "capitalizedPerAcre": cap_pad_per_acre,
        },
        "blendedEstimate": {
            "low": blended_low,
            "value": blended_val,
            "high": blended_high,
            "perLotSqft": blended_per_lot_sqft,
            "perAcre": blended_per_acre,
            "askingPrice": asking_price,
            "priceVsEstimatePct": price_vs_est_pct,
            "confidence": confidence,
            "methodology": format!(
                "{:.0}% Development Residual Land Value ({hbu}) + {:.0}% Capitalized NNN Ground-Lease Pad Value",
                rlv_weight * 100.0,
                gl_weight * 100.0
            ),
        },
    })
}

/// Synchronous / offline entrypoint for Commercial Land & Agricultural/Farmland valuation.
/// Makes zero network calls; uses `soil` when provided or `infer_baseline_soil_profile` otherwise.
pub fn evaluate_land_and_farm_offline(
    report: &Value,
    is_farmland: bool,
    soil: Option<UsdaSoilProfile>,
    opts: Option<&LandFarmEvalOptions>,
) -> ModuleOutput {
    let state = extract_state_code(report);
    let soil_profile = soil.unwrap_or_else(|| infer_baseline_soil_profile(report, &state));
    let soil_is_live = soil_profile.data_source == "usda_nrcs_ssurgo_live";

    let mut warnings = Vec::new();
    if is_farmland {
        if resolve_farmland_acreage(report, opts).is_none() {
            warnings.push(
                "Parcel area (lotSqft / lotAcres) is missing from public property records; farmland dollar valuation and operating pro-forma are omitted until parcel acreage is supplied."
                    .to_string(),
            );
        }
        if !soil_is_live {
            warnings.push(
                "USDA NRCS Soil Data Access (SSURGO) live query unavailable or offline; using regional state/terrain soil productivity baseline."
                    .to_string(),
            );
        }
    } else if resolve_commercial_lot_sqft(report, opts).is_none() {
        warnings.push(
            "Parcel area (lotSqft / lotAcres) is missing from public property records; commercial land Residual Land Value and ground-lease estimates are omitted until parcel acreage is supplied."
                .to_string(),
        );
    }

    let comm_val = evaluate_commercial_land(report, opts);
    let farm_val = evaluate_farmland(report, soil_profile, opts);

    let (mode_code, active_status, active_lot_sqft, active_lot_acres, active_basis, active_blended) =
        if is_farmland {
            (
                "agricultural_farm",
                farm_val
                    .get("status")
                    .cloned()
                    .unwrap_or_else(|| json!("ok")),
                farm_val.get("lotSqft").cloned().unwrap_or(Value::Null),
                farm_val.get("lotAcres").cloned().unwrap_or(Value::Null),
                farm_val
                    .get("acreageBasis")
                    .cloned()
                    .unwrap_or_else(|| json!("missing_parcel_area")),
                farm_val
                    .pointer("/valuation/blendedEstimate")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
        } else {
            (
                "commercial_land",
                comm_val
                    .get("status")
                    .cloned()
                    .unwrap_or_else(|| json!("ok")),
                comm_val
                    .pointer("/siteSummary/lotSqft")
                    .cloned()
                    .unwrap_or(Value::Null),
                comm_val
                    .pointer("/siteSummary/lotAcres")
                    .cloned()
                    .unwrap_or(Value::Null),
                comm_val
                    .pointer("/siteSummary/acreageBasis")
                    .cloned()
                    .unwrap_or_else(|| json!("missing_parcel_area")),
                comm_val
                    .get("blendedEstimate")
                    .cloned()
                    .unwrap_or(Value::Null),
            )
        };

    let mut sources = vec![
        SourceItem::new(
            "USDA National Agricultural Statistics Service (NASS) — Land Values & Cash Rents Survey (Public Domain, 17 U.S.C. § 105)",
            "https://www.nass.usda.gov/Surveys/Guide_to_NASS_Surveys/June_Area/",
        ),
        SourceItem::new(
            "USDA Farm Service Agency (FSA) — Direct & Guaranteed Farm Ownership Loans",
            "https://www.fsa.usda.gov/resources/loans/farm-ownership-loans",
        ),
    ];
    if soil_is_live || is_farmland {
        sources.insert(
            0,
            SourceItem::new(
                "USDA Natural Resources Conservation Service (NRCS) — Soil Data Access (SSURGO)",
                "https://sdmdataaccess.sc.egov.usda.gov/",
            ),
        );
    }

    let data = json!({
        "status": active_status,
        "mode": mode_code,
        "isFarmland": is_farmland,
        "state": state,
        "lotSqft": active_lot_sqft,
        "lotAcres": active_lot_acres,
        "acreageBasis": active_basis,
        "blendedEstimate": active_blended,
        // Direct top-level accessors for both modes + nested `commercialLand` and `farmland` objects
        "buildableEnvelope": comm_val.get("buildableEnvelope").cloned().unwrap_or(Value::Null),
        "residualLandValuation": comm_val.get("residualLandValuation").cloned().unwrap_or(Value::Null),
        "groundLeaseValuation": comm_val.get("groundLeaseValuation").cloned().unwrap_or(Value::Null),
        "soilAndAgronomy": farm_val.get("soilAndAgronomy").cloned().unwrap_or(Value::Null),
        "valuation": farm_val.get("valuation").cloned().unwrap_or(Value::Null),
        "operatingModes": farm_val.get("operatingModes").cloned().unwrap_or(Value::Null),
        "agTaxAndFinancing": farm_val.get("agTaxAndFinancing").cloned().unwrap_or(Value::Null),
        "commercialLand": comm_val,
        "farmland": farm_val,
        "disclaimer": LAND_FARM_DISCLAIMER,
    });

    ModuleOutput {
        data,
        sources,
        warnings,
    }
}

/// Unified async entrypoint for Commercial Land & Agricultural/Farmland valuation.
/// When `http` is provided and `is_farmland` is `true` (or soil cache is warm), queries USDA NRCS
/// Soil Data Access (`5s` timeout) and falls back cleanly to `infer_baseline_soil_profile` if offline.
pub async fn evaluate_land_and_farm(
    http: Option<&HttpClient>,
    report: &Value,
    is_farmland: bool,
    opts: Option<&LandFarmEvalOptions>,
) -> ModuleOutput {
    let live_soil = if is_farmland {
        if let (Some(client), Some((lat, lon))) = (http, extract_coords(report)) {
            fetch_usda_soil_profile(client, lat, lon).await
        } else {
            None
        }
    } else {
        None
    };
    evaluate_land_and_farm_offline(report, is_farmland, live_soil, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nass_50_state_registry_completeness_and_cap_rate_bounds() {
        let all_50 = [
            "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN",
            "IA", "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV",
            "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN",
            "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
        ];
        assert_eq!(all_50.len(), 50);
        for st in all_50 {
            let b = lookup_nass_state_benchmark(st);
            assert_eq!(b.state_code, st, "state {st} must match exact entry");
            assert!(b.irrigated_cropland_per_acre >= b.non_irrigated_cropland_per_acre);
            assert!(b.non_irrigated_cropland_per_acre >= b.pasture_rangeland_per_acre * 0.9);
            assert!(
                (3.2..=5.2).contains(&b.ag_cap_rate_pct),
                "state {st} ag cap rate {} out of 3.2-5.2% range",
                b.ag_cap_rate_pct
            );
            assert!(b.non_irrigated_cash_rent_per_acre_yr > 0.0);
        }
        let fb = lookup_nass_state_benchmark("ZZ");
        assert_eq!(fb.state_code, "US");
    }

    #[test]
    fn test_real_parcel_commercial_land_farmland_and_missing_area_honesty() {
        // Synthetic Santa Clara County parcel (9,200 lotSqft)
        let campbell = crate::test_synth::synth_report_secondary();
        let comm_out = evaluate_land_and_farm_offline(
            &campbell,
            false,
            None,
            Some(&LandFarmEvalOptions {
                target_use: Some("Neighborhood Strip Center".to_string()),
                ..Default::default()
            }),
        );
        assert_eq!(comm_out.data["status"], json!("ok"));
        assert_eq!(comm_out.data["mode"], json!("commercial_land"));
        assert_eq!(comm_out.data["acreageBasis"], json!("parcel_lot_sqft"));
        assert_eq!(comm_out.data["lotSqft"], json!(9_200.0));
        assert_eq!(
            comm_out.data["residualLandValuation"]["softCostsAndImpactFeesPct"],
            json!(16.0)
        );
        assert_eq!(
            comm_out.data["residualLandValuation"]["developerProfitTargetPct"],
            json!(18.0)
        );
        assert!(comm_out.data["blendedEstimate"]["value"].as_f64().unwrap() > 100_000.0);

        // Synthetic Miami-Dade County parcel (`synth-fl-33134-older-sfr`) & farmland pro-forma math
        let fl_parcel = crate::test_synth::synth_report_from_holdout("synth-fl-33134-older-sfr");
        let farm_out = evaluate_land_and_farm_offline(
            &fl_parcel,
            true,
            None,
            Some(&LandFarmEvalOptions {
                farming_mode: Some("orchard_vineyard".to_string()),
                ..Default::default()
            }),
        );
        assert_eq!(farm_out.data["status"], json!("ok"));
        assert_eq!(farm_out.data["mode"], json!("agricultural_farm"));
        assert_eq!(farm_out.data["state"], json!("FL"));
        let modes = farm_out.data["operatingModes"]
            .as_array()
            .expect("4 operating modes");
        assert_eq!(modes.len(), 4);
        for m in modes {
            let gross = m["grossRevenuePerAcre"].as_f64().unwrap();
            let opex = m["operatingCostPerAcre"].as_f64().unwrap();
            let net = m["operatorNetProfitPerAcre"].as_f64().unwrap();
            assert!((gross - opex - net).abs() <= 1.0);
            assert!(m["landlordAnnualNnnCashIncome"].as_f64().unwrap() > 0.0);
        }
        assert_eq!(
            farm_out.data["agTaxAndFinancing"]["usdaFsaDirectOwnershipLoan"]["statutoryMaxLoanUsd"],
            json!(600_000.0)
        );
        assert_eq!(
            farm_out.data["agTaxAndFinancing"]["usdaFsaGuaranteedOwnershipLoan"]
                ["statutoryMaxLoanUsd"],
            json!(2_250_000.0)
        );
        assert_eq!(
            farm_out.data["agTaxAndFinancing"]["farmCreditSystemMortgage"]["maxLtvPct"],
            json!(65.0)
        );

        // Missing parcel area (`lotSqft` / `lotAcres` absent and no user `acreage` override):
        // must NEVER invent 40.0 acres or 21,780 sq ft
        let mut missing_area = campbell.clone();
        if let Some(f) = missing_area
            .get_mut("facts")
            .and_then(|v| v.as_object_mut())
        {
            f.remove("lotSqft");
            f.remove("landSqft");
            f.remove("lotAcres");
            f.remove("acres");
        }
        let comm_missing = evaluate_land_and_farm_offline(&missing_area, false, None, None);
        assert_eq!(comm_missing.data["status"], json!("missing_parcel_area"));
        assert!(comm_missing.data["lotSqft"].is_null());
        assert!(comm_missing.data["lotAcres"].is_null());
        assert!(comm_missing.data["blendedEstimate"].is_null());
        assert!(comm_missing.data["residualLandValuation"].is_null());

        let farm_missing = evaluate_land_and_farm_offline(&missing_area, true, None, None);
        assert_eq!(farm_missing.data["status"], json!("missing_parcel_area"));
        assert!(farm_missing.data["lotSqft"].is_null());
        assert!(farm_missing.data["lotAcres"].is_null());
        assert!(farm_missing.data["blendedEstimate"].is_null());
        assert!(farm_missing.data["valuation"].is_null());
        assert_eq!(
            farm_missing.data["operatingModes"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn test_offline_guard_and_live_usda_nrcs_ssurgo_query() {
        let off_http = HttpClient::offline_loopback_only(None);
        assert!(
            fetch_usda_soil_profile(&off_http, 38.7185, -121.7732)
                .await
                .is_none(),
            "offline loopback HttpClient must make zero external calls"
        );

        // Live keyless public-domain USDA NRCS Soil Data Access check over Yolo County CA agricultural plain
        let live_http = HttpClient::new(None);
        if let Some(live_soil) = fetch_usda_soil_profile(&live_http, 38.7185, -121.7732).await {
            assert_eq!(live_soil.data_source, "usda_nrcs_ssurgo_live");
            assert!(!live_soil.map_unit_name.is_empty());
            assert!((1..=8).contains(&live_soil.capability_class_num));
            assert!((0.05..=1.0).contains(&live_soil.nccpi_overall));
        }
    }
}
