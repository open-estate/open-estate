// Natural-language-to-SQL property search, `sea_query` parameterized query compiler,
// Fair Housing & prompt-injection guardrails, deterministic ranker, and OpenAI Responses API
// (`LLM_MATCH_MODEL` -> `LLM_FALLBACK_MODEL`) grounded candidate ranker.

pub use crate::db::{
    format_listing_row, school_grade_to_min_score, score_to_school_grade, ListingRow,
};

use crate::ai::llm::{
    parse_model_spec, responses_json_with_cache, DbLlmCache, LlmCallOptions,
    DEFAULT_FALLBACK_MODEL, DEFAULT_MATCH_MODEL,
};
use crate::db::{
    proto, sql_bit_is_set_expr, sql_bitmask_all_set_expr, sql_bitmask_none_set_expr, DbStore,
    FeatureTag, ListingFullView, PropertyCategory, ReportHeaderProto, SqlParam,
};
use crate::scoring::catalog::{Dimension, Lang};
use crate::scoring::engine::{rank_with, CommuteMatrix, RankOptions, RankResult};
use crate::scoring::factors::HomeFactors;
use crate::scoring::presets::PRESETS;
use crate::scoring::profile::{Place, Profile, TravelMode, MAX_WEIGHT};
use crate::search::sync::{str_from_val, sync_areas_on_demand};
use crate::shared::{now_unix_ms, round_n, EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use sea_query::{
    Alias, Asterisk, Condition, Expr, ExprTrait, Func, MysqlQueryBuilder, Order, Query,
    SqliteQueryBuilder,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use thiserror::Error;

pub const CURRENT_FILTER_SCHEMA_VERSION: i64 = 2;

pub const FAIR_HOUSING_NOTICE_TEXT: &str =
    "Under the US Fair Housing Act, Open Estate ignores race, religion, national origin, and demographic criteria and matches strictly on objective property, pricing, and public school data.";

const PARSE_INSTRUCTIONS: &str = "You are the Open Estate Database Query Planner. Translate a buyer's natural-language US home requirements (inside <untrusted_buyer_requirements>) into a structured filter plan over the `listings` SQL table.

RULES:
1. LOCATION EXTRACTION:
   - Put explicit 5-digit US ZIP codes in `zips` (e.g., [\"95118\"]). If the buyer names a well-known US neighborhood without a ZIP (e.g., \"Cambrian San Jose\" -> [\"95118\", \"95124\"], \"Willow Glen\" -> [\"95125\"]), include those canonical ZIPs.
   - Only populate `cities` for cities NOT already narrowed to a specific ZIP in `zips` (e.g., for \"San Jose 95118\", set zips:[\"95118\"], cities:[]; for \"95118 or Campbell\", set zips:[\"95118\"], cities:[\"Campbell\"]; for \"San Jose\", set zips:[], cities:[\"San Jose\"]).
2. PRICE & UNITS:
   - Convert shorthand accurately: \"$2.2M\" -> 2200000, \"$950k\" -> 950000, \"220万\" -> 2200000.
   - Lot acres: \"quarter acre\" -> minLotSqft: 10890.
3. SCHOOL GRADE / RATING:
   - Map school expectations to BOTH `minSchoolGrade` and `minSchoolScore` (0-10 scale):
     \"A+\" -> \"A+\" / 9.0; \"A\" or \"top schools\" -> \"A\" / 8.0; \"A-\" -> \"A-\" / 7.5; \"B+\" or \"good schools\" -> \"B+\" / 7.0; \"B\" -> \"B\" / 6.0.
4. LISTING STATUS:
   - Default `statuses` to [\"for_sale\", \"pending\"] unless the user explicitly asks for sold homes (\"sold\" -> [\"sold\"]) or both active and sold ([\"for_sale\", \"pending\", \"sold\"]).
5. HOA & PROPERTY TYPE:
   - \"No HOA\" -> maxHoaMonthly: 0.
   - \"Single family\" / \"house\" / \"detached\" / \"独栋\" -> [\"Single-Family Detached\"].
   - \"Townhouse\" / \"联排\" -> [\"Townhome / Attached\"]. \"Condo\" / \"公寓\" -> [\"Condo / Co-op\"].
6. KEYWORDS vs HARD FILTERS:
   - Map explicit lifestyle/condition/hazard criteria to structured fields when requested: `minSafetyScore` (0–100), `hasHardwoodFloors`, `hasUpdatedKitchen`, `remodeledOnly`, `quietStreetOnly`, `maxNoiseDb`, `maxSeismicPga`, and canonical snake_case `requiredTags` (e.g. `hardwood_floors`, `updated_kitchen`, `remodeled`, `quiet_street`, `solar`, `adu`, `pool`, `fiber`).
   - Put soft/text features (e.g. \"sunroom\", \"south-facing\") in `niceToHaveKeywords` unless strictly required.
7. FAIR HOUSING COMPLIANCE:
   - NEVER filter on race, religion, national origin, familial status, sex, disability, or demographic stereotypes. If mentioned, ignore that demographic phrase and set a brief neutral note in `fairHousingNotice`.";

const RANK_INSTRUCTIONS: &str = "You are the Open Estate Property Match Analyst. Given a buyer's requirements (<untrusted_buyer_requirements>) and CANDIDATE_LISTINGS_JSON retrieved from our SQL database, explain the already-ranked candidate homes cleanly.

RULES:
1. Ground EVERY number (price, beds, baths, sqft, lot sqft, year built, HOA, school grade/score/names, status, sold date, and any `reportHighlights` such as valuation estimate, monthly cost, rent estimate, flood/seismic/wildfire, utility, fiber, and report headline) strictly in CANDIDATE_LISTINGS_JSON. Never invent attributes, numeric scores, or rankings.
2. Keep `whyItMatches` to 1-2 crisp sentences citing concrete property numbers, and `tradeoffs` to 1 short phrase (or null if none). Plain text only — never output code fences or SQL.
3. Enforce Fair Housing Act strictly: discuss property, pricing, physical hazards, and public school rating numbers only.
4. Never mention Zillow, Zestimate, ZHVI, ZORI, Redfin, Walk Score, Realtor.com, Homes.com, Trulia, Movoto, Compass, or Opendoor; say \"Walkability score\", \"Third-party estimate\", or \"ZIP home-value index\" instead.";

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[error("[{code}] ({status}): {message}")]
pub struct SearchValidationError {
    pub code: String,
    pub message: String,
    pub status: u16,
}

impl SearchValidationError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, status: u16) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
        }
    }
}

pub fn format_int_commas(n: i64) -> String {
    crate::shared::helpers::fmt_commas(n)
}

pub fn fmt_clean_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        format!("{}", round_n(v, 1))
    }
}

pub fn clean_untrusted_text(raw: &str, max_len: usize) -> String {
    crate::guardrails::clean_untrusted_text(raw, max_len)
}

pub fn wrap_untrusted_block(tag_name: &str, raw_text: &str) -> String {
    crate::guardrails::wrap_untrusted_block(tag_name, raw_text)
}

pub static FAIR_HOUSING_DEMOGRAPHIC_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:in\s+a\s+)?(?:mostly\s+|predominantly\s+)?(?:white|black|african[\s-]*american|asian|chinese|indian|hispanic|latino|jewish|christian|muslim|catholic|hindu|ethnic|racial)(?:\s+(?:white|black|african[\s-]*american|asian|chinese|indian|hispanic|latino|jewish|christian|muslim|catholic|hindu|ethnic|racial))*\s+(?:neighborhood|area|community|residents|families|buyers|neighbors)\b|\b(?:mostly|predominantly)\s+(?:white|black|african[\s-]*american|asian|chinese|indian|hispanic|latino|christian|jewish|muslim|catholic|hindu)\b").unwrap()
});

pub fn inspect_output_guardrail(text: &str) -> bool {
    crate::guardrails::inspect_output_guardrail(text).is_some()
}

/// Validates natural-language home search criteria against prompt injection, coding/scraper hijacks, and SQLi.
/// Returns `Err(SearchValidationError { code: "INVALID_SEARCH", status: 400, .. })` on violation.
pub fn validate_search_requirements(
    raw_text: &str,
) -> std::result::Result<String, SearchValidationError> {
    crate::guardrails::validate_search_requirements(raw_text)
        .map_err(|ge| SearchValidationError::new("INVALID_SEARCH", ge.message, ge.status))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilters {
    pub on_topic: bool,
    pub intent_summary: String,
    pub zips: Vec<String>,
    pub cities: Vec<String>,
    pub states: Vec<String>,
    pub statuses: Vec<String>,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    pub min_beds: Option<f64>,
    pub max_beds: Option<f64>,
    pub min_baths: Option<f64>,
    pub min_sqft: Option<i64>,
    pub max_sqft: Option<i64>,
    pub min_lot_sqft: Option<i64>,
    pub min_year_built: Option<i64>,
    pub max_year_built: Option<i64>,
    pub stories: Option<i64>,
    pub max_hoa_monthly: Option<i64>,
    pub property_types: Vec<String>,
    pub min_school_score: Option<f64>,
    pub min_school_grade: Option<String>,
    pub exclude_high_flood_risk: bool,
    pub exclude_high_wildfire_risk: bool,
    pub must_have_keywords: Vec<String>,
    pub nice_to_have_keywords: Vec<String>,
    pub deal_breakers: Vec<String>,
    pub sort_by: String,
    pub fair_housing_notice: Option<String>,
    // Amazon-style structured utility, internet/fiber, monthly-carry & hardware shopping facets:
    #[serde(default)]
    pub fiber_only: bool,
    #[serde(default)]
    pub high_speed_internet_only: bool,
    #[serde(default)]
    pub internet_carriers: Vec<String>,
    #[serde(default)]
    pub min_download_mbps: Option<i64>,
    #[serde(default)]
    pub municipal_electric_only: bool,
    #[serde(default)]
    pub max_electric_rate_kwh: Option<f64>,
    #[serde(default)]
    pub electric_carriers: Vec<String>,
    #[serde(default)]
    pub municipal_water_only: bool,
    #[serde(default)]
    pub municipal_sewer_only: bool,
    #[serde(default)]
    pub max_monthly_utilities: Option<i64>,
    #[serde(default)]
    pub max_all_in_monthly: Option<i64>,
    #[serde(default)]
    pub exclude_special_tax: bool,
    #[serde(default)]
    pub has_solar: bool,
    #[serde(default)]
    pub has_ev_charger: bool,
    #[serde(default)]
    pub has_heat_pump: bool,
    #[serde(default)]
    pub has_adu: bool,
    #[serde(default)]
    pub adu_feasible: bool,
    #[serde(default)]
    pub has_pool: bool,
    #[serde(default)]
    pub min_garage_spaces: Option<f64>,
    #[serde(default)]
    pub architectural_styles: Vec<String>,
    // Extended structured condition, safety, noise, seismic, and canonical tag filters (schema v2):
    #[serde(default)]
    pub min_safety_score: Option<f64>,
    #[serde(default)]
    pub has_hardwood_floors: bool,
    #[serde(default)]
    pub has_updated_kitchen: bool,
    #[serde(default)]
    pub remodeled_only: bool,
    #[serde(default)]
    pub quiet_street_only: bool,
    #[serde(default)]
    pub max_noise_db: Option<f64>,
    #[serde(default)]
    pub max_seismic_pga: Option<f64>,
    #[serde(default)]
    pub required_tags: Vec<String>,
    #[serde(default)]
    pub schema_version: Option<i64>,
}

impl Default for SearchFilters {
    fn default() -> Self {
        Self {
            on_topic: true,
            intent_summary: "Parsed criteria: any indexed area".to_string(),
            zips: Vec::new(),
            cities: Vec::new(),
            states: Vec::new(),
            statuses: vec!["for_sale".to_string(), "pending".to_string()],
            min_price: None,
            max_price: None,
            min_beds: None,
            max_beds: None,
            min_baths: None,
            min_sqft: None,
            max_sqft: None,
            min_lot_sqft: None,
            min_year_built: None,
            max_year_built: None,
            stories: None,
            max_hoa_monthly: None,
            property_types: Vec::new(),
            min_school_score: None,
            min_school_grade: None,
            exclude_high_flood_risk: false,
            exclude_high_wildfire_risk: false,
            must_have_keywords: Vec::new(),
            nice_to_have_keywords: Vec::new(),
            deal_breakers: Vec::new(),
            sort_by: "best_match".to_string(),
            fair_housing_notice: None,
            fiber_only: false,
            high_speed_internet_only: false,
            internet_carriers: Vec::new(),
            min_download_mbps: None,
            municipal_electric_only: false,
            max_electric_rate_kwh: None,
            electric_carriers: Vec::new(),
            municipal_water_only: false,
            municipal_sewer_only: false,
            max_monthly_utilities: None,
            max_all_in_monthly: None,
            exclude_special_tax: false,
            has_solar: false,
            has_ev_charger: false,
            has_heat_pump: false,
            has_adu: false,
            adu_feasible: false,
            has_pool: false,
            min_garage_spaces: None,
            architectural_styles: Vec::new(),
            min_safety_score: None,
            has_hardwood_floors: false,
            has_updated_kitchen: false,
            remodeled_only: false,
            quiet_street_only: false,
            max_noise_db: None,
            max_seismic_pga: None,
            required_tags: Vec::new(),
            schema_version: None,
        }
    }
}

fn parse_dollar(num_str: &str, unit: Option<&str>) -> Option<i64> {
    let cleaned = num_str.replace(',', "");
    let v = cleaned.parse::<f64>().ok().filter(|n| n.is_finite())?;
    let u = unit.unwrap_or("").trim().to_ascii_lowercase();
    match u.as_str() {
        "m" | "mil" | "million" => Some((v * 1_000_000.0).round() as i64),
        "k" => Some((v * 1_000.0).round() as i64),
        "万" => Some((v * 10_000.0).round() as i64),
        _ => {
            if v >= 50_000.0 {
                Some(v.round() as i64)
            } else {
                None
            }
        }
    }
}

static ZIP_TOKEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{5})\b").unwrap());
static RANGE_PRICE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\$?\s*(\d+(?:\.\d+)?)\s*(m|mil|million|k|万)?\s*(?:-|–|to|~)\s*\$?\s*(\d+(?:\.\d+)?)\s*(m|mil|million|k|万)").unwrap()
});
static UNDER_PRICE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:under|below|max|up\s+to|budget|less\s+than|以内|不超过|低于|<|<=)\s*\$?\s*(\d[\d,]*(?:\.\d+)?)\s*(m|mil|million|k|万)?").unwrap()
});
static BARE_DOLLAR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\$\s*(\d[\d,]*(?:\.\d+)?)\s*(m|mil|million|k)\b").unwrap());
static OVER_PRICE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:over|above|min|at\s+least|>|>=)\s*\$?\s*(\d[\d,]*(?:\.\d+)?)\s*(m|mil|million|k|万)?").unwrap()
});
static BED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\d+)\s*\+?\s*(?:bed|beds|bd|br|bedroom|bedrooms|房|卧)").unwrap()
});
static BATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*\+?\s*(?:bath|baths|ba|bathroom|bathrooms|卫)").unwrap()
});
static SQFT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\d{3,5})\s*\+?\s*(?:sq\s*ft|sqft|square\s*feet)").unwrap());
static LOT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\d{4,6})\s*\+?\s*(?:sq\s*ft|sqft)?\s*lot").unwrap());
static YR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:built\s+(?:after|since)|after)\s*(19\d{2}|20\d{2})").unwrap());
static SCH_GRADE_1_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b([AB][+-]?)\s*(?:-|\s)?(?:rated\s+|grade\s+)?schools?\b").unwrap()
});
static SCH_GRADE_2_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)schools?\s*(?:grade|rated|at\s+least)?\s*([AB][+-]?)\b").unwrap()
});
static SCH_GRADE_ZH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)学区\s*([AB][+-]?)").unwrap());
static SCH_NUM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b([6-9]|10)\s*\+?\s*(?:/\s*10\s*)?(?:rated\s+|greatschools\s+)?schools?\b")
        .unwrap()
});
static TOP_SCH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:top|excellent|great)\s+schools?\b|好学区|顶级学区").unwrap()
});
static SFR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)single[-\s]?family|detached|\bsfr\b|独栋").unwrap());
static TH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)townhome|townhouse|联排").unwrap());
static CONDO_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bcondo\b|公寓").unwrap());
static NO_HOA_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bno\s+hoa\b|zero\s+hoa|不要\s*hoa|无\s*hoa").unwrap());
static ONE_STORY_Q_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:1[-\s]?story|single[-\s]?story|one[-\s]?story|一层|平层)\b").unwrap()
});
static SOLD_ONLY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bsold\s+only\b|\brecently\s+sold\b|\bsold\s+homes?\b|已成交").unwrap()
});
static FOR_SALE_Q_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)for\s+sale").unwrap());
static INC_SOLD_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bincluding\s+sold\b|\ball\s+homes\b").unwrap());
static NO_FLOOD_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bno\s+flood\b|\blow\s+flood\b").unwrap());
static NO_FIRE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bno\s+(?:wild)?fire\b|\blow\s+(?:wild)?fire\b").unwrap());
static NO_FIXER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bno\s+fixer\b").unwrap());
static MONTHLY_BUDGET_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:under|below|max|up\s+to|<=|<)?\s*\$\s*(\d[\d,]*(?:\.\d+)?)\s*(k)?\s*(?:/\s*mo(?:nth)?|\bper\s+month\b|\bmonthly\b)").unwrap()
});
static KWH_RATE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:under|below|max|up\s+to|<=|<)?\s*\$?\s*(0?\.\d{1,3})\s*(?:/\s*kwh|\bper\s*kwh\b)",
    )
    .unwrap()
});
static GBPS_SPEED_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b([1-9]|10)\s*\+?\s*gbps\b").unwrap());
static GARAGE_Q_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b([1-4])\s*\+?\s*[- ]?car\s+garage\b").unwrap());
static SAFETY_SCORE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)safety(?:\s+score)?\s*(?:>=|>|at\s+least|min|over|above)?\s*(\d{2,3})\b").unwrap()
});
static NOISE_DB_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:under|below|max|<=|<|quiet(?:er)?\s+than)?\s*(\d{2})\s*dba?\b").unwrap()
});
static SEISMIC_PGA_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:seismic|pga)\s*(?:<=|<|under|below|max)?\s*(0\.\d{1,2})\s*g?\b").unwrap()
});

/// Extracts structured utility, fiber/broadband, monthly-budget, special-tax, and energy/hardware
/// shopping facets from natural-language buyer text and merges them into `filters`.
pub fn apply_shopping_heuristics(raw_text: &str, filters: &mut SearchFilters) {
    let q = FAIR_HOUSING_DEMOGRAPHIC_RE
        .replace_all(raw_text, " ")
        .into_owned();
    let lower = q.to_ascii_lowercase();
    let mut added_notes: Vec<String> = Vec::new();

    if let Some(m) = MONTHLY_BUDGET_RE.captures(&q) {
        if let Ok(base) = m[1].replace(',', "").parse::<f64>() {
            let mult = if m.get(2).is_some() { 1_000.0 } else { 1.0 };
            let mo = (base * mult).round() as i64;
            if (500..=60_000).contains(&mo) {
                filters.max_all_in_monthly = Some(mo);
                added_notes.push(format!("≤ ${}/mo all-in", format_int_commas(mo)));
                // Guard against `$7k/mo` having been mistaken for a $7,000 purchase price
                if filters.max_price == Some(mo) {
                    filters.max_price = None;
                }
            }
        }
    }

    if let Some(m) = KWH_RATE_RE.captures(&q) {
        if let Ok(rate) = m[1].parse::<f64>() {
            if (0.05..=0.75).contains(&rate) {
                filters.max_electric_rate_kwh = Some(rate);
                added_notes.push(format!("≤ ${rate:.2}/kWh"));
            }
        }
    } else if lower.contains("cheap electric")
        || lower.contains("low electric rate")
        || lower.contains("cheap power")
    {
        filters.max_electric_rate_kwh.get_or_insert(0.25);
        added_notes.push("≤ $0.25/kWh".to_string());
    }

    if lower.contains("municipal electric")
        || lower.contains("municipal power")
        || lower.contains("public power")
        || lower.contains("city electric")
    {
        filters.municipal_electric_only = true;
        added_notes.push("Municipal Electric".to_string());
    }

    for (needle, canon) in [
        ("silicon valley power", "Silicon Valley Power"),
        ("palo alto utilities", "Palo Alto Utilities"),
        ("alameda municipal", "Alameda Municipal Power"),
        ("roseville electric", "Roseville Electric"),
        ("smud", "SMUD"),
        ("ladwp", "LADWP"),
        ("austin energy", "Austin Energy"),
        ("cps energy", "CPS Energy"),
        ("seattle city light", "Seattle City Light"),
        ("pg&e", "PG&E"),
        ("coned", "Con Edison"),
        ("con edison", "Con Edison"),
    ] {
        if lower.contains(needle)
            && !filters
                .electric_carriers
                .iter()
                .any(|c| c.eq_ignore_ascii_case(canon))
        {
            filters.electric_carriers.push(canon.to_string());
            added_notes.push(canon.to_string());
        }
    }

    if lower.contains("fiber")
        || lower.contains("ftth")
        || lower.contains("symmetrical internet")
        || q.contains("光纤")
    {
        filters.fiber_only = true;
        added_notes.push("Fiber (FTTH)".to_string());
    }
    if lower.contains("high speed internet")
        || lower.contains("high-speed internet")
        || lower.contains("gigabit")
    {
        filters.high_speed_internet_only = true;
        if lower.contains("gigabit") {
            filters.min_download_mbps.get_or_insert(1000);
        }
    }
    if let Some(m) = GBPS_SPEED_RE.captures(&q) {
        if let Ok(g) = m[1].parse::<i64>() {
            filters.min_download_mbps = Some(g * 1000);
            filters.high_speed_internet_only = true;
            added_notes.push(format!("{g}+ Gbps"));
        }
    }

    for (needle, canon, is_fiber) in [
        ("at&t fiber", "AT&T Fiber", true),
        ("att fiber", "AT&T Fiber", true),
        ("sonic", "Sonic", true),
        ("google fiber", "Google Fiber", true),
        ("verizon fios", "Verizon Fios", true),
        ("fios", "Verizon Fios", true),
        ("frontier fiber", "Frontier Fiber", true),
        ("quantum fiber", "Quantum Fiber", true),
        ("ziply", "Ziply Fiber", true),
        ("xfinity", "Xfinity", false),
        ("spectrum", "Spectrum", false),
    ] {
        if lower.contains(needle)
            && !filters
                .internet_carriers
                .iter()
                .any(|c| c.eq_ignore_ascii_case(canon))
        {
            filters.internet_carriers.push(canon.to_string());
            if is_fiber {
                filters.fiber_only = true;
            }
            added_notes.push(canon.to_string());
        }
    }

    if lower.contains("no septic")
        || lower.contains("public sewer")
        || lower.contains("municipal sewer")
        || lower.contains("city sewer")
    {
        filters.municipal_sewer_only = true;
        added_notes.push("Municipal Sewer".to_string());
    }
    if lower.contains("no well")
        || lower.contains("public water")
        || lower.contains("municipal water")
        || lower.contains("city water")
    {
        filters.municipal_water_only = true;
        added_notes.push("Municipal Water".to_string());
    }
    if lower.contains("no mello-roos")
        || lower.contains("no mello roos")
        || lower.contains("no mud")
        || lower.contains("no cdd")
        || lower.contains("no special tax")
    {
        filters.exclude_special_tax = true;
        added_notes.push("No Special Tax District".to_string());
    }

    if lower.contains("solar") || lower.contains("powerwall") {
        filters.has_solar = true;
        added_notes.push("Solar".to_string());
    }
    if lower.contains("ev charger")
        || lower.contains("tesla charger")
        || lower.contains("level 2 charger")
        || lower.contains("nema 14-50")
    {
        filters.has_ev_charger = true;
        added_notes.push("EV Charger".to_string());
    }
    if lower.contains("heat pump") || lower.contains("mini-split") {
        filters.has_heat_pump = true;
        added_notes.push("Heat Pump".to_string());
    }
    if lower.contains("adu potential")
        || lower.contains("adu eligible")
        || lower.contains("adu-eligible")
        || lower.contains("room for adu")
    {
        filters.adu_feasible = true;
        added_notes.push("ADU-Eligible".to_string());
    } else if lower.contains("existing adu")
        || lower.contains("has adu")
        || lower.contains("with adu")
        || lower.contains("in-law unit")
        || lower.contains("guest house")
    {
        filters.has_adu = true;
        added_notes.push("Has ADU".to_string());
    }
    if lower.contains("must have pool")
        || lower.contains("with a pool")
        || lower.contains("private pool")
    {
        filters.has_pool = true;
    }
    if let Some(m) = GARAGE_Q_RE.captures(&q) {
        if let Ok(g) = m[1].parse::<f64>() {
            filters.min_garage_spaces = Some(g);
            added_notes.push(format!("{}+ car garage", g as i64));
        }
    }

    if let Some(m) = SAFETY_SCORE_RE.captures(&q) {
        if let Ok(sc) = m[1].parse::<f64>() {
            if (20.0..=100.0).contains(&sc) {
                filters.min_safety_score = Some(sc);
                added_notes.push(format!("Safety ≥ {}", sc as i64));
            }
        }
    } else if lower.contains("safe neighborhood")
        || lower.contains("safe area")
        || lower.contains("low crime")
        || lower.contains("high safety")
    {
        filters.min_safety_score.get_or_insert(70.0);
        added_notes.push("Low Crime / Safe Area".to_string());
    }

    if lower.contains("hardwood") || lower.contains("wood floors") {
        filters.has_hardwood_floors = true;
        added_notes.push("Hardwood Floors".to_string());
    }
    if lower.contains("updated kitchen")
        || lower.contains("remodeled kitchen")
        || lower.contains("renovated kitchen")
        || lower.contains("modern kitchen")
        || lower.contains("chef's kitchen")
        || lower.contains("quartz kitchen")
    {
        filters.has_updated_kitchen = true;
        added_notes.push("Updated Kitchen".to_string());
    }
    if lower.contains("remodeled")
        || lower.contains("renovated")
        || lower.contains("turnkey")
        || lower.contains("fully updated")
    {
        filters.remodeled_only = true;
        added_notes.push("Remodeled".to_string());
    }
    if lower.contains("quiet street")
        || lower.contains("quiet neighborhood")
        || lower.contains("cul-de-sac")
        || lower.contains("cul de sac")
        || lower.contains("low noise")
    {
        filters.quiet_street_only = true;
        added_notes.push("Quiet Street".to_string());
    }
    if let Some(m) = NOISE_DB_RE.captures(&q) {
        if let Ok(db) = m[1].parse::<f64>() {
            if (30.0..=85.0).contains(&db) {
                filters.max_noise_db = Some(db);
                added_notes.push(format!("Noise ≤ {} dB", db as i64));
            }
        }
    }
    if let Some(m) = SEISMIC_PGA_RE.captures(&q) {
        if let Ok(pga) = m[1].parse::<f64>() {
            if (0.05..=2.5).contains(&pga) {
                filters.max_seismic_pga = Some(pga);
                added_notes.push(format!("Seismic ≤ {pga:.2}g"));
            }
        }
    } else if lower.contains("low seismic") || lower.contains("low earthquake") {
        filters.max_seismic_pga.get_or_insert(0.45);
        added_notes.push("Low Seismic".to_string());
    }

    filters.schema_version = Some(CURRENT_FILTER_SCHEMA_VERSION);

    if !added_notes.is_empty() {
        for note in added_notes {
            if !filters.intent_summary.contains(&note) {
                filters.intent_summary.push_str(" · ");
                filters.intent_summary.push_str(&note);
            }
        }
    }
}

/// Deterministic 0ms heuristic parser with Fair Housing protected-class stripping and `fair_housing_notice`.
pub fn parse_requirements_heuristic(raw_text: &str) -> SearchFilters {
    let has_fha = FAIR_HOUSING_DEMOGRAPHIC_RE.is_match(raw_text);
    // Strip protected-class demographic phrases before extracting any criteria/keywords
    let q = FAIR_HOUSING_DEMOGRAPHIC_RE
        .replace_all(raw_text, " ")
        .into_owned();
    let lower = q.to_ascii_lowercase();
    let q_for_price = MONTHLY_BUDGET_RE.replace_all(&q, " ");
    let q_for_price = KWH_RATE_RE.replace_all(&q_for_price, " ");

    let mut zips: Vec<String> = Vec::new();
    for cap in ZIP_TOKEN_RE.captures_iter(&q) {
        if let Some(m) = cap.get(1) {
            let z = m.as_str().to_string();
            if !zips.contains(&z) && zips.len() < 10 {
                zips.push(z);
            }
        }
    }

    let mut min_price = None;
    let mut max_price = None;
    if let Some(rm) = RANGE_PRICE_RE.captures(&q_for_price) {
        let u1 = rm.get(2).map(|m| m.as_str());
        let u2 = rm.get(4).map(|m| m.as_str());
        min_price = parse_dollar(&rm[1], u1.or(u2)).filter(|&p| p >= 35_000);
        max_price = parse_dollar(&rm[3], u2).filter(|&p| p >= 35_000);
    } else {
        if let Some(um) = UNDER_PRICE_RE
            .captures(&q_for_price)
            .or_else(|| BARE_DOLLAR_RE.captures(&q_for_price))
        {
            max_price =
                parse_dollar(&um[1], um.get(2).map(|m| m.as_str())).filter(|&p| p >= 35_000);
        }
        if let Some(om) = OVER_PRICE_RE.captures(&q_for_price) {
            min_price =
                parse_dollar(&om[1], om.get(2).map(|m| m.as_str())).filter(|&p| p >= 35_000);
        }
    }

    let min_beds = BED_RE.captures(&q).and_then(|c| c[1].parse::<f64>().ok());
    let min_baths = BATH_RE.captures(&q).and_then(|c| c[1].parse::<f64>().ok());
    let min_sqft = SQFT_RE.captures(&q).and_then(|c| c[1].parse::<i64>().ok());
    let min_lot_sqft = LOT_RE
        .captures(&q)
        .and_then(|c| c[1].parse::<i64>().ok())
        .or_else(|| {
            if lower.contains("quarter acre") {
                Some(10890)
            } else {
                None
            }
        });
    let min_year_built = YR_RE.captures(&q).and_then(|c| c[1].parse::<i64>().ok());

    let mut min_school_grade = None;
    let mut min_school_score = None;
    if let Some(sg) = SCH_GRADE_1_RE
        .captures(&q)
        .or_else(|| SCH_GRADE_2_RE.captures(&q))
        .or_else(|| SCH_GRADE_ZH_RE.captures(&q))
    {
        let g = sg[1].to_ascii_uppercase();
        min_school_score = school_grade_to_min_score(Some(&g));
        min_school_grade = Some(g);
    } else if let Some(sn) = SCH_NUM_RE.captures(&q) {
        if let Ok(num) = sn[1].parse::<f64>() {
            min_school_score = Some(num);
            min_school_grade = Some(
                if num >= 9.0 {
                    "A+"
                } else if num >= 8.0 {
                    "A"
                } else if num >= 7.0 {
                    "B+"
                } else {
                    "B"
                }
                .to_string(),
            );
        }
    } else if TOP_SCH_RE.is_match(&q) {
        min_school_grade = Some("A".to_string());
        min_school_score = Some(8.0);
    }

    let mut property_types = Vec::new();
    if SFR_RE.is_match(&q) {
        property_types.push("Single-Family Detached".to_string());
    }
    if TH_RE.is_match(&q) {
        property_types.push("Townhome / Attached".to_string());
    }
    if CONDO_RE.is_match(&q) {
        property_types.push("Condo / Co-op".to_string());
    }

    let max_hoa_monthly = if NO_HOA_RE.is_match(&q) {
        Some(0)
    } else {
        None
    };
    let stories = if ONE_STORY_Q_RE.is_match(&q) {
        Some(1)
    } else {
        None
    };

    let statuses = if SOLD_ONLY_RE.is_match(&lower) && !FOR_SALE_Q_RE.is_match(&lower) {
        vec!["sold".to_string()]
    } else if INC_SOLD_RE.is_match(&lower) {
        vec![
            "for_sale".to_string(),
            "pending".to_string(),
            "sold".to_string(),
        ]
    } else {
        vec!["for_sale".to_string(), "pending".to_string()]
    };

    let mut cities = Vec::new();
    if zips.is_empty() {
        for c in [
            "San Jose",
            "Campbell",
            "Sunnyvale",
            "Cupertino",
            "Santa Clara",
            "Los Gatos",
            "Saratoga",
            "Palo Alto",
            "Mountain View",
            "San Francisco",
            "Sacramento",
            "Roseville",
            "Alameda",
            "Los Angeles",
            "San Diego",
            "Irvine",
            "Austin",
            "Houston",
            "Dallas",
            "San Antonio",
            "Seattle",
            "Bellevue",
            "Miami",
            "Orlando",
        ] {
            if lower.contains(&c.to_ascii_lowercase()) {
                cities.push(c.to_string());
            }
        }
    }

    let mut summary_parts = vec![if !zips.is_empty() {
        format!("ZIP {}", zips.join(", "))
    } else if !cities.is_empty() {
        cities.join(", ")
    } else {
        "any indexed area".to_string()
    }];
    if let Some(mp) = max_price {
        summary_parts.push(format!("up to ${}", format_int_commas(mp)));
    }
    if let Some(b) = min_beds {
        summary_parts.push(format!("{}+ beds", fmt_clean_num(b)));
    }
    if let Some(ba) = min_baths {
        summary_parts.push(format!("{}+ baths", fmt_clean_num(ba)));
    }
    if let Some(ref g) = min_school_grade {
        summary_parts.push(format!("School {g}+"));
    }
    if !property_types.is_empty() {
        summary_parts.push(property_types.join("/"));
    }

    let nice_to_have_keywords = ["pool", "adu", "sunroom", "remodeled", "cul-de-sac"]
        .into_iter()
        .filter(|k| lower.contains(k))
        .map(ToOwned::to_owned)
        .collect();

    let mut out = SearchFilters {
        on_topic: true,
        intent_summary: format!("Parsed criteria: {}", summary_parts.join(" · ")),
        zips,
        cities,
        states: Vec::new(),
        statuses,
        min_price,
        max_price,
        min_beds,
        max_beds: None,
        min_baths,
        min_sqft,
        max_sqft: None,
        min_lot_sqft,
        min_year_built,
        max_year_built: None,
        stories,
        max_hoa_monthly,
        property_types,
        min_school_score,
        min_school_grade,
        exclude_high_flood_risk: NO_FLOOD_RE.is_match(&q),
        exclude_high_wildfire_risk: NO_FIRE_RE.is_match(&q),
        must_have_keywords: Vec::new(),
        nice_to_have_keywords,
        deal_breakers: if NO_FIXER_RE.is_match(&q) {
            vec!["fixer".to_string()]
        } else {
            Vec::new()
        },
        sort_by: "best_match".to_string(),
        fair_housing_notice: if has_fha {
            Some(FAIR_HOUSING_NOTICE_TEXT.to_string())
        } else {
            None
        },
        ..Default::default()
    };
    apply_shopping_heuristics(raw_text, &mut out);
    out
}

fn scrub_demographic_keywords(filters: &mut SearchFilters) {
    static PROTECTED_WORD_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\b(?:white|black|african[\s-]*american|asian|chinese|indian|hispanic|latino|jewish|christian|muslim|catholic|hindu|ethnic|racial)\b").unwrap()
    });
    filters
        .must_have_keywords
        .retain(|k| !PROTECTED_WORD_RE.is_match(k));
    filters
        .nice_to_have_keywords
        .retain(|k| !PROTECTED_WORD_RE.is_match(k));
    filters
        .deal_breakers
        .retain(|k| !PROTECTED_WORD_RE.is_match(k));
}

// ---------------------------------------------------------------- Parameterized `sea_query` SQL Compiler
#[derive(Debug, Clone)]
pub struct CompiledSqlQuery {
    pub sql: String,
    pub params: Vec<SqlParam>,
    pub sql_preview: String,
}

fn format_preview_param(p: &SqlParam) -> String {
    match p {
        SqlParam::Null => "NULL".to_string(),
        SqlParam::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        SqlParam::I64(n) => n.to_string(),
        SqlParam::F64(f) => fmt_clean_num(*f),
        SqlParam::Text(s) => format!("'{}'", s.replace('\'', "''")),
        SqlParam::Bytes(b) => format!("X'{}'", hex::encode(b)),
    }
}

pub fn compile_filters_where_condition(
    filters: &SearchFilters,
    relaxed: bool,
    override_statuses: Option<&[String]>,
) -> Condition {
    let mut cond = Condition::all();

    let statuses: Vec<String> = override_statuses.map(|s| s.to_vec()).unwrap_or_else(|| {
        if filters.statuses.is_empty() {
            vec!["for_sale".to_string(), "pending".to_string()]
        } else {
            filters.statuses.clone()
        }
    });
    if !statuses.is_empty() {
        cond = cond.add(Expr::col(Alias::new("status")).is_in(statuses));
    }

    if !filters.zips.is_empty() || !filters.cities.is_empty() {
        let mut loc = Condition::any();
        if !filters.zips.is_empty() {
            loc = loc.add(Expr::col(Alias::new("zip")).is_in(filters.zips.clone()));
        }
        if !filters.cities.is_empty() {
            let mut city_any = Condition::any();
            for c in &filters.cities {
                city_any = city_any.add(
                    Func::lower(Expr::col(Alias::new("city")))
                        .eq(Func::lower(Expr::val(c.clone()))),
                );
            }
            loc = loc.add(city_any);
        }
        cond = cond.add(loc);
    }

    if !filters.states.is_empty() && filters.zips.is_empty() {
        let mut st_any = Condition::any();
        for st in &filters.states {
            st_any = st_any.add(
                Func::upper(Expr::col(Alias::new("state"))).eq(Func::upper(Expr::val(st.clone()))),
            );
        }
        cond = cond.add(st_any);
    }

    if let Some(min_p) = filters.min_price {
        let eff = ((min_p as f64) * if relaxed { 0.88 } else { 1.0 }).round() as i64;
        cond = cond.add(Expr::col(Alias::new("price")).gte(eff));
    }
    if let Some(max_p) = filters.max_price {
        let eff = ((max_p as f64) * if relaxed { 1.15 } else { 1.0 }).round() as i64;
        cond = cond.add(Expr::col(Alias::new("price")).lte(eff));
    }

    if let Some(min_b) = filters.min_beds {
        let eff = (min_b - if relaxed { 1.0 } else { 0.0 }).max(1.0);
        cond = cond.add(
            Condition::any()
                .add(Expr::col(Alias::new("beds")).is_null())
                .add(Expr::col(Alias::new("beds")).gte(eff)),
        );
    }
    if let (Some(max_b), false) = (filters.max_beds, relaxed) {
        cond = cond.add(Expr::col(Alias::new("beds")).lte(max_b));
    }

    if let Some(min_ba) = filters.min_baths {
        let eff = (min_ba - if relaxed { 0.5 } else { 0.0 }).max(1.0);
        cond = cond.add(
            Condition::any()
                .add(Expr::col(Alias::new("baths")).is_null())
                .add(Expr::col(Alias::new("baths")).gte(eff)),
        );
    }

    if let Some(min_sq) = filters.min_sqft {
        let eff = ((min_sq as f64) * if relaxed { 0.85 } else { 1.0 }).round() as i64;
        cond = cond.add(
            Condition::any()
                .add(Expr::col(Alias::new("sqft")).is_null())
                .add(Expr::col(Alias::new("sqft")).gte(eff)),
        );
    }
    if let (Some(max_sq), false) = (filters.max_sqft, relaxed) {
        cond = cond.add(Expr::col(Alias::new("sqft")).lte(max_sq));
    }

    if let Some(min_lot) = filters.min_lot_sqft {
        let eff = ((min_lot as f64) * if relaxed { 0.8 } else { 1.0 }).round() as i64;
        cond = cond.add(
            Condition::any()
                .add(Expr::col(Alias::new("lot_sqft")).is_null())
                .add(Expr::col(Alias::new("lot_sqft")).gte(eff)),
        );
    }

    if !relaxed {
        if let Some(min_yr) = filters.min_year_built {
            cond = cond.add(
                Condition::any()
                    .add(Expr::col(Alias::new("year_built")).is_null())
                    .add(Expr::col(Alias::new("year_built")).gte(min_yr)),
            );
        }
        if let Some(max_yr) = filters.max_year_built {
            cond = cond.add(
                Condition::any()
                    .add(Expr::col(Alias::new("year_built")).is_null())
                    .add(Expr::col(Alias::new("year_built")).lte(max_yr)),
            );
        }
        if let Some(st) = filters.stories {
            cond = cond.add(
                Condition::any()
                    .add(Expr::col(Alias::new("stories")).is_null())
                    .add(Expr::col(Alias::new("stories")).eq(st as f64)),
            );
        }
        if let Some(max_hoa) = filters.max_hoa_monthly {
            cond = cond.add(
                Condition::any()
                    .add(Expr::col(Alias::new("hoa_monthly")).is_null())
                    .add(Expr::col(Alias::new("hoa_monthly")).lte(max_hoa)),
            );
        }
        if !filters.property_types.is_empty() {
            cond = cond
                .add(Expr::col(Alias::new("property_type")).is_in(filters.property_types.clone()));
        }
    }

    // A null `school_score` means "no official school data" (unknown), never "bad schools": such listings
    // stay in the candidate set, are ordered after known matches, and the deterministic explanation says
    // "school data unavailable".
    let eff_school = filters
        .min_school_score
        .or_else(|| school_grade_to_min_score(filters.min_school_grade.as_deref()));
    if let Some(sc) = eff_school {
        let threshold = round_n(sc - if relaxed { 1.0 } else { 0.2 }, 1);
        cond = cond.add(
            Condition::any()
                .add(Expr::col(Alias::new("school_score")).is_null())
                .add(Expr::col(Alias::new("school_score")).gte(threshold)),
        );
    }

    if filters.exclude_high_flood_risk && !relaxed {
        cond = cond.add(
            Condition::any()
                .add(Expr::col(Alias::new("flood_zone")).is_null())
                .add(
                    Expr::col(Alias::new("flood_zone"))
                        .is_not_in(["AE", "A", "AO", "AH", "VE", "V"]),
                ),
        );
    }

    if filters.exclude_high_wildfire_risk && !relaxed {
        // `wildfire_zone` holds short labels such as "CA FHSZ Very High", "CA FHSZ none" or
        // "WHP 4 High" (see `extract_listings_from_report`); older rows may hold a bare "High" or
        // "Very High". Every High and Very High class contains "high"; no lower class does.
        cond = cond.add(
            Condition::any()
                .add(Expr::col(Alias::new("wildfire_zone")).is_null())
                .add(Func::lower(Expr::col(Alias::new("wildfire_zone"))).not_like("%high%")),
        );
    }

    // Structured Utility, Fiber/Internet, Monthly Carry & Energy/Hardware Shopping Facets
    if !relaxed {
        if filters.fiber_only {
            cond = cond.add(Expr::col(Alias::new("fiber_available")).eq(1_i64));
        }
        if filters.high_speed_internet_only {
            cond = cond.add(Expr::col(Alias::new("high_speed_internet")).eq(1_i64));
        }
        if !filters.internet_carriers.is_empty() {
            let mut isp_any = Condition::any();
            for c in filters.internet_carriers.iter().take(6) {
                let needle = c.trim().to_ascii_lowercase();
                if !needle.is_empty() {
                    let pat = format!("%{needle}%");
                    isp_any = isp_any
                        .add(
                            Func::lower(Expr::col(Alias::new("internet_carriers")))
                                .like(pat.clone()),
                        )
                        .add(Func::lower(Expr::col(Alias::new("fiber_carriers"))).like(pat));
                }
            }
            cond = cond.add(isp_any);
        }
        if let Some(min_dl) = filters.min_download_mbps {
            cond = cond.add(Expr::col(Alias::new("max_download_mbps")).gte(min_dl));
        }
        if filters.municipal_electric_only {
            cond = cond.add(Expr::col(Alias::new("electric_utility_type")).eq("municipal"));
        }
        if !filters.electric_carriers.is_empty() {
            let mut elec_any = Condition::any();
            for c in filters.electric_carriers.iter().take(6) {
                let needle = c.trim().to_ascii_lowercase();
                if !needle.is_empty() {
                    elec_any = elec_any.add(
                        Func::lower(Expr::col(Alias::new("electric_carrier")))
                            .like(format!("%{needle}%")),
                    );
                }
            }
            cond = cond.add(elec_any);
        }
        if filters.municipal_water_only {
            cond = cond.add(Expr::col(Alias::new("water_source_type")).eq("municipal"));
        }
        if filters.municipal_sewer_only {
            cond = cond.add(Expr::col(Alias::new("sewer_type")).eq("municipal"));
        }
        if filters.exclude_special_tax {
            cond = cond.add(
                Condition::any()
                    .add(Expr::col(Alias::new("has_special_tax")).is_null())
                    .add(Expr::col(Alias::new("has_special_tax")).eq(0_i64)),
            );
        }
        if filters.has_solar {
            cond = cond.add(Expr::col(Alias::new("has_solar")).eq(1_i64));
        }
        if filters.has_ev_charger {
            cond = cond.add(Expr::col(Alias::new("has_ev_charger")).eq(1_i64));
        }
        if filters.has_heat_pump {
            cond = cond.add(Expr::col(Alias::new("has_heat_pump")).eq(1_i64));
        }
        if filters.has_adu {
            cond = cond.add(Expr::col(Alias::new("has_adu")).eq(1_i64));
        }
        if filters.adu_feasible {
            cond = cond.add(Expr::col(Alias::new("adu_feasible")).eq(1_i64));
        }
        if filters.has_pool {
            cond = cond.add(Expr::col(Alias::new("has_pool")).eq(1_i64));
        }
        if let Some(min_g) = filters.min_garage_spaces {
            cond = cond.add(Expr::col(Alias::new("garage_spaces")).gte(min_g));
        }
        if !filters.architectural_styles.is_empty() {
            let mut st_any = Condition::any();
            for s in filters.architectural_styles.iter().take(6) {
                let needle = s.trim().to_ascii_lowercase();
                if !needle.is_empty() {
                    st_any = st_any.add(
                        Func::lower(Expr::col(Alias::new("architectural_style")))
                            .like(format!("%{needle}%")),
                    );
                }
            }
            cond = cond.add(st_any);
        }
        if let Some(min_safety) = filters.min_safety_score {
            cond = cond.add(Expr::col(Alias::new("safety_score")).gte(min_safety));
        }
        if filters.has_hardwood_floors {
            cond = cond.add(Expr::col(Alias::new("has_hardwood_floors")).eq(1_i64));
        }
        if filters.has_updated_kitchen {
            cond = cond.add(Expr::col(Alias::new("has_updated_kitchen")).eq(1_i64));
        }
        if filters.remodeled_only {
            cond = cond.add(Expr::col(Alias::new("has_remodeled")).eq(1_i64));
        }
        if filters.quiet_street_only {
            cond = cond.add(Expr::col(Alias::new("quiet_street")).eq(1_i64));
        }
        if let Some(max_db) = filters.max_noise_db {
            cond = cond.add(Expr::col(Alias::new("noise_max_db")).lte(max_db));
        }
        if let Some(max_pga) = filters.max_seismic_pga {
            cond = cond.add(Expr::col(Alias::new("seismic_pga")).lte(max_pga));
        }
        for tag in filters.required_tags.iter().take(6) {
            let clean_tag = tag.trim().to_ascii_lowercase().replace(' ', "_");
            if !clean_tag.is_empty() {
                let pat_words = format!("%{}%", clean_tag.replace('_', " "));
                cond = cond.add(
                    Condition::any()
                        .add(Expr::cust_with_values(
                            "EXISTS (SELECT 1 FROM listing_tags lt WHERE lt.listing_sk = listings.listing_sk AND lt.tag_code = ?)",
                            [clean_tag],
                        ))
                        .add(Func::lower(Expr::col(Alias::new("features_text"))).like(pat_words)),
                );
            }
        }
    }

    if let Some(max_r) = filters.max_electric_rate_kwh {
        let eff = round_n(max_r * if relaxed { 1.15 } else { 1.0 }, 4);
        cond = cond.add(Expr::col(Alias::new("electric_rate_kwh")).lte(eff));
    }
    if let Some(max_u) = filters.max_monthly_utilities {
        let eff = ((max_u as f64) * if relaxed { 1.15 } else { 1.0 }).round() as i64;
        cond = cond.add(Expr::col(Alias::new("est_monthly_utilities")).lte(eff));
    }
    if let Some(max_m) = filters.max_all_in_monthly {
        let eff = ((max_m as f64) * if relaxed { 1.15 } else { 1.0 }).round() as i64;
        cond = cond.add(Expr::col(Alias::new("all_in_monthly")).lte(eff));
    }

    if !relaxed {
        for kw in filters.must_have_keywords.iter().take(3) {
            let clean_kw = kw.trim();
            if !clean_kw.is_empty() {
                cond = cond.add(
                    Func::lower(Expr::col(Alias::new("features_text")))
                        .like(format!("%{}%", clean_kw.to_ascii_lowercase())),
                );
            }
        }
    }

    cond
}

pub fn compile_filters_to_sql(
    filters: &SearchFilters,
    dialect: &str,
    relaxed: bool,
    override_statuses: Option<&[String]>,
    limit: usize,
) -> CompiledSqlQuery {
    let mut query = Query::select();
    query.column(Asterisk).from(Alias::new("listings"));

    let cond = compile_filters_where_condition(filters, relaxed, override_statuses);
    query.cond_where(cond);

    match filters.sort_by.as_str() {
        "price_asc" => {
            query
                .order_by_expr(
                    Expr::cust("CASE WHEN price IS NULL THEN 1 ELSE 0 END"),
                    Order::Asc,
                )
                .order_by(Alias::new("price"), Order::Asc);
        }
        "price_desc" => {
            query.order_by_expr(Expr::cust("COALESCE(price, 0)"), Order::Desc);
        }
        "monthly_asc" => {
            query
                .order_by_expr(
                    Expr::cust("CASE WHEN all_in_monthly IS NULL THEN 1 ELSE 0 END"),
                    Order::Asc,
                )
                .order_by(Alias::new("all_in_monthly"), Order::Asc)
                .order_by(Alias::new("price"), Order::Asc);
        }
        "electric_rate_asc" => {
            query
                .order_by_expr(
                    Expr::cust("CASE WHEN electric_rate_kwh IS NULL THEN 1 ELSE 0 END"),
                    Order::Asc,
                )
                .order_by(Alias::new("electric_rate_kwh"), Order::Asc)
                .order_by(Alias::new("price"), Order::Asc);
        }
        "internet_desc" => {
            query
                .order_by_expr(Expr::cust("COALESCE(fiber_available, 0)"), Order::Desc)
                .order_by_expr(Expr::cust("COALESCE(max_download_mbps, 0)"), Order::Desc)
                .order_by(Alias::new("price"), Order::Asc);
        }
        "newest" => {
            query.order_by(Alias::new("status_changed_at"), Order::Desc);
        }
        "school_desc" => {
            // Known official school scores first (highest first), unknown (null) after them.
            query
                .order_by_expr(
                    Expr::cust("CASE WHEN school_score IS NULL THEN 1 ELSE 0 END"),
                    Order::Asc,
                )
                .order_by(Alias::new("school_score"), Order::Desc)
                .order_by(Alias::new("price"), Order::Asc);
        }
        "sqft_desc" => {
            query.order_by_expr(Expr::cust("COALESCE(sqft, 0)"), Order::Desc);
        }
        _ => {
            query
                .order_by_expr(
                    Expr::cust(
                        "CASE status WHEN 'for_sale' THEN 0 WHEN 'pending' THEN 1 WHEN 'sold' THEN 2 ELSE 3 END",
                    ),
                    Order::Asc,
                )
                .order_by_expr(Expr::cust("COALESCE(school_score, 0)"), Order::Desc)
                .order_by(Alias::new("price"), Order::Asc);
        }
    }

    let safe_limit = limit.clamp(1, 500) as u64;
    query.limit(safe_limit);

    let (sql, sea_vals) = if dialect == "mysql" {
        query.build(MysqlQueryBuilder)
    } else {
        query.build(SqliteQueryBuilder)
    };
    let params: Vec<SqlParam> = sea_vals.0.into_iter().map(SqlParam::from).collect();

    // Build readable preview string purely for UI diagnostics (never executed)
    let mut sql_preview = String::with_capacity(sql.len() + 64);
    let mut param_iter = params.iter();
    for ch in sql.chars() {
        if ch == '?' {
            if let Some(p) = param_iter.next() {
                sql_preview.push_str(&format_preview_param(p));
            } else {
                sql_preview.push('?');
            }
        } else {
            sql_preview.push(ch);
        }
    }

    CompiledSqlQuery {
        sql,
        params,
        sql_preview,
    }
}

const NARROW_LISTING_CORE_PROJ: &str = "l.listing_sk, l.external_id, l.report_slug, l.zip, l.state_code, l.county_fips, l.city, l.street, l.address, l.lat, l.lon, l.status, l.property_category, l.price, l.list_price, l.sold_price, l.estimated_value, l.price_per_sqft, l.hoa_monthly, l.all_in_monthly, l.beds, l.baths, l.sqft, l.lot_sqft, l.year_built, l.stories, l.days_on_market, l.listed_epoch_day, l.sold_epoch_day, l.school_score, l.school_grade, l.walk_score, l.safety_score, l.overall_score, l.electric_rate_kwh, l.max_download_mbps, l.tag_bits, l.known_bits, l.first_seen_ts, l.last_seen_ts, l.status_changed_ts, l.price_changed_ts";

fn render_sql_preview(sql: &str, params: &[SqlParam]) -> String {
    let mut sql_preview = String::with_capacity(sql.len() + 64);
    let mut param_iter = params.iter();
    for ch in sql.chars() {
        if ch == '?' {
            if let Some(p) = param_iter.next() {
                sql_preview.push_str(&format_preview_param(p));
            } else {
                sql_preview.push('?');
            }
        } else {
            sql_preview.push(ch);
        }
    }
    sql_preview
}

/// Narrow v3 SQL compiler over `listings l` (`Q1_pure_sql`):
/// - Uses Rapid-safe bitmask predicates (`sql_bitmask_all_set_expr` / `sql_bitmask_none_set_expr` / `sql_bit_is_set_expr` — zero MySQL `&`, zero `LOWER(tags_json) LIKE`).
/// - Drops index-defeating `LOWER(l.city)` and `UPPER(l.state_code)`.
/// - Attaches 1:1 domain `LEFT JOIN`s ONLY when non-core filter columns are requested.
pub fn compile_core_filters_to_sql(
    filters: &SearchFilters,
    dialect: &str,
    rapid_hint: &str,
    relaxed: bool,
    override_statuses: Option<&[String]>,
    limit: usize,
) -> CompiledSqlQuery {
    let need_utilities = (!relaxed
        && (!filters.internet_carriers.is_empty() || !filters.electric_carriers.is_empty()))
        || filters.max_monthly_utilities.is_some();
    let need_hazards =
        !relaxed && (filters.max_noise_db.is_some() || filters.max_seismic_pga.is_some());
    let need_features =
        !relaxed && (filters.min_garage_spaces.is_some() || !filters.architectural_styles.is_empty());
    let need_descriptions = !relaxed
        && filters
            .must_have_keywords
            .iter()
            .take(3)
            .any(|k| !k.trim().is_empty());

    let mut joins = String::new();
    if need_utilities {
        joins.push_str(" LEFT JOIN listing_utilities lu ON lu.listing_sk = l.listing_sk");
    }
    if need_hazards {
        joins.push_str(" LEFT JOIN listing_hazards lh ON lh.listing_sk = l.listing_sk");
    }
    if need_features {
        joins.push_str(" LEFT JOIN listing_features lf ON lf.listing_sk = l.listing_sk");
    }
    if need_descriptions {
        joins.push_str(" LEFT JOIN listing_descriptions ld ON ld.listing_sk = l.listing_sk");
    }

    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    let raw_statuses: &[String] = override_statuses.unwrap_or_else(|| {
        if filters.statuses.is_empty() {
            &[]
        } else {
            filters.statuses.as_slice()
        }
    });
    let mut status_codes: Vec<i64> = if raw_statuses.is_empty() {
        vec![1, 2]
    } else {
        raw_statuses
            .iter()
            .map(|s| i64::from(proto::listing_status_to_code(Some(s))))
            .collect()
    };
    status_codes.sort_unstable();
    status_codes.dedup();
    if status_codes.len() == 1 {
        clauses.push("l.status = ?".to_string());
        params.push(SqlParam::I64(status_codes[0]));
    } else if !status_codes.is_empty() {
        let ph = vec!["?"; status_codes.len()].join(", ");
        clauses.push(format!("l.status IN ({ph})"));
        for c in status_codes {
            params.push(SqlParam::I64(c));
        }
    }

    if !filters.zips.is_empty() || !filters.cities.is_empty() {
        let mut loc_or: Vec<String> = Vec::new();
        if !filters.zips.is_empty() {
            if filters.zips.len() == 1 {
                loc_or.push("l.zip = ?".to_string());
                params.push(SqlParam::Text(filters.zips[0].clone()));
            } else {
                let ph = vec!["?"; filters.zips.len()].join(", ");
                loc_or.push(format!("l.zip IN ({ph})"));
                for z in &filters.zips {
                    params.push(SqlParam::Text(z.clone()));
                }
            }
        }
        if !filters.cities.is_empty() {
            let mut city_or: Vec<String> = Vec::new();
            for c in &filters.cities {
                let clean_c = c.trim();
                if clean_c.is_empty() {
                    continue;
                }
                if dialect == "mysql" {
                    city_or.push("l.city = ?".to_string());
                } else {
                    city_or.push("l.city = ? COLLATE NOCASE".to_string());
                }
                params.push(SqlParam::Text(clean_c.to_string()));
            }
            if !city_or.is_empty() {
                loc_or.push(format!("({})", city_or.join(" OR ")));
            }
        }
        if !loc_or.is_empty() {
            clauses.push(if loc_or.len() == 1 {
                loc_or.remove(0)
            } else {
                format!("({})", loc_or.join(" OR "))
            });
        }
    }

    if !filters.states.is_empty() && filters.zips.is_empty() {
        let mut clean_states: Vec<String> = filters
            .states
            .iter()
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        clean_states.sort();
        clean_states.dedup();
        if clean_states.len() == 1 {
            clauses.push("l.state_code = ?".to_string());
            params.push(SqlParam::Text(clean_states.remove(0)));
        } else if !clean_states.is_empty() {
            let ph = vec!["?"; clean_states.len()].join(", ");
            clauses.push(format!("l.state_code IN ({ph})"));
            for st in clean_states {
                params.push(SqlParam::Text(st));
            }
        }
    }

    if let Some(min_p) = filters.min_price {
        let eff = ((min_p as f64) * if relaxed { 0.88 } else { 1.0 }).round() as i64;
        clauses.push("l.price >= ?".to_string());
        params.push(SqlParam::I64(eff));
    }
    if let Some(max_p) = filters.max_price {
        let eff = ((max_p as f64) * if relaxed { 1.15 } else { 1.0 }).round() as i64;
        clauses.push("l.price <= ?".to_string());
        params.push(SqlParam::I64(eff));
    }

    if let Some(min_b) = filters.min_beds {
        let eff = (min_b - if relaxed { 1.0 } else { 0.0 }).max(1.0);
        clauses.push("(l.beds IS NULL OR l.beds >= ?)".to_string());
        params.push(SqlParam::F64(eff));
    }
    if let (Some(max_b), false) = (filters.max_beds, relaxed) {
        clauses.push("l.beds <= ?".to_string());
        params.push(SqlParam::F64(max_b));
    }

    if let Some(min_ba) = filters.min_baths {
        let eff = (min_ba - if relaxed { 0.5 } else { 0.0 }).max(1.0);
        clauses.push("(l.baths IS NULL OR l.baths >= ?)".to_string());
        params.push(SqlParam::F64(eff));
    }

    if let Some(min_sq) = filters.min_sqft {
        let eff = ((min_sq as f64) * if relaxed { 0.85 } else { 1.0 }).round() as i64;
        clauses.push("(l.sqft IS NULL OR l.sqft >= ?)".to_string());
        params.push(SqlParam::I64(eff));
    }
    if let (Some(max_sq), false) = (filters.max_sqft, relaxed) {
        clauses.push("l.sqft <= ?".to_string());
        params.push(SqlParam::I64(max_sq));
    }

    if let Some(min_lot) = filters.min_lot_sqft {
        let eff = ((min_lot as f64) * if relaxed { 0.8 } else { 1.0 }).round() as i64;
        clauses.push("(l.lot_sqft IS NULL OR l.lot_sqft >= ?)".to_string());
        params.push(SqlParam::I64(eff));
    }

    if !relaxed {
        if let Some(min_yr) = filters.min_year_built {
            clauses.push("(l.year_built IS NULL OR l.year_built >= ?)".to_string());
            params.push(SqlParam::I64(min_yr));
        }
        if let Some(max_yr) = filters.max_year_built {
            clauses.push("(l.year_built IS NULL OR l.year_built <= ?)".to_string());
            params.push(SqlParam::I64(max_yr));
        }
        if let Some(st) = filters.stories {
            clauses.push("(l.stories IS NULL OR l.stories = ?)".to_string());
            params.push(SqlParam::F64(st as f64));
        }
        if let Some(max_hoa) = filters.max_hoa_monthly {
            clauses.push("(l.hoa_monthly IS NULL OR l.hoa_monthly <= ?)".to_string());
            params.push(SqlParam::I64(max_hoa));
        }
        if !filters.property_types.is_empty() {
            let mut cat_codes: Vec<i64> = Vec::new();
            for pt in &filters.property_types {
                let code = proto::property_category_to_code(Some(pt));
                if code == PropertyCategory::Manufactured as i32
                    || code == PropertyCategory::LandLot as i32
                    || pt.eq_ignore_ascii_case("Manufactured / Land")
                {
                    cat_codes.push(PropertyCategory::Manufactured as i64);
                    cat_codes.push(PropertyCategory::LandLot as i64);
                } else if code > 0 {
                    cat_codes.push(i64::from(code));
                }
            }
            cat_codes.sort_unstable();
            cat_codes.dedup();
            if cat_codes.len() == 1 {
                clauses.push("l.property_category = ?".to_string());
                params.push(SqlParam::I64(cat_codes[0]));
            } else if !cat_codes.is_empty() {
                let ph = vec!["?"; cat_codes.len()].join(", ");
                clauses.push(format!("l.property_category IN ({ph})"));
                for c in cat_codes {
                    params.push(SqlParam::I64(c));
                }
            }
        }
    }

    let eff_school = filters
        .min_school_score
        .or_else(|| school_grade_to_min_score(filters.min_school_grade.as_deref()));
    if let Some(sc) = eff_school {
        let threshold = round_n(sc - if relaxed { 1.0 } else { 0.2 }, 1);
        clauses.push("(l.school_score IS NULL OR l.school_score >= ?)".to_string());
        params.push(SqlParam::F64(threshold));
    }

    if !relaxed {
        let (mut req_mask, mut excl_mask) = proto::build_search_filter_tag_masks(
            filters.fiber_only,
            filters.high_speed_internet_only,
            filters.municipal_electric_only,
            filters.municipal_water_only,
            filters.municipal_sewer_only,
            filters.has_solar,
            filters.has_ev_charger,
            filters.has_heat_pump,
            filters.has_adu,
            filters.adu_feasible,
            filters.has_pool,
            filters.exclude_special_tax,
            &[],
        );
        if filters.has_hardwood_floors {
            req_mask |= proto::feature_tag_bit(FeatureTag::HardwoodFloors);
        }
        if filters.has_updated_kitchen {
            req_mask |= proto::feature_tag_bit(FeatureTag::UpdatedKitchen);
        }
        if filters.remodeled_only {
            req_mask |= proto::feature_tag_bit(FeatureTag::Remodeled);
        }
        if filters.quiet_street_only {
            req_mask |= proto::feature_tag_bit(FeatureTag::QuietStreet);
        }
        if filters.exclude_high_flood_risk {
            excl_mask |= proto::feature_tag_bit(FeatureTag::InSfhaFlood);
        }
        if filters.exclude_high_wildfire_risk {
            excl_mask |= proto::feature_tag_bit(FeatureTag::HighWildfire);
        }
        if req_mask > 0 {
            clauses.push(sql_bitmask_all_set_expr(dialect, "l.tag_bits", req_mask));
        }
        if excl_mask > 0 {
            clauses.push(sql_bitmask_none_set_expr(dialect, "l.tag_bits", excl_mask));
        }

        if let Some(min_dl) = filters.min_download_mbps {
            clauses.push("l.max_download_mbps >= ?".to_string());
            params.push(SqlParam::I64(min_dl));
        }
        if let Some(min_safety) = filters.min_safety_score {
            clauses.push("l.safety_score >= ?".to_string());
            params.push(SqlParam::F64(min_safety));
        }

        if !filters.internet_carriers.is_empty() {
            let mut isp_or = Vec::new();
            for c in filters.internet_carriers.iter().take(6) {
                let needle = c.trim();
                if !needle.is_empty() {
                    let pat = format!("%{needle}%");
                    isp_or.push("(lu.internet_carriers LIKE ? OR lu.fiber_carriers LIKE ?)".to_string());
                    params.push(SqlParam::Text(pat.clone()));
                    params.push(SqlParam::Text(pat));
                }
            }
            if !isp_or.is_empty() {
                clauses.push(format!("({})", isp_or.join(" OR ")));
            }
        }
        if !filters.electric_carriers.is_empty() {
            let mut elec_or = Vec::new();
            for c in filters.electric_carriers.iter().take(6) {
                let needle = c.trim();
                if !needle.is_empty() {
                    elec_or.push("lu.electric_carrier LIKE ?".to_string());
                    params.push(SqlParam::Text(format!("%{needle}%")));
                }
            }
            if !elec_or.is_empty() {
                clauses.push(format!("({})", elec_or.join(" OR ")));
            }
        }

        if let Some(min_g) = filters.min_garage_spaces {
            clauses.push("lf.garage_spaces >= ?".to_string());
            params.push(SqlParam::F64(min_g));
        }
        if !filters.architectural_styles.is_empty() {
            let mut st_or = Vec::new();
            for s in filters.architectural_styles.iter().take(6) {
                let needle = s.trim();
                if !needle.is_empty() {
                    let code = proto::architectural_style_str_to_code(Some(needle));
                    if code > 0 {
                        st_or.push("(lf.style_code = ? OR lf.architectural_style LIKE ?)".to_string());
                        params.push(SqlParam::I64(i64::from(code)));
                        params.push(SqlParam::Text(format!("%{needle}%")));
                    } else {
                        st_or.push("lf.architectural_style LIKE ?".to_string());
                        params.push(SqlParam::Text(format!("%{needle}%")));
                    }
                }
            }
            if !st_or.is_empty() {
                clauses.push(format!("({})", st_or.join(" OR ")));
            }
        }

        if let Some(max_db) = filters.max_noise_db {
            clauses.push("lh.noise_max_db <= ?".to_string());
            params.push(SqlParam::F64(max_db));
        }
        if let Some(max_pga) = filters.max_seismic_pga {
            clauses.push("lh.seismic_pga <= ?".to_string());
            params.push(SqlParam::F64(max_pga));
        }

        for tag in filters.required_tags.iter().take(6) {
            let clean_tag = tag.trim().to_ascii_lowercase().replace(' ', "_");
            if clean_tag.is_empty() {
                continue;
            }
            let pat_words = format!("%{}%", clean_tag.replace('_', " "));
            if let Some(ft) = proto::feature_tag_from_slug(&clean_tag) {
                let bit_expr = sql_bit_is_set_expr(dialect, "l.tag_bits", ft as u32);
                clauses.push(format!(
                    "({bit_expr} OR EXISTS (SELECT 1 FROM listing_descriptions ld_t WHERE ld_t.listing_sk = l.listing_sk AND ld_t.features_text LIKE ?))"
                ));
                params.push(SqlParam::Text(pat_words));
            } else {
                clauses.push(
                    "(EXISTS (SELECT 1 FROM listing_tags lt WHERE lt.listing_sk = l.listing_sk AND lt.tag_code = ?) \
                      OR EXISTS (SELECT 1 FROM listing_descriptions ld_t WHERE ld_t.listing_sk = l.listing_sk AND ld_t.features_text LIKE ?))"
                        .to_string(),
                );
                params.push(SqlParam::Text(clean_tag));
                params.push(SqlParam::Text(pat_words));
            }
        }
    }

    if let Some(max_r) = filters.max_electric_rate_kwh {
        let eff = round_n(max_r * if relaxed { 1.15 } else { 1.0 }, 4);
        clauses.push("l.electric_rate_kwh <= ?".to_string());
        params.push(SqlParam::F64(eff));
    }
    if let Some(max_u) = filters.max_monthly_utilities {
        let eff = ((max_u as f64) * if relaxed { 1.15 } else { 1.0 }).round() as i64;
        clauses.push("lu.est_monthly_utilities <= ?".to_string());
        params.push(SqlParam::I64(eff));
    }
    if let Some(max_m) = filters.max_all_in_monthly {
        let eff = ((max_m as f64) * if relaxed { 1.15 } else { 1.0 }).round() as i64;
        clauses.push("l.all_in_monthly <= ?".to_string());
        params.push(SqlParam::I64(eff));
    }

    if need_descriptions {
        for kw in filters.must_have_keywords.iter().take(3) {
            let clean_kw = kw.trim();
            if !clean_kw.is_empty() {
                clauses.push("ld.features_text LIKE ?".to_string());
                params.push(SqlParam::Text(format!("%{clean_kw}%")));
            }
        }
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let order_sql = match filters.sort_by.as_str() {
        "price_asc" => "CASE WHEN l.price IS NULL THEN 1 ELSE 0 END ASC, l.price ASC".to_string(),
        "price_desc" => "COALESCE(l.price, 0) DESC".to_string(),
        "monthly_asc" => {
            "CASE WHEN l.all_in_monthly IS NULL THEN 1 ELSE 0 END ASC, l.all_in_monthly ASC, l.price ASC"
                .to_string()
        }
        "electric_rate_asc" => {
            "CASE WHEN l.electric_rate_kwh IS NULL THEN 1 ELSE 0 END ASC, l.electric_rate_kwh ASC, l.price ASC"
                .to_string()
        }
        "internet_desc" => {
            let fiber_expr =
                sql_bit_is_set_expr(dialect, "l.tag_bits", FeatureTag::FiberAvailable as u32);
            format!(
                "CASE WHEN {fiber_expr} THEN 1 ELSE 0 END DESC, COALESCE(l.max_download_mbps, 0) DESC, l.price ASC"
            )
        }
        "newest" => "l.status_changed_ts DESC".to_string(),
        "school_desc" => {
            "CASE WHEN l.school_score IS NULL THEN 1 ELSE 0 END ASC, l.school_score DESC, l.price ASC"
                .to_string()
        }
        "sqft_desc" => "COALESCE(l.sqft, 0) DESC".to_string(),
        _ => {
            "CASE l.status WHEN 1 THEN 0 WHEN 2 THEN 1 WHEN 3 THEN 2 ELSE 3 END ASC, COALESCE(l.school_score, 0) DESC, l.price ASC"
                .to_string()
        }
    };

    let safe_limit = limit.clamp(1, 500) as i64;
    params.push(SqlParam::I64(safe_limit));
    let sql = format!(
        "SELECT {rapid_hint}{NARROW_LISTING_CORE_PROJ} FROM listings l{joins}{where_sql} ORDER BY {order_sql} LIMIT ?"
    );
    let sql_preview = render_sql_preview(&sql, &params);
    CompiledSqlQuery {
        sql,
        params,
        sql_preview,
    }
}

/// Executes narrow `Q1_pure_sql` core search (`db.search_listing_cores`) followed by top-K card
/// hydration (`db.hydrate_listing_cards`) on ONLY the returned `limit` (e.g. 24) candidates.
pub async fn search_listings_narrow(
    db: &DbStore,
    filters: &SearchFilters,
    relaxed: bool,
    override_statuses: Option<&[String]>,
    limit: usize,
) -> Result<(CompiledSqlQuery, Vec<ListingRow>)> {
    let compiled = compile_core_filters_to_sql(
        filters,
        db.dialect(),
        "",
        relaxed,
        override_statuses,
        limit,
    );
    let cores = db
        .search_listing_cores(&compiled.sql, &compiled.params)
        .await?;
    let cards = db.hydrate_listing_cards(&cores).await?;
    let rows = cards.iter().map(ListingFullView::to_listing_row).collect();
    Ok((compiled, rows))
}

// ---------------------------------------------------------------- Deterministic Scorer
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeterministicMatchScore {
    pub match_score: i64,
    pub match_tier: String,
    pub why_it_matches: String,
    pub tradeoffs: Option<String>,
}

pub fn score_listing_deterministic(
    listing: &ListingRow,
    filters: &SearchFilters,
) -> DeterministicMatchScore {
    let mut score: f64 = 82.0;
    let mut pros: Vec<String> = Vec::new();
    let mut cons: Vec<String> = Vec::new();

    if !filters.zips.is_empty() {
        if filters.zips.contains(&listing.zip) {
            pros.push(format!("in ZIP {}", listing.zip));
        } else {
            score -= 20.0;
            cons.push(format!("outside target ZIP ({})", listing.zip));
        }
    }

    if let (Some(max_p), Some(lp)) = (filters.max_price, listing.price) {
        if lp <= max_p {
            pros.push(format!(
                "${} fits ${} budget",
                format_int_commas(lp),
                format_int_commas(max_p)
            ));
            score += 6.0;
        } else if max_p > 0 {
            let over_pct = ((lp - max_p) as f64 / (max_p as f64)) * 100.0;
            score -= (over_pct * 2.0).round().min(30.0);
            cons.push(format!(
                "{:.1}% above ${} max",
                over_pct,
                format_int_commas(max_p)
            ));
        }
    }

    if let (Some(min_b), Some(lb)) = (filters.min_beds, listing.beds) {
        if lb >= min_b {
            pros.push(format!("{} beds", fmt_clean_num(lb)));
            score += 4.0;
        } else {
            score -= 14.0;
            cons.push(format!(
                "{} beds vs {}+ requested",
                fmt_clean_num(lb),
                fmt_clean_num(min_b)
            ));
        }
    }

    if let (Some(min_ba), Some(lba)) = (filters.min_baths, listing.baths) {
        if lba >= min_ba {
            pros.push(format!("{} baths", fmt_clean_num(lba)));
        } else {
            score -= 8.0;
            cons.push(format!(
                "{} baths vs {}+ requested",
                fmt_clean_num(lba),
                fmt_clean_num(min_ba)
            ));
        }
    }

    if let (Some(min_sq), Some(lsq)) = (filters.min_sqft, listing.sqft) {
        if lsq >= min_sq {
            pros.push(format!("{} sq ft", format_int_commas(lsq)));
        } else {
            score -= 10.0;
            cons.push(format!(
                "{} sq ft vs {}+",
                format_int_commas(lsq),
                format_int_commas(min_sq)
            ));
        }
    }

    if let (Some(min_lot), Some(llot)) = (filters.min_lot_sqft, listing.lot_sqft) {
        if llot >= min_lot {
            pros.push(format!("{} sq ft lot", format_int_commas(llot)));
        } else {
            score -= 8.0;
            cons.push(format!("smaller lot ({} sq ft)", format_int_commas(llot)));
        }
    }

    // Schools: `school_score` comes only from official public data; null means unknown. With a school
    // requirement an unknown listing is kept, earns no school bonus (so it ranks below known matches
    // and above known misses), and says so in its explanation.
    let req_school = filters
        .min_school_score
        .or_else(|| school_grade_to_min_score(filters.min_school_grade.as_deref()));
    if let Some(req_sc) = req_school {
        if let Some(lsc) = listing.school_score {
            let lg = listing.school_grade.as_deref().unwrap_or("");
            if lsc >= req_sc - 0.2 {
                score += 7.0;
                pros.push(format!("School Grade {lg} ({}/10)", fmt_clean_num(lsc)));
            } else {
                score -= 14.0;
                cons.push(format!(
                    "School {lg} ({}/10) below target",
                    fmt_clean_num(lsc)
                ));
            }
        } else {
            cons.push("school data unavailable".to_string());
        }
    } else if let Some(ref lg) = listing.school_grade {
        if let Some(lsc) = listing.school_score {
            pros.push(format!("Schools {lg} ({}/10)", fmt_clean_num(lsc)));
        }
    }

    if let (Some(max_hoa), Some(lhoa)) = (filters.max_hoa_monthly, listing.hoa_monthly) {
        if lhoa > max_hoa {
            score -= 12.0;
            cons.push(format!("HOA ${lhoa}/mo"));
        }
    }

    if filters.fiber_only {
        if listing.fiber_available == Some(1) {
            score += 6.0;
            if let Some(ref fc) = listing.fiber_carriers {
                pros.push(format!("Fiber ({fc})"));
            } else {
                pros.push("Symmetrical Fiber (FTTH)".to_string());
            }
        } else {
            score -= 15.0;
            cons.push("no FTTH fiber confirmed".to_string());
        }
    } else if listing.fiber_available == Some(1) {
        if let Some(ref fc) = listing.fiber_carriers {
            pros.push(format!(
                "Fiber ({})",
                fc.split(',').next().unwrap_or(fc).trim()
            ));
        }
    }

    if filters.municipal_electric_only {
        if listing.electric_utility_type.as_deref() == Some("municipal") {
            score += 6.0;
            if let (Some(ref ec), Some(rate)) =
                (&listing.electric_carrier, listing.electric_rate_kwh)
            {
                pros.push(format!("{ec} (${rate:.3}/kWh)"));
            } else {
                pros.push("Municipal electric utility".to_string());
            }
        } else {
            score -= 14.0;
            cons.push("IOU electric territory".to_string());
        }
    } else if let (Some(max_r), Some(rate)) =
        (filters.max_electric_rate_kwh, listing.electric_rate_kwh)
    {
        if rate <= max_r {
            score += 5.0;
            pros.push(format!("${rate:.3}/kWh electric"));
        } else {
            score -= 12.0;
            cons.push(format!("${rate:.3}/kWh exceeds ${max_r:.2}/kWh"));
        }
    }

    if let (Some(max_mo), Some(lmo)) = (filters.max_all_in_monthly, listing.all_in_monthly) {
        if lmo <= max_mo {
            score += 5.0;
            pros.push(format!("~${}/mo all-in", format_int_commas(lmo)));
        } else {
            score -= 12.0;
            cons.push(format!("~${}/mo all-in", format_int_commas(lmo)));
        }
    }

    if filters.has_solar && listing.has_solar == Some(1) {
        score += 4.0;
        pros.push("Solar".to_string());
    }
    if filters.has_ev_charger && listing.has_ev_charger == Some(1) {
        score += 4.0;
        pros.push("EV Charger".to_string());
    }

    if let Some(min_safe) = filters.min_safety_score {
        if let Some(lsafe) = listing.safety_score {
            if lsafe >= min_safe {
                score += 6.0;
                pros.push(format!("Safety {}/100", lsafe.round() as i64));
            } else {
                score -= 14.0;
                cons.push(format!(
                    "Safety {}/100 below {}/100 target",
                    lsafe.round() as i64,
                    min_safe.round() as i64
                ));
            }
        }
    }
    if filters.has_hardwood_floors {
        if listing.has_hardwood_floors == Some(1) {
            score += 4.0;
            pros.push("Hardwood floors".to_string());
        } else {
            score -= 12.0;
            cons.push("no hardwood floors confirmed".to_string());
        }
    }
    if filters.has_updated_kitchen {
        if listing.has_updated_kitchen == Some(1) {
            score += 4.0;
            pros.push("Updated kitchen".to_string());
        } else {
            score -= 12.0;
            cons.push("no updated kitchen confirmed".to_string());
        }
    }
    if filters.remodeled_only {
        if listing.has_remodeled == Some(1) {
            score += 4.0;
            pros.push("Remodeled".to_string());
        } else {
            score -= 12.0;
            cons.push("not confirmed remodeled".to_string());
        }
    }
    if filters.quiet_street_only {
        if listing.quiet_street == Some(1) {
            score += 5.0;
            pros.push("Quiet street".to_string());
        } else if listing.quiet_street == Some(0) {
            score -= 15.0;
            cons.push("higher corridor noise".to_string());
        }
    }
    if let (Some(max_db), Some(ldb)) = (filters.max_noise_db, listing.noise_max_db) {
        if ldb <= max_db {
            score += 4.0;
            pros.push(format!("Noise {} dB", ldb.round() as i64));
        } else {
            score -= 12.0;
            cons.push(format!("Noise {} dB exceeds {} dB", ldb.round() as i64, max_db.round() as i64));
        }
    }
    if let (Some(max_pga), Some(lpga)) = (filters.max_seismic_pga, listing.seismic_pga) {
        if lpga <= max_pga {
            score += 4.0;
            pros.push(format!("Seismic {lpga:.2}g"));
        } else {
            score -= 12.0;
            cons.push(format!("Seismic {lpga:.2}g exceeds {max_pga:.2}g"));
        }
    }
    let tags_lc = listing
        .tags_json
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    for tag in &filters.required_tags {
        let clean_tag = tag.trim().to_ascii_lowercase().replace(' ', "_");
        if !clean_tag.is_empty() {
            let word_form = clean_tag.replace('_', " ");
            let f_lc = listing
                .features_text
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            if tags_lc.contains(&clean_tag) || f_lc.contains(&word_form) {
                score += 4.0;
                pros.push(format!("tag:{clean_tag}"));
            } else {
                score -= 12.0;
                cons.push(format!("missing tag:{clean_tag}"));
            }
        }
    }

    let ftxt = listing
        .features_text
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    for kw in filters
        .must_have_keywords
        .iter()
        .chain(filters.nice_to_have_keywords.iter())
    {
        if !kw.is_empty() && ftxt.contains(&kw.to_ascii_lowercase()) {
            score += 4.0;
            pros.push(format!("mentions \"{kw}\""));
        }
    }
    for dbk in &filters.deal_breakers {
        if !dbk.is_empty() && ftxt.contains(&dbk.to_ascii_lowercase()) {
            score -= 18.0;
            cons.push(format!("flagged \"{dbk}\""));
        }
    }

    let clamped = (score.round() as i64).clamp(25, 99);
    let match_tier = if clamped >= 85 {
        "Strong Match"
    } else if clamped >= 68 {
        "Good Fit"
    } else {
        "Near Match"
    }
    .to_string();

    let why_it_matches = if !pros.is_empty() {
        format!(
            "Matches on {}.",
            pros.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
        )
    } else {
        format!(
            "{} in {} {}.",
            listing
                .property_type
                .as_deref()
                .unwrap_or("Residential property"),
            listing.city.as_deref().unwrap_or(""),
            listing.zip
        )
    };

    DeterministicMatchScore {
        match_score: clamped,
        match_tier,
        why_it_matches,
        tradeoffs: if cons.is_empty() {
            None
        } else {
            Some(cons.join("; "))
        },
    }
}

// ---------------------------------------------------------------- OpenAI Responses API (`LLM_MATCH_MODEL` -> `LLM_FALLBACK_MODEL`)
fn resolve_match_model_specs(env: &EnvConfig) -> Vec<String> {
    let primary_raw = env.get("LLM_MATCH_MODEL").unwrap_or(DEFAULT_MATCH_MODEL);
    let fb_raw = env
        .get("LLM_FALLBACK_MODEL")
        .unwrap_or(DEFAULT_FALLBACK_MODEL);
    let mut out = Vec::new();
    for spec in std::iter::once(primary_raw).chain(fb_raw.split(',')) {
        let trimmed = spec.trim();
        if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("none") {
            let parsed = parse_model_spec(trimmed, None);
            if !parsed.model.is_empty() {
                let formatted = match parsed.effort {
                    Some(eff) => format!("{}@{eff}", parsed.model),
                    None => parsed.model,
                };
                if !out.contains(&formatted) {
                    out.push(formatted);
                }
            }
        }
    }
    out
}

fn requirements_parse_schema() -> Value {
    serde_json::from_str(
        r#"{
            "type": "object",
            "additionalProperties": false,
            "required": [
                "onTopic", "intentSummary", "zips", "cities", "states", "statuses",
                "minPrice", "maxPrice", "minBeds", "maxBeds", "minBaths", "minSqft", "maxSqft",
                "minLotSqft", "minYearBuilt", "maxYearBuilt", "stories", "maxHoaMonthly",
                "propertyTypes", "minSchoolScore", "minSchoolGrade", "excludeHighFloodRisk",
                "excludeHighWildfireRisk", "mustHaveKeywords", "niceToHaveKeywords",
                "dealBreakers", "sortBy", "fairHousingNotice",
                "minSafetyScore", "hasHardwoodFloors", "hasUpdatedKitchen", "remodeledOnly",
                "quietStreetOnly", "maxNoiseDb", "maxSeismicPga", "requiredTags"
            ],
            "properties": {
                "onTopic": { "type": "boolean" },
                "intentSummary": { "type": "string" },
                "zips": { "type": "array", "maxItems": 10, "items": { "type": "string" } },
                "cities": { "type": "array", "maxItems": 6, "items": { "type": "string" } },
                "states": { "type": "array", "maxItems": 4, "items": { "type": "string" } },
                "statuses": {
                    "type": "array", "maxItems": 4,
                    "items": { "type": "string", "enum": ["for_sale", "pending", "sold", "off_market"] }
                },
                "minPrice": { "type": ["integer", "null"] },
                "maxPrice": { "type": ["integer", "null"] },
                "minBeds": { "type": ["number", "null"] },
                "maxBeds": { "type": ["number", "null"] },
                "minBaths": { "type": ["number", "null"] },
                "minSqft": { "type": ["integer", "null"] },
                "maxSqft": { "type": ["integer", "null"] },
                "minLotSqft": { "type": ["integer", "null"] },
                "minYearBuilt": { "type": ["integer", "null"] },
                "maxYearBuilt": { "type": ["integer", "null"] },
                "stories": { "type": ["integer", "null"] },
                "maxHoaMonthly": { "type": ["integer", "null"] },
                "propertyTypes": {
                    "type": "array", "maxItems": 5,
                    "items": {
                        "type": "string",
                        "enum": [
                            "Single-Family Detached",
                            "Townhome / Attached",
                            "Condo / Co-op",
                            "2–4 Unit Multifamily",
                            "Manufactured / Land"
                        ]
                    }
                },
                "minSchoolScore": { "type": ["number", "null"] },
                "minSchoolGrade": {
                    "type": ["string", "null"],
                    "enum": ["A+", "A", "A-", "B+", "B", "B-", "C+", "C", "D", null]
                },
                "excludeHighFloodRisk": { "type": "boolean" },
                "excludeHighWildfireRisk": { "type": "boolean" },
                "mustHaveKeywords": { "type": "array", "maxItems": 6, "items": { "type": "string" } },
                "niceToHaveKeywords": { "type": "array", "maxItems": 8, "items": { "type": "string" } },
                "dealBreakers": { "type": "array", "maxItems": 6, "items": { "type": "string" } },
                "sortBy": {
                    "type": "string",
                    "enum": ["best_match", "price_asc", "price_desc", "newest", "school_desc", "sqft_desc"]
                },
                "fairHousingNotice": { "type": ["string", "null"] },
                "minSafetyScore": { "type": ["number", "null"] },
                "hasHardwoodFloors": { "type": "boolean" },
                "hasUpdatedKitchen": { "type": "boolean" },
                "remodeledOnly": { "type": "boolean" },
                "quietStreetOnly": { "type": "boolean" },
                "maxNoiseDb": { "type": ["number", "null"] },
                "maxSeismicPga": { "type": ["number", "null"] },
                "requiredTags": { "type": "array", "maxItems": 8, "items": { "type": "string" } }
            }
        }"#,
    )
    .expect("valid requirements_parse_schema JSON")
}

fn candidate_rank_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["overview", "matches"],
        "properties": {
            "overview": { "type": "string" },
            "matches": {
                "type": "array",
                "maxItems": 15,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "whyItMatches", "tradeoffs"],
                    "properties": {
                        "id": { "type": "string" },
                        "whyItMatches": { "type": "string" },
                        "tradeoffs": { "type": ["string", "null"] }
                    }
                }
            }
        }
    })
}

struct CodexJsonOutcome {
    data: Value,
    model: String,
    effort: Option<String>,
    fallback_used: bool,
}

async fn call_codex_json(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    schema_name: &str,
    instructions: &str,
    input_text: &str,
    schema: &Value,
    max_output_tokens: u32,
) -> Result<CodexJsonOutcome> {
    let models = resolve_match_model_specs(env);
    if models.is_empty() {
        return Err(anyhow!("No LLM match models configured"));
    }
    let mut opts = LlmCallOptions::new(
        "property_search",
        schema_name,
        instructions,
        input_text,
        schema.clone(),
    );
    opts.models = models;
    opts.max_output_tokens = Some(max_output_tokens);
    opts.timeout_ms = Some(45_000);

    let cache = DbLlmCache(db.clone());
    let res = responses_json_with_cache(http, env, opts, Some(&cache)).await?;
    Ok(CodexJsonOutcome {
        data: res.data,
        model: res.model,
        effort: res.effort,
        fallback_used: res.fallback_used,
    })
}

#[derive(Debug, Clone)]
pub struct ParsedQueryPlan {
    pub filters: SearchFilters,
    pub sanitized_query: String,
    pub model: String,
    pub effort: Option<String>,
    pub fallback_used: bool,
}

pub async fn parse_requirements_to_query(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    query_text: &str,
    explicit_zips: &[String],
    explicit_statuses: Option<&[String]>,
) -> Result<ParsedQueryPlan> {
    let clean_query = validate_search_requirements(query_text)?;
    let has_fha = FAIR_HOUSING_DEMOGRAPHIC_RE.is_match(&clean_query);

    let mut model_used = "heuristic-parser".to_string();
    let mut effort_used: Option<String> = None;
    let mut fallback_used = false;
    let mut filters = if env.llm_configured() {
        let schema = requirements_parse_schema();
        let wrapped = wrap_untrusted_block("untrusted_buyer_requirements", &clean_query);
        let parse_instructions = crate::shared::apply_brand_to_str(
            PARSE_INSTRUCTIONS,
            &env.brand_name(),
            &env.brand_domain(),
        );
        match call_codex_json(
            db,
            http,
            env,
            "openestate_search_query_plan",
            &parse_instructions,
            &wrapped,
            &schema,
            2048,
        )
        .await
        {
            Ok(res) => match serde_json::from_value::<SearchFilters>(res.data) {
                Ok(parsed) => {
                    model_used = res.model;
                    effort_used = res.effort;
                    fallback_used = res.fallback_used;
                    parsed
                }
                Err(_) => parse_requirements_heuristic(&clean_query),
            },
            Err(_) => parse_requirements_heuristic(&clean_query),
        }
    } else {
        parse_requirements_heuristic(&clean_query)
    };
    apply_shopping_heuristics(&clean_query, &mut filters);

    if has_fha {
        scrub_demographic_keywords(&mut filters);
        if filters.fair_housing_notice.is_none() {
            filters.fair_housing_notice = Some(crate::shared::apply_brand_to_str(
                FAIR_HOUSING_NOTICE_TEXT,
                &env.brand_name(),
                &env.brand_domain(),
            ));
        }
    }

    // Merge explicit ZIP chips
    let mut merged_zips: Vec<String> = Vec::new();
    for z in explicit_zips.iter().chain(filters.zips.iter()) {
        if let Some(m) = ZIP_TOKEN_RE.captures(z.trim()).and_then(|c| c.get(1)) {
            let five = m.as_str().to_string();
            if !merged_zips.contains(&five) {
                merged_zips.push(five);
            }
        }
    }
    filters.zips = merged_zips;

    if let Some(exp_st) = explicit_statuses.filter(|s| !s.is_empty()) {
        filters.statuses = exp_st.to_vec();
    } else if filters.statuses.is_empty() {
        filters.statuses = vec!["for_sale".to_string(), "pending".to_string()];
    }

    if filters.min_school_grade.is_some() && filters.min_school_score.is_none() {
        filters.min_school_score = school_grade_to_min_score(filters.min_school_grade.as_deref());
    }

    Ok(ParsedQueryPlan {
        filters,
        sanitized_query: clean_query,
        model: model_used,
        effort: effort_used,
        fallback_used,
    })
}

#[derive(Debug, Clone)]
pub struct RankedCandidatesResult {
    pub overview: String,
    pub ranked: Vec<Value>,
}

/// Builds a compact `reportHighlights` object from a cached report and listing row,
/// preserving only verified present values (zero invented numbers).
pub fn build_candidate_report_highlights(report: &Value, l: &ListingRow) -> Option<Value> {
    let mut hl = serde_json::Map::new();

    if let Some(est) = report
        .pointer("/modules/valuation/data/estimate/value")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|x| x.round() as i64)))
        .or(l.estimated_value)
    {
        hl.insert("estimateValue".to_string(), json!(est));
    }

    if let Some(mc) = report
        .pointer("/modules/valuation/data/ownership/monthly/total")
        .or_else(|| {
            report.pointer("/modules/buyerDiligence/data/ongoingCosts/allInMonthlyBurn/allInMonthlyBurnUsd")
        })
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|x| x.round() as i64)))
        .or(l.all_in_monthly)
    {
        hl.insert("monthlyCost".to_string(), json!(mc));
    }

    if let Some(re) = report
        .pointer("/modules/valuation/data/ownership/rentEstimate")
        .or_else(|| report.pointer("/facts/rentEstimate"))
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|x| x.round() as i64)))
    {
        hl.insert("rentEstimate".to_string(), json!(re));
    }

    if let Some(fz) = report
        .pointer("/modules/hazards/data/flood/zone")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| l.flood_zone.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    {
        hl.insert("floodZone".to_string(), json!(fz));
    }

    if let Some(pga) = report
        .pointer("/modules/hazards/data/earthquake/designMaps/pgaM")
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite())
    {
        hl.insert("seismicPgaG".to_string(), json!(pga));
    }

    if let Some(wf) = report
        .pointer("/modules/hazards/data/wildfire/california/fhsz")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            report
                .pointer("/modules/hazards/data/wildfire/whp/label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            l.wildfire_zone
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
    {
        hl.insert("wildfireHazard".to_string(), json!(wf));
    }

    if let Some(elec) = report
        .pointer("/modules/buyerDiligence/data/ongoingCosts/monthlyUtilities/electricCarrier/shortName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            report
                .pointer("/modules/buyerDiligence/data/ongoingCosts/monthlyUtilities/electricCarrier/carrierName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            l.electric_carrier
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
    {
        hl.insert("utilityElectric".to_string(), json!(elec));
    }

    if let Some(fiber) = report
        .pointer("/modules/buyerDiligence/data/connectivity/broadbandCatalog/fiberAvailable")
        .and_then(Value::as_bool)
        .or_else(|| {
            report
                .pointer("/modules/buyerDiligence/data/connectivity/fiberLikely")
                .and_then(Value::as_bool)
        })
        .or_else(|| l.fiber_available.map(|v| v != 0))
    {
        hl.insert("fiberAvailable".to_string(), json!(fiber));
    }

    if let Some(raw_hl) = report
        .pointer("/modules/aiAnalyst/data/headline")
        .or_else(|| report.pointer("/aiAnalyst/headline"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let clean: String = crate::shared::scrub_competitor_brands(raw_hl)
            .chars()
            .take(180)
            .collect();
        if !clean.is_empty() {
            hl.insert("aiHeadline".to_string(), json!(clean));
        }
    }

    if hl.is_empty() {
        None
    } else {
        Some(Value::Object(hl))
    }
}

/// Builds `reportHighlights` strictly from narrow `ReportHeaderProto` (`~240 B/row`) + hydrated
/// `ListingRow` fields (`Q1_highlights` — eliminates 12x N+1 `get_cached_report` 409 KB decompressions!).
pub fn build_candidate_report_highlights_from_header(
    hdr: Option<&ReportHeaderProto>,
    l: &ListingRow,
) -> Option<Value> {
    let mut hl = serde_json::Map::new();

    if let Some(est) = hdr.and_then(|h| h.estimated_value).or(l.estimated_value) {
        hl.insert("estimateValue".to_string(), json!(est));
    }
    if let Some(mc) = hdr
        .and_then(|h| h.all_in_monthly.map(i64::from))
        .or(l.all_in_monthly)
    {
        hl.insert("monthlyCost".to_string(), json!(mc));
    }
    if let Some(re) = hdr.and_then(|h| h.rent_estimate) {
        hl.insert("rentEstimate".to_string(), json!(re));
    }
    if let Some(fz) = l
        .flood_zone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        hl.insert("floodZone".to_string(), json!(fz));
    }
    if let Some(pga) = l.seismic_pga.filter(|x| x.is_finite()) {
        hl.insert("seismicPgaG".to_string(), json!(pga));
    }
    if let Some(wf) = l
        .wildfire_zone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        hl.insert("wildfireHazard".to_string(), json!(wf));
    }
    if let Some(elec) = l
        .electric_carrier
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        hl.insert("utilityElectric".to_string(), json!(elec));
    }
    if let Some(fiber) = l.fiber_available.map(|v| v != 0) {
        hl.insert("fiberAvailable".to_string(), json!(fiber));
    }
    if let Some(raw_hl) = hdr
        .and_then(|h| h.ai_headline.as_deref().or(h.verdict.as_deref()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let clean: String = crate::shared::scrub_competitor_brands(raw_hl)
            .chars()
            .take(180)
            .collect();
        if !clean.is_empty() {
            hl.insert("aiHeadline".to_string(), json!(clean));
        }
    }

    if hl.is_empty() {
        None
    } else {
        Some(Value::Object(hl))
    }
}

pub async fn rank_candidates_with_llm(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    candidates: &[ListingRow],
    sanitized_query: &str,
    filters: &SearchFilters,
) -> RankedCandidatesResult {
    if candidates.is_empty() {
        return RankedCandidatesResult {
            overview: "No matching properties were found in the database for these criteria."
                .to_string(),
            ranked: Vec::new(),
        };
    }

    let mut pre_scored: Vec<(&ListingRow, DeterministicMatchScore)> = candidates
        .iter()
        .map(|c| (c, score_listing_deterministic(c, filters)))
        .collect();
    pre_scored.sort_by(|a, b| b.1.match_score.cmp(&a.1.match_score));

    let make_merged_json =
        |row: &ListingRow, score: i64, tier: &str, why: &str, tradeoffs: Option<&str>| -> Value {
            let mut v = format_listing_row(row);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("matchScore".to_string(), json!(score));
                obj.insert("matchTier".to_string(), json!(tier));
                obj.insert("whyItMatches".to_string(), json!(why));
                obj.insert("tradeoffs".to_string(), json!(tradeoffs));
            }
            v
        };

    if !env.llm_configured() {
        let ranked = pre_scored
            .iter()
            .map(|(row, det)| {
                make_merged_json(
                    row,
                    det.match_score,
                    &det.match_tier,
                    &det.why_it_matches,
                    det.tradeoffs.as_deref(),
                )
            })
            .collect();
        return RankedCandidatesResult {
            overview: format!(
                "Found {} matching properties in the database.",
                candidates.len()
            ),
            ranked,
        };
    }

    let top_slugs: Vec<String> = pre_scored
        .iter()
        .take(12)
        .filter_map(|(l, _)| {
            l.report_slug
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .collect();
    let header_map: HashMap<String, ReportHeaderProto> = if top_slugs.is_empty() {
        HashMap::new()
    } else {
        db.get_report_headers_batch(&top_slugs)
            .await
            .unwrap_or_default()
    };

    let mut compact: Vec<Value> = Vec::with_capacity(pre_scored.len().min(12));
    for (l, _) in pre_scored.iter().take(12) {
        let schools_joined = [
            l.school_elem_name.as_deref(),
            l.school_mid_name.as_deref(),
            l.school_high_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let feat: String = l
            .features_text
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(220)
            .collect();
        let mut cand = json!({
            "id": l.id,
            "address": l.address,
            "zip": l.zip,
            "status": l.status,
            "price": l.price,
            "beds": l.beds,
            "baths": l.baths,
            "sqft": l.sqft,
            "lotSqft": l.lot_sqft,
            "yearBuilt": l.year_built,
            "stories": l.stories,
            "propertyType": l.property_type,
            "hoaMonthly": l.hoa_monthly,
            "schoolGrade": l.school_grade,
            "schoolScore": l.school_score,
            "schools": schools_joined,
            "floodZone": l.flood_zone,
            "soldDate": l.sold_date,
            "features": feat,
        });
        if let Some(slug) = l
            .report_slug
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(hdr) = header_map.get(slug) {
                if let Some(highlights) = build_candidate_report_highlights_from_header(Some(hdr), l) {
                    if let Some(obj) = cand.as_object_mut() {
                        obj.insert("reportHighlights".to_string(), highlights);
                    }
                }
            }
        }
        compact.push(cand);
    }

    let input_payload = format!(
        "CANDIDATE_LISTINGS_JSON:\n{}\n\n{}",
        serde_json::to_string(&compact).unwrap_or_else(|_| "[]".to_string()),
        wrap_untrusted_block("untrusted_buyer_requirements", sanitized_query)
    );
    let schema = candidate_rank_schema();

    let rank_instructions = crate::shared::apply_brand_to_str(
        RANK_INSTRUCTIONS,
        &env.brand_name(),
        &env.brand_domain(),
    );
    match call_codex_json(
        db,
        http,
        env,
        "openestate_candidate_ranker",
        &rank_instructions,
        &input_payload,
        &schema,
        3200,
    )
    .await
    {
        Ok(res) => {
            let mut by_id: HashMap<String, Value> = HashMap::new();
            if let Some(arr) = res.data.get("matches").and_then(Value::as_array) {
                for m in arr {
                    if let Some(id) = str_from_val(m.get("id")) {
                        by_id.insert(id, m.clone());
                    }
                }
            }
            // Keep strictly deterministic numeric scores, tiers, and sort order (`pre_scored` order);
            // LLM contributes only text explanations after guardrail inspection.
            let ranked: Vec<Value> = pre_scored
                .iter()
                .map(|(row, det)| {
                    if let Some(llm_hit) = by_id.get(&row.id) {
                        let raw_why = str_from_val(llm_hit.get("whyItMatches"))
                            .unwrap_or_else(|| det.why_it_matches.clone());
                        let why = if inspect_output_guardrail(&raw_why) {
                            crate::shared::scrub_competitor_brands(&det.why_it_matches)
                        } else {
                            let scrubbed = crate::shared::scrub_competitor_brands(&raw_why);
                            if scrubbed.trim().is_empty() {
                                crate::shared::scrub_competitor_brands(&det.why_it_matches)
                            } else {
                                scrubbed
                            }
                        };
                        let raw_trade = str_from_val(llm_hit.get("tradeoffs"));
                        let trade = match raw_trade {
                            Some(ref t) if inspect_output_guardrail(t) => det
                                .tradeoffs
                                .as_deref()
                                .map(crate::shared::scrub_competitor_brands)
                                .filter(|s| !s.trim().is_empty()),
                            Some(ref t) => {
                                let s = crate::shared::scrub_competitor_brands(t);
                                (!s.trim().is_empty()).then_some(s)
                            }
                            None => None,
                        };
                        make_merged_json(
                            row,
                            det.match_score,
                            &det.match_tier,
                            &why,
                            trade.as_deref(),
                        )
                    } else {
                        let clean_why = crate::shared::scrub_competitor_brands(&det.why_it_matches);
                        let clean_trade = det
                            .tradeoffs
                            .as_deref()
                            .map(crate::shared::scrub_competitor_brands)
                            .filter(|s| !s.trim().is_empty());
                        make_merged_json(
                            row,
                            det.match_score,
                            &det.match_tier,
                            &clean_why,
                            clean_trade.as_deref(),
                        )
                    }
                })
                .collect();
            let raw_overview = str_from_val(res.data.get("overview")).unwrap_or_else(|| {
                format!(
                    "Evaluated {} matching homes against your criteria.",
                    ranked.len()
                )
            });
            let overview = if inspect_output_guardrail(&raw_overview) {
                format!(
                    "Evaluated {} matching homes against your criteria.",
                    ranked.len()
                )
            } else {
                let scrubbed = crate::shared::scrub_competitor_brands(&raw_overview);
                if scrubbed.trim().is_empty() {
                    format!(
                        "Evaluated {} matching homes against your criteria.",
                        ranked.len()
                    )
                } else {
                    scrubbed
                }
            };
            RankedCandidatesResult { overview, ranked }
        }
        Err(_) => {
            let ranked = pre_scored
                .iter()
                .map(|(row, det)| {
                    make_merged_json(
                        row,
                        det.match_score,
                        &det.match_tier,
                        &det.why_it_matches,
                        det.tradeoffs.as_deref(),
                    )
                })
                .collect();
            RankedCandidatesResult {
                overview: format!(
                    "Matched {} properties against your requirements.",
                    pre_scored.len()
                ),
                ranked,
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub zips: Vec<String>,
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    #[serde(default)]
    pub auto_sync: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            zips: Vec::new(),
            statuses: None,
            auto_sync: false,
        }
    }
}

pub async fn search_listings_with_llm(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    req: SearchRequest,
) -> Result<Value> {
    db.ensure_schema().await?;
    let t0 = now_unix_ms();

    // Seed pre-existing local reports if DB is empty and `./reports` exists
    if db.count_listings_total().await? == 0 {
        let default_reports = Path::new("reports");
        if default_reports.is_dir() {
            let _ = crate::search::sync::seed_reports_from_dir(db, default_reports).await;
        }
    }

    let allowed_statuses = ["for_sale", "pending", "sold", "off_market"];
    let explicit_statuses: Option<Vec<String>> = req.statuses.as_ref().map(|sts| {
        sts.iter()
            .filter(|s| allowed_statuses.contains(&s.as_str()))
            .cloned()
            .collect()
    });

    let parsed_res = parse_requirements_to_query(
        db,
        http,
        env,
        &req.query,
        &req.zips,
        explicit_statuses.as_deref(),
    )
    .await?;
    let filters = parsed_res.filters;
    let sanitized_query = parsed_res.sanitized_query;

    // Auto-sync missing ZIP codes across all selected ZIPs and resolved cities (no `.take(2)` cap)
    let mut synced_on_demand = Vec::new();
    if req.auto_sync && (!filters.zips.is_empty() || !filters.cities.is_empty()) {
        let default_state = if filters.states.len() == 1 {
            Some(filters.states[0].clone())
        } else {
            None
        };
        let city_pairs: Vec<(String, Option<String>)> = filters
            .cities
            .iter()
            .map(|c| (c.clone(), default_state.clone()))
            .collect();
        if let Ok(sum) = sync_areas_on_demand(
            db,
            http,
            env,
            &filters.zips,
            &city_pairs,
            false,
            4,
            8_000,
            None,
        )
        .await
        {
            for item in sum.synced {
                synced_on_demand.push(json!({
                    "zip": item.zip,
                    "upserted": item.upserted,
                }));
            }
        }
    }

    let mut fallback_mode = "exact";
    let (mut compiled, mut rows) =
        search_listings_narrow(db, &filters, false, None, 24).await?;

    // Relaxed numeric bounds fallback
    if rows.is_empty() {
        let (relaxed_q, r_rows) =
            search_listings_narrow(db, &filters, true, None, 24).await?;
        if !r_rows.is_empty() {
            fallback_mode = "relaxed_bounds";
            compiled = relaxed_q;
            rows = r_rows;
        }
    }

    // Recent sales fallback when only active was requested and area only has sold/off_market comps
    let only_active_requested = filters
        .statuses
        .iter()
        .all(|s| s == "for_sale" || s == "pending")
        && explicit_statuses
            .as_ref()
            .map(|v| v.is_empty())
            .unwrap_or(true);
    if rows.is_empty() && only_active_requested {
        let all_st = vec![
            "for_sale".to_string(),
            "pending".to_string(),
            "sold".to_string(),
            "off_market".to_string(),
        ];
        let (inc_sold_q, s_rows) =
            search_listings_narrow(db, &filters, true, Some(&all_st), 20).await?;
        if !s_rows.is_empty() {
            fallback_mode = "recent_sales_in_area";
            compiled = inc_sold_q;
            rows = s_rows;
        }
    }

    let ranked_res =
        rank_candidates_with_llm(db, http, env, &rows, &sanitized_query, &filters).await;

    Ok(json!({
        "ok": true,
        "elapsedMs": (now_unix_ms().saturating_sub(t0)) as u64,
        "dialect": db.dialect(),
        "driver": db.driver_name(),
        "model": parsed_res.model,
        "effort": parsed_res.effort,
        "fallbackUsed": parsed_res.fallback_used,
        "fallbackMode": fallback_mode,
        "syncedOnDemand": synced_on_demand,
        "query": {
            "raw": sanitized_query,
            "intentSummary": filters.intent_summary,
            "filters": filters,
            "sqlPreview": compiled.sql_preview,
        },
        "overview": ranked_res.overview,
        "matches": ranked_res.ranked,
    }))
}

// ---------------------------------------------------------------- Feedback & Weight Suggestions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackAction {
    Save,
    Dismiss,
    Tour,
    Hide,
}

impl FeedbackAction {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "save" => Some(Self::Save),
            "dismiss" => Some(Self::Dismiss),
            "tour" => Some(Self::Tour),
            "hide" => Some(Self::Hide),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Dismiss => "dismiss",
            Self::Tour => "tour",
            Self::Hide => "hide",
        }
    }

    pub fn is_positive(self) -> bool {
        matches!(self, Self::Save | Self::Tour)
    }

    pub fn is_negative(self) -> bool {
        matches!(self, Self::Dismiss | Self::Hide)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSample {
    pub listing_id: String,
    pub action: FeedbackAction,
    #[serde(default)]
    pub dimension_utilities: BTreeMap<Dimension, f64>,
    #[serde(default)]
    pub estimated_dimensions: Vec<Dimension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WeightSuggestion {
    pub dimension: Dimension,
    pub current_weight: f64,
    pub suggested_weight: f64,
    pub delta: f64,
    pub saved_avg_utility: f64,
    pub dismissed_avg_utility: f64,
    pub utility_gap: f64,
    pub saved_count: usize,
    pub dismissed_count: usize,
    pub direction: String,
    pub reason: String,
}

const FORBIDDEN_SNAPSHOT_KEYS: &[&str] = &[
    "race",
    "racepct",
    "ethnicity",
    "ancestry",
    "religion",
    "familialstatus",
    "protectedtrait",
    "nationalorigin",
    "demographics",
    "pctwhite",
    "pctblack",
    "pctasian",
    "pcthispanic",
];

fn is_forbidden_snapshot_key(k: &str) -> bool {
    let low = k.trim().to_ascii_lowercase();
    FORBIDDEN_SNAPSHOT_KEYS.iter().any(|f| low.contains(f))
}

/// Parses a `dimensionSnapshot` JSON payload in either flat `{dim: utility}` form,
/// `{dimensionUtilities: {...}, estimatedDimensions: [...]}` form, or `ScoredHome.dimensions`
/// array form. Rejects protected demographic keys or out-of-range utilities.
pub fn parse_dimension_snapshot(
    val: &Value,
) -> std::result::Result<(BTreeMap<Dimension, f64>, Vec<Dimension>), String> {
    let mut utils: BTreeMap<Dimension, f64> = BTreeMap::new();
    let mut estimated: Vec<Dimension> = Vec::new();

    if val.is_null() {
        return Ok((utils, estimated));
    }

    let parse_dim_array = |arr: &[Value],
                           utils: &mut BTreeMap<Dimension, f64>,
                           estimated: &mut Vec<Dimension>|
     -> std::result::Result<(), String> {
        for item in arr {
            let Some(obj) = item.as_object() else {
                return Err("dimension array entries must be objects".to_string());
            };
            for k in obj.keys() {
                if is_forbidden_snapshot_key(k) {
                    return Err(format!("Forbidden demographic field `{k}` in snapshot"));
                }
            }
            let dim_str = obj
                .get("dimension")
                .and_then(Value::as_str)
                .ok_or_else(|| "dimension entry missing `dimension` key".to_string())?;
            let dim = Dimension::from_key(dim_str.trim())
                .ok_or_else(|| format!("Unknown scoring dimension `{dim_str}`"))?;
            let u = obj
                .get("utility")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
                .ok_or_else(|| format!("Utility for `{dim_str}` must be between 0 and 100"))?;
            utils.insert(dim, u);
            if obj.get("estimated").and_then(Value::as_bool) == Some(true)
                && !estimated.contains(&dim)
            {
                estimated.push(dim);
            }
        }
        Ok(())
    };

    match val {
        Value::Array(arr) => {
            parse_dim_array(arr, &mut utils, &mut estimated)?;
        }
        Value::Object(map) => {
            for k in map.keys() {
                if is_forbidden_snapshot_key(k) {
                    return Err(format!("Forbidden demographic field `{k}` in snapshot"));
                }
            }
            if let Some(arr) = map.get("dimensions").and_then(Value::as_array) {
                parse_dim_array(arr, &mut utils, &mut estimated)?;
            } else if let Some(uobj) = map
                .get("dimensionUtilities")
                .or_else(|| map.get("dimension_utilities"))
                .and_then(Value::as_object)
            {
                for (k, v) in uobj {
                    if is_forbidden_snapshot_key(k) {
                        return Err(format!("Forbidden demographic field `{k}` in snapshot"));
                    }
                    let dim = Dimension::from_key(k.trim())
                        .ok_or_else(|| format!("Unknown scoring dimension `{k}`"))?;
                    let u = v
                        .as_f64()
                        .filter(|x| x.is_finite() && (0.0..=100.0).contains(x))
                        .ok_or_else(|| format!("Utility for `{k}` must be between 0 and 100"))?;
                    utils.insert(dim, u);
                }
                if let Some(est_arr) = map
                    .get("estimatedDimensions")
                    .or_else(|| map.get("estimated_dimensions"))
                    .and_then(Value::as_array)
                {
                    for item in est_arr {
                        if let Some(s) = item.as_str() {
                            if let Some(d) = Dimension::from_key(s.trim()) {
                                if !estimated.contains(&d) {
                                    estimated.push(d);
                                }
                            }
                        }
                    }
                }
            } else {
                for (k, v) in map {
                    let dim = Dimension::from_key(k.trim())
                        .ok_or_else(|| format!("Unknown scoring dimension `{k}`"))?;
                    let u = v
                        .as_f64()
                        .filter(|x| x.is_finite() && (0.0..=100.0).contains(x))
                        .ok_or_else(|| format!("Utility for `{k}` must be between 0 and 100"))?;
                    utils.insert(dim, u);
                }
            }
        }
        _ => return Err("dimensionSnapshot must be a JSON object or array".to_string()),
    }

    Ok((utils, estimated))
}

/// Deterministic preference learning: compares saved/toured homes against dismissed/hidden homes
/// across the 11 objective scoring dimensions and proposes bounded weight adjustments without
/// mutating the buyer's profile automatically.
pub fn suggest_weight_adjustments(
    profile: &Profile,
    feedback_items: &[FeedbackSample],
) -> Vec<WeightSuggestion> {
    const MIN_UTILITY_GAP: f64 = 8.0;

    let saved: Vec<&FeedbackSample> = feedback_items
        .iter()
        .filter(|f| f.action.is_positive())
        .collect();
    let dismissed: Vec<&FeedbackSample> = feedback_items
        .iter()
        .filter(|f| f.action.is_negative())
        .collect();
    if saved.is_empty() || dismissed.is_empty() {
        return Vec::new();
    }

    let total_positive_weights = Dimension::ALL
        .iter()
        .filter(|&&d| profile.weight(d) > 0.0)
        .count();

    let mut out = Vec::new();
    for d in Dimension::ALL {
        let mut s_vals = Vec::new();
        for item in &saved {
            if item.estimated_dimensions.contains(&d) {
                continue;
            }
            if let Some(&u) = item.dimension_utilities.get(&d) {
                if u.is_finite() && (0.0..=100.0).contains(&u) {
                    s_vals.push(u);
                }
            }
        }
        let mut d_vals = Vec::new();
        for item in &dismissed {
            if item.estimated_dimensions.contains(&d) {
                continue;
            }
            if let Some(&u) = item.dimension_utilities.get(&d) {
                if u.is_finite() && (0.0..=100.0).contains(&u) {
                    d_vals.push(u);
                }
            }
        }
        if s_vals.is_empty() || d_vals.is_empty() {
            continue;
        }

        let s_avg = s_vals.iter().sum::<f64>() / (s_vals.len() as f64);
        let d_avg = d_vals.iter().sum::<f64>() / (d_vals.len() as f64);
        let gap = s_avg - d_avg;
        if gap.abs() < MIN_UTILITY_GAP {
            continue;
        }

        let current_weight = profile.weight(d);
        let step = (gap.abs() / 5.0).round().clamp(2.0, 12.0);
        let (direction, suggested_weight) = if gap >= MIN_UTILITY_GAP {
            (
                "increase".to_string(),
                (current_weight + step).min(MAX_WEIGHT),
            )
        } else {
            if current_weight <= 0.0 {
                continue;
            }
            let floor = if total_positive_weights <= 1 {
                1.0
            } else {
                0.0
            };
            ("decrease".to_string(), (current_weight - step).max(floor))
        };

        let delta = round_n(suggested_weight - current_weight, 1);
        if delta.abs() < 1.0 {
            continue;
        }

        let label = d.label().en;
        let reason = if direction == "increase" {
            format!(
                "Homes you saved or toured averaged {:.0}/100 on {label} vs {:.0}/100 for dismissed homes (+{:.0} pts).",
                s_avg, d_avg, gap
            )
        } else {
            format!(
                "Homes you saved or toured averaged {:.0}/100 on {label} vs {:.0}/100 for dismissed homes ({:.0} pts), suggesting you readily trade off {label}.",
                s_avg, d_avg, gap
            )
        };

        out.push(WeightSuggestion {
            dimension: d,
            current_weight: round_n(current_weight, 1),
            suggested_weight: round_n(suggested_weight, 1),
            delta,
            saved_avg_utility: round_n(s_avg, 1),
            dismissed_avg_utility: round_n(d_avg, 1),
            utility_gap: round_n(gap, 1),
            saved_count: s_vals.len(),
            dismissed_count: d_vals.len(),
            direction,
            reason,
        });
    }

    out.sort_by(|a, b| {
        b.utility_gap
            .abs()
            .total_cmp(&a.utility_gap.abs())
            .then_with(|| a.dimension.cmp(&b.dimension))
    });
    out.truncate(5);
    out
}

// ---------------------------------------------------------------- Fair Housing Audit
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PresetAuditResult {
    pub preset_key: String,
    pub language_invariant: bool,
    pub demographic_poison_invariant: bool,
    pub max_score_delta: f64,
    pub ranked_ids: Vec<String>,
    pub mismatch_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FairHousingAuditReport {
    pub passed: bool,
    pub presets_tested: usize,
    pub languages_tested: Vec<String>,
    pub homes_tested: usize,
    pub language_invariant: bool,
    pub demographic_poison_invariant: bool,
    pub poisoned_fields_checked: Vec<String>,
    pub max_score_delta: f64,
    pub preset_results: Vec<PresetAuditResult>,
    pub failures: Vec<String>,
}

const AUDIT_POISON_FIELDS: &[&str] = &[
    "racePct",
    "ancestry",
    "religion",
    "familialStatus",
    "protectedTrait",
];

/// Inert key-ignore invariance probe for Fair Housing Act (42 U.S.C. § 3604) verification:
/// attaches opaque non-demographic sentinel tokens under protected-attribute key names to verify
/// `HomeFactors::from_report` never reads or scores those keys.
#[doc(hidden)]
pub fn poison_report_with_demographics(report: &Value, variant: usize) -> Value {
    let mut probed = report.clone();
    let sentinel = json!(format!("__fha_ignored_key_probe_{variant}"));
    let mut probe_map = serde_json::Map::new();
    for &k in AUDIT_POISON_FIELDS {
        probe_map.insert(k.to_string(), sentinel.clone());
    }
    let probe_obj = Value::Object(probe_map);

    if let Some(root) = probed.as_object_mut() {
        for &k in AUDIT_POISON_FIELDS {
            root.insert(k.to_string(), sentinel.clone());
        }
        root.insert("demographics".to_string(), probe_obj.clone());
    }
    for pointer in ["/facts", "/geo", "/summary", "/modules/civic/data"] {
        if let Some(obj) = probed.pointer_mut(pointer).and_then(Value::as_object_mut) {
            for &k in AUDIT_POISON_FIELDS {
                obj.insert(k.to_string(), sentinel.clone());
            }
            obj.insert("demographics".to_string(), probe_obj.clone());
        }
    }
    probed
}

fn rank_signature(r: &RankResult) -> Vec<(String, usize, bool, i64, String)> {
    r.ranked
        .iter()
        .chain(r.near_misses.iter())
        .chain(r.flagged.iter())
        .map(|h| {
            (
                h.id.clone(),
                h.rank,
                h.tied,
                (h.score * 1_000_000.0).round() as i64,
                h.band.key().to_string(),
            )
        })
        .collect()
}

fn max_rank_score_delta(a: &RankResult, b: &RankResult) -> f64 {
    let mut max_d = 0.0_f64;
    for ha in a.all() {
        if let Some(hb) = b.find(&ha.id) {
            max_d = max_d.max((ha.score - hb.score).abs());
        } else {
            max_d = max_d.max(100.0);
        }
    }
    max_d
}

fn build_audit_profile_for_preset(
    preset_idx: usize,
    homes: &[HomeFactors],
    commute: &CommuteMatrix,
) -> Profile {
    let mut profile = PRESETS[preset_idx].to_profile();
    let max_places = commute
        .homes
        .values()
        .map(|v| v.len())
        .max()
        .unwrap_or(0)
        .min(crate::scoring::profile::MAX_PLACES);
    for i in 0..max_places {
        profile.places.push(Place {
            label: format!("Audit Place {}", i + 1),
            lat: 37.3349,
            lon: -122.0090,
            max_minutes: 30.0,
            trips_per_week: 5.0,
            mode: TravelMode::Drive,
        });
    }
    let mut prices: Vec<f64> = homes
        .iter()
        .filter_map(|h| h.cost.price_for_budget())
        .filter(|p| p.is_finite() && *p > 0.0)
        .collect();
    if !prices.is_empty() {
        prices.sort_unstable_by(f64::total_cmp);
        profile.budget.max_price = Some(prices[prices.len() / 2]);
    }
    profile
}

fn run_fair_housing_audit_inner(
    homes: &[HomeFactors],
    report_pairs: Option<(&[Value], &[Value])>,
    commute: &CommuteMatrix,
) -> FairHousingAuditReport {
    let languages = vec![
        Lang::En.code().to_string(),
        Lang::ZhCn.code().to_string(),
        Lang::Es.code().to_string(),
    ];
    if homes.is_empty()
        || report_pairs
            .map(|(cr, pr)| cr.len() != homes.len() || pr.len() != homes.len())
            .unwrap_or(false)
    {
        return FairHousingAuditReport {
            passed: false,
            presets_tested: PRESETS.len(),
            languages_tested: languages,
            homes_tested: homes.len(),
            language_invariant: false,
            demographic_poison_invariant: false,
            poisoned_fields_checked: AUDIT_POISON_FIELDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            max_score_delta: 0.0,
            preset_results: Vec::new(),
            failures: vec!["Fair housing audit requires at least one HomeFactors item.".to_string()],
        };
    }

    let extracted_pairs: Option<(Vec<HomeFactors>, Vec<HomeFactors>)> =
        report_pairs.map(|(clean_reports, poisoned_reports)| {
            (
                clean_reports.iter().map(HomeFactors::from_report).collect(),
                poisoned_reports
                    .iter()
                    .map(HomeFactors::from_report)
                    .collect(),
            )
        });

    let mut global_failures: Vec<String> = Vec::new();
    if let Some((ref clean_extracted, ref poisoned_extracted)) = extracted_pairs {
        for (idx, (ch, ph)) in clean_extracted
            .iter()
            .zip(poisoned_extracted.iter())
            .enumerate()
        {
            if ch != ph {
                global_failures.push(format!(
                    "HomeFactors::from_report differed on home index {idx} (`{}`) after protected-key probe",
                    ch.id
                ));
            }
        }
    }

    let mut preset_results = Vec::with_capacity(PRESETS.len());
    let mut all_lang_ok = true;
    let mut all_poison_ok = global_failures.is_empty();
    let mut global_max_delta = 0.0_f64;

    for (p_idx, preset) in PRESETS.iter().enumerate() {
        let profile = build_audit_profile_for_preset(p_idx, homes, commute);
        let mut mismatch_reasons = Vec::new();

        // (a) Language invariance across en, zh-CN, es
        let mut r_en = rank_with(&profile, homes, commute, &RankOptions::default());
        r_en.localize(Lang::En);
        let sig_en = rank_signature(&r_en);

        let mut lang_ok = true;
        for lang in [Lang::ZhCn, Lang::Es] {
            let mut r_lang = rank_with(&profile, homes, commute, &RankOptions::default());
            r_lang.localize(lang);
            let delta = max_rank_score_delta(&r_en, &r_lang);
            global_max_delta = global_max_delta.max(delta);
            if rank_signature(&r_lang) != sig_en || delta > 1e-9 {
                lang_ok = false;
                mismatch_reasons.push(format!(
                    "Language `{}` changed ranking/scores vs `en` (max_delta={delta})",
                    lang.code()
                ));
            }
        }

        // (b) Protected-key invariance when real report JSONs are supplied
        let (poison_ok, poison_delta) = if let Some((ref clean_extracted, ref poisoned_extracted)) =
            extracted_pairs
        {
            let r_clean = rank_with(&profile, clean_extracted, commute, &RankOptions::default());
            let r_poison = rank_with(
                &profile,
                poisoned_extracted,
                commute,
                &RankOptions::default(),
            );
            let pd = max_rank_score_delta(&r_clean, &r_poison);
            global_max_delta = global_max_delta.max(pd);
            let ok = global_failures.is_empty()
                && rank_signature(&r_clean) == rank_signature(&r_poison)
                && pd <= 1e-9;
            if rank_signature(&r_clean) != rank_signature(&r_poison) || pd > 1e-9 {
                mismatch_reasons.push(format!(
                    "Protected-key probe fields altered ranking/scores (max_delta={pd})"
                ));
            }
            (ok, pd)
        } else {
            (true, 0.0)
        };

        if !lang_ok {
            all_lang_ok = false;
        }
        if !poison_ok {
            all_poison_ok = false;
        }
        for m in &mismatch_reasons {
            global_failures.push(format!("Preset `{}`: {m}", preset.key));
        }

        preset_results.push(PresetAuditResult {
            preset_key: preset.key.to_string(),
            language_invariant: lang_ok,
            demographic_poison_invariant: poison_ok,
            max_score_delta: poison_delta,
            ranked_ids: r_en.ranked.iter().map(|h| h.id.clone()).collect(),
            mismatch_reasons,
        });
    }

    let passed =
        all_lang_ok && all_poison_ok && global_failures.is_empty() && preset_results.len() == 10;
    FairHousingAuditReport {
        passed,
        presets_tested: preset_results.len(),
        languages_tested: languages,
        homes_tested: homes.len(),
        language_invariant: all_lang_ok,
        demographic_poison_invariant: all_poison_ok,
        poisoned_fields_checked: AUDIT_POISON_FIELDS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        max_score_delta: global_max_delta,
        preset_results,
        failures: global_failures,
    }
}

/// Fair Housing audit over `HomeFactors`: tests all 10 presets in `en`, `zh-CN`, and `es`
/// directly on verified `HomeFactors` without fabricating synthetic report JSONs.
pub fn fair_housing_audit(
    homes: &[HomeFactors],
    commute: &CommuteMatrix,
) -> FairHousingAuditReport {
    run_fair_housing_audit_inner(homes, None, commute)
}

/// Fair Housing audit directly on real Open Estate report JSON documents.
pub fn fair_housing_audit_reports(
    reports: &[Value],
    commute: &CommuteMatrix,
) -> FairHousingAuditReport {
    let homes: Vec<HomeFactors> = reports.iter().map(HomeFactors::from_report).collect();
    let probed_reports: Vec<Value> = reports
        .iter()
        .enumerate()
        .map(|(idx, r)| poison_report_with_demographics(r, idx))
        .collect();
    run_fair_housing_audit_inner(&homes, Some((reports, &probed_reports)), commute)
}
