// Background ZIP-code listing & sold-home synchronizer, Open Estate report/comp harvester,
// `csv`-crate MLS/Redfin CSV parser, school-grade enricher, and diff event tracker.

use crate::db::{
    format_listing_row, listing_slug, normalize_listing_status, normalize_property_category,
    score_to_school_grade, DbStore, ListingRow, SqlParam,
};
use crate::shared::{haversine_m, now_iso, round_n, EnvConfig, HttpClient, MILE_M};
use anyhow::{anyhow, Result};
use base64::Engine as _;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;
use url::Url;
use uuid::Uuid;

const ATTOM_BASE: &str = "https://api.gateway.attomdata.com/propertyapi/v1.0.0";
const SIMPLYRETS_BASE: &str = "https://api.simplyrets.com";
const TIGER_ZCTA_URL: &str =
    "https://tigerweb.geo.census.gov/arcgis/rest/services/TIGERweb/PUMA_TAD_TAZ_UGA_ZCTA/MapServer/4";
const NCES_BASE: &str = "https://nces.ed.gov/opengis/rest/services/K12_School_Locations";
const PUBLIC_YEARS: &[&str] = &["2425", "2324", "2223"];
const SCHOOL_CACHE_TTL_SECS: i64 = 14 * 24 * 3600;

static ZIP5_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{5})\b").unwrap());
static HDR_CELL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(address|street|property\s*type|price|sale\s*type)$").unwrap());
static DISCLAIMER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^in accordance with").unwrap());
static PAST_SOLD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)past|sold|closed").unwrap());
static ELEM_GRADE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)K.?5|K.?6").unwrap());
static MID_GRADE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)6.?8|7.?8").unwrap());
static HIGH_GRADE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)9.?12").unwrap());
static ACTIVE_SUBJECT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)active|for\s*sale").unwrap());
static PENDING_SUBJECT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)pending|contract").unwrap());
static ONE_STORY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)1-story|single-story").unwrap());
static TWO_STORY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)2-story|two-story").unwrap());

pub fn sync_connector_status(env: &EnvConfig) -> Value {
    json!([
        {
            "id": "reso_zip",
            "name": "RESO Web API 2.0 ZIP IDX",
            "active": env.get("RESO_API_URL").is_some() && env.get("RESO_BEARER_TOKEN").is_some(),
        },
        {
            "id": "simplyrets_zip",
            "name": "SimplyRETS ZIP IDX",
            "active": env.get("SIMPLYRETS_AUTH").is_some(),
        },
        {
            "id": "attom_zip",
            "name": "ATTOM ZIP Recorded Sales",
            "active": env.get("ATTOM_API_KEY").is_some(),
        },
        {
            "id": "public_feed",
            "name": "Public Feed / Open Estate Report & Comp Harvester",
            "active": true,
        }
    ])
}

pub(crate) fn num_from_val(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => num_from_str(s),
        _ => None,
    }
}

pub(crate) fn int_from_val(v: Option<&Value>) -> Option<i64> {
    num_from_val(v).map(|x| x.round() as i64)
}

pub(crate) fn str_from_val(v: Option<&Value>) -> Option<String> {
    let v = v?;
    match v {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn num_from_str(raw: &str) -> Option<f64> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !matches!(c, '$' | ',' | '%' | ' ' | '\t'))
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse::<f64>().ok().filter(|n| n.is_finite())
}

pub(crate) fn int_from_str(raw: &str) -> Option<i64> {
    num_from_str(raw).map(|n| n.round() as i64)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingInput {
    pub id: Option<String>,
    pub address: String,
    pub street: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub status: Option<String>,
    pub price: Option<i64>,
    pub list_price: Option<i64>,
    pub sold_price: Option<i64>,
    pub estimated_value: Option<i64>,
    pub beds: Option<f64>,
    pub baths: Option<f64>,
    pub sqft: Option<i64>,
    pub lot_sqft: Option<i64>,
    pub year_built: Option<i64>,
    pub stories: Option<f64>,
    pub property_type: Option<String>,
    pub hoa_monthly: Option<i64>,
    pub days_on_market: Option<i64>,
    pub listed_date: Option<String>,
    pub sold_date: Option<String>,
    pub school_score: Option<f64>,
    pub school_grade: Option<String>,
    pub school_elem_name: Option<String>,
    pub school_elem_rating: Option<f64>,
    pub school_mid_name: Option<String>,
    pub school_mid_rating: Option<f64>,
    pub school_high_name: Option<String>,
    pub school_high_rating: Option<f64>,
    pub school_district: Option<String>,
    pub walk_score: Option<f64>,
    pub flood_zone: Option<String>,
    pub wildfire_zone: Option<String>,
    pub overall_score: Option<f64>,
    pub verdict: Option<String>,
    pub mls_number: Option<String>,
    pub source: Option<String>,
    pub url: Option<String>,
    pub photo_url: Option<String>,
    pub report_slug: Option<String>,
    pub features_text: Option<String>,
    pub safety_score: Option<f64>,
    pub has_hardwood_floors: Option<i64>,
    pub has_updated_kitchen: Option<i64>,
    pub has_remodeled: Option<i64>,
    pub quiet_street: Option<i64>,
    pub noise_max_db: Option<f64>,
    pub seismic_pga: Option<f64>,
    pub tags_json: Option<String>,
    // [feat/report-ux-likes] Optional raw payload snippet preserving real portal engagement signals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<String>,
}

impl ListingInput {
    pub fn from_json(v: &Value, fallback_zip: &str) -> Option<Self> {
        let obj = v.as_object()?;
        let get = |k1: &str, k2: &str| obj.get(k1).or_else(|| obj.get(k2));

        let street = str_from_val(get("street", "addressLine1"));
        let address =
            str_from_val(get("address", "formattedAddress")).or_else(|| street.clone())?;
        let zip_raw = str_from_val(get("zip", "zip_code"))
            .or_else(|| str_from_val(obj.get("postalCode")))
            .unwrap_or_else(|| fallback_zip.to_string());
        let zip = ZIP5_RE
            .captures(&zip_raw)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| fallback_zip.chars().take(5).collect());
        if zip.len() != 5 || !zip.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }

        Some(Self {
            id: str_from_val(obj.get("id")),
            address,
            street,
            city: str_from_val(obj.get("city")),
            state: str_from_val(obj.get("state")),
            zip,
            lat: num_from_val(get("lat", "latitude")),
            lon: num_from_val(get("lon", "longitude")),
            status: str_from_val(obj.get("status")),
            price: int_from_val(obj.get("price")),
            list_price: int_from_val(get("listPrice", "list_price")),
            sold_price: int_from_val(get("soldPrice", "sold_price")),
            estimated_value: int_from_val(get("estimatedValue", "estimated_value")),
            beds: num_from_val(get("beds", "bedrooms")),
            baths: num_from_val(get("baths", "bathrooms")),
            sqft: int_from_val(get("sqft", "squareFootage")),
            lot_sqft: int_from_val(get("lotSqft", "lot_sqft")),
            year_built: int_from_val(get("yearBuilt", "year_built")),
            stories: num_from_val(obj.get("stories")),
            property_type: str_from_val(get("propertyType", "property_type")),
            hoa_monthly: int_from_val(get("hoaMonthly", "hoa_monthly")),
            days_on_market: int_from_val(get("daysOnMarket", "days_on_market")),
            listed_date: str_from_val(get("listedDate", "listed_date")),
            sold_date: str_from_val(get("soldDate", "sold_date")),
            school_score: num_from_val(get("schoolScore", "school_score")),
            school_grade: str_from_val(get("schoolGrade", "school_grade")),
            school_elem_name: str_from_val(get("schoolElemName", "school_elem_name")),
            school_elem_rating: num_from_val(get("schoolElemRating", "school_elem_rating")),
            school_mid_name: str_from_val(get("schoolMidName", "school_mid_name")),
            school_mid_rating: num_from_val(get("schoolMidRating", "school_mid_rating")),
            school_high_name: str_from_val(get("schoolHighName", "school_high_name")),
            school_high_rating: num_from_val(get("schoolHighRating", "school_high_rating")),
            school_district: str_from_val(get("schoolDistrict", "school_district")),
            walk_score: num_from_val(get("walkScore", "walk_score")),
            flood_zone: str_from_val(get("floodZone", "flood_zone")),
            wildfire_zone: str_from_val(get("wildfireZone", "wildfire_zone")),
            overall_score: num_from_val(get("overallScore", "overall_score")),
            verdict: str_from_val(obj.get("verdict")),
            mls_number: str_from_val(get("mlsNumber", "mls_number")),
            source: str_from_val(obj.get("source")),
            url: str_from_val(obj.get("url")),
            photo_url: str_from_val(get("photoUrl", "photo_url")),
            report_slug: str_from_val(get("reportSlug", "report_slug")),
            features_text: str_from_val(get("featuresText", "features_text")),
            safety_score: num_from_val(get("safetyScore", "safety_score")),
            has_hardwood_floors: bool_as_flag(get("hasHardwoodFloors", "has_hardwood_floors")),
            has_updated_kitchen: bool_as_flag(get("hasUpdatedKitchen", "has_updated_kitchen")),
            has_remodeled: bool_as_flag(get("hasRemodeled", "has_remodeled")),
            quiet_street: bool_as_flag(get("quietStreet", "quiet_street")),
            noise_max_db: num_from_val(get("noiseMaxDb", "noise_max_db")),
            seismic_pga: num_from_val(get("seismicPga", "seismic_pga")),
            tags_json: str_from_val(get("tagsJson", "tags_json")).or_else(|| {
                obj.get("tags")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| serde_json::to_string(arr).ok())
            }),
            raw_json: merge_explicit_shopping_json(
                crate::modules::engagement::extract_raw_engagement_json(v),
                v,
            ),
        })
    }

    fn merge_non_empty(&mut self, other: &Self) {
        macro_rules! overwrite_if_some {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field = other.$field.clone();
                }
            };
        }
        if !other.address.trim().is_empty() {
            self.address = other.address.clone();
        }
        if !other.zip.trim().is_empty() {
            self.zip = other.zip.clone();
        }
        overwrite_if_some!(id);
        overwrite_if_some!(street);
        overwrite_if_some!(city);
        overwrite_if_some!(state);
        overwrite_if_some!(lat);
        overwrite_if_some!(lon);
        overwrite_if_some!(status);
        overwrite_if_some!(price);
        overwrite_if_some!(list_price);
        overwrite_if_some!(sold_price);
        overwrite_if_some!(estimated_value);
        overwrite_if_some!(beds);
        overwrite_if_some!(baths);
        overwrite_if_some!(sqft);
        overwrite_if_some!(lot_sqft);
        overwrite_if_some!(year_built);
        overwrite_if_some!(stories);
        overwrite_if_some!(property_type);
        overwrite_if_some!(hoa_monthly);
        overwrite_if_some!(days_on_market);
        overwrite_if_some!(listed_date);
        overwrite_if_some!(sold_date);
        overwrite_if_some!(school_score);
        overwrite_if_some!(school_grade);
        overwrite_if_some!(school_elem_name);
        overwrite_if_some!(school_elem_rating);
        overwrite_if_some!(school_mid_name);
        overwrite_if_some!(school_mid_rating);
        overwrite_if_some!(school_high_name);
        overwrite_if_some!(school_high_rating);
        overwrite_if_some!(school_district);
        overwrite_if_some!(walk_score);
        overwrite_if_some!(flood_zone);
        overwrite_if_some!(wildfire_zone);
        overwrite_if_some!(overall_score);
        overwrite_if_some!(verdict);
        overwrite_if_some!(mls_number);
        overwrite_if_some!(source);
        overwrite_if_some!(url);
        overwrite_if_some!(photo_url);
        overwrite_if_some!(report_slug);
        overwrite_if_some!(safety_score);
        overwrite_if_some!(has_hardwood_floors);
        overwrite_if_some!(has_updated_kitchen);
        overwrite_if_some!(has_remodeled);
        overwrite_if_some!(quiet_street);
        overwrite_if_some!(noise_max_db);
        overwrite_if_some!(seismic_pga);
        overwrite_if_some!(tags_json);
        if other
            .features_text
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        {
            self.features_text = other.features_text.clone();
        }
        if let Some(ref other_raw) = other.raw_json {
            self.raw_json = Some(merge_two_raw_json_blobs(
                self.raw_json.as_deref(),
                other_raw,
            ));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingChangeEvent {
    pub event_type: String,
    pub old_status: Option<String>,
    pub old_price: Option<i64>,
    pub listing: ListingRow,
    pub listing_json: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertBatchResult {
    pub upserted: usize,
    pub new_listings: Vec<Value>,
    pub sold_listings: Vec<Value>,
    pub price_changes: Vec<Value>,
    pub changed_listings: Vec<ListingChangeEvent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZipSyncResult {
    pub zip: String,
    pub upserted: usize,
    pub active_count: i64,
    pub sold_count: i64,
    pub new_listings: Vec<Value>,
    pub sold_listings: Vec<Value>,
    pub price_changes: Vec<Value>,
    pub changed_listings: Vec<ListingChangeEvent>,
    pub sources_used: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------- CSV Parser (`csv` crate)
pub fn parse_listing_csv(csv_text: &str, fallback_zip: &str) -> Vec<ListingInput> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(csv_text.as_bytes());

    let mut matrix: Vec<Vec<String>> = Vec::new();
    for rec in rdr.records().flatten() {
        let cells: Vec<String> = rec.iter().map(|s| s.trim().to_string()).collect();
        if cells.iter().any(|c| !c.is_empty()) {
            matrix.push(cells);
        }
    }
    if matrix.len() < 2 {
        return Vec::new();
    }

    let hdr_idx = matrix
        .iter()
        .position(|r| r.iter().any(|c| HDR_CELL_RE.is_match(c.trim())))
        .unwrap_or(0);
    let headers: Vec<String> = matrix[hdr_idx]
        .iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();

    let col = |names: &[&str]| -> Option<usize> {
        for n in names {
            if let Some(i) = headers.iter().position(|h| h == n || h.contains(n)) {
                return Some(i);
            }
        }
        None
    };

    let Some(i_addr) = col(&["address", "street"]) else {
        return Vec::new();
    };
    let i_city = col(&["city"]);
    let i_state = col(&["state or province", "state"]);
    let i_zip = col(&["zip or postal code", "zip", "postal"]);
    let i_price = col(&["price", "list price", "sale price"]);
    let i_beds = col(&["beds", "bedrooms"]);
    let i_baths = col(&["baths", "bathrooms"]);
    let i_sqft = col(&["square feet", "sqft", "living area"]);
    let i_lot = col(&["lot size", "lot sqft"]);
    let i_year = col(&["year built"]);
    let i_dom = col(&["days on market", "dom"]);
    let i_hoa = col(&["hoa/month", "hoa"]);
    let i_status = col(&["status"]);
    let i_sale_type = col(&["sale type"]);
    let i_sold_date = col(&["sold date", "close date"]);
    let i_type = col(&["property type"]);
    let i_url = col(&["url (see", "url", "link"]);
    let i_mls = col(&["mls#", "mls"]);
    let i_lat = col(&["latitude", "lat"]);
    let i_lon = col(&["longitude", "lon", "lng"]);
    // [feat/report-ux-likes] Optional real portal engagement columns in CSV imports
    let i_favs = col(&["favorite count", "favorites", "favorite", "saves", "likes"]);
    let i_views = col(&["page view count", "page views", "views count", "views"]);
    let i_hot = col(&["hot home", "hothome", "hot_home"]);
    let i_src = col(&["source", "portal"]);

    fn cell<'a>(row: &'a [String], idx: Option<usize>) -> Option<&'a str> {
        idx.and_then(|i| row.get(i))
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    fn canonical_street_suffix(tok: &str) -> Option<&'static str> {
        match tok.to_ascii_lowercase().as_str() {
            "ave" | "av" | "avenue" => Some("Ave"),
            "st" | "street" => Some("St"),
            "dr" | "drive" => Some("Dr"),
            "way" | "wy" => Some("Way"),
            "ct" | "court" => Some("Ct"),
            "ln" | "lane" => Some("Ln"),
            "blvd" | "boulevard" => Some("Blvd"),
            "pl" | "place" => Some("Pl"),
            "cir" | "circle" => Some("Cir"),
            "rd" | "road" => Some("Rd"),
            "ter" | "terrace" => Some("Ter"),
            "pkwy" | "parkway" => Some("Pkwy"),
            "loop" => Some("Loop"),
            "trl" | "trail" => Some("Trl"),
            "hwy" | "highway" => Some("Hwy"),
            "sq" | "square" => Some("Sq"),
            "walk" => Some("Walk"),
            "run" => Some("Run"),
            "aly" | "alley" => Some("Aly"),
            _ => None,
        }
    }

    fn recover_street_from_portal_url(street: &str, zip: &str, url: Option<&str>) -> String {
        let trimmed = street.trim();
        let (base_part, unit_tail) = match trimmed.find('#') {
            Some(pos) => (trimmed[..pos].trim_end(), Some(trimmed[pos..].trim())),
            None => (trimmed, None),
        };
        let street_tokens: Vec<&str> = base_part
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
            .filter(|t| !t.is_empty())
            .collect();
        if street_tokens.len() < 2
            || street_tokens
                .iter()
                .skip(1)
                .any(|t| canonical_street_suffix(t).is_some())
        {
            return trimmed.to_string();
        }
        let Some(u) = url else {
            return trimmed.to_string();
        };
        let zip_suffix = format!("-{zip}");
        let Some(seg) = u
            .split('/')
            .find(|s| s.ends_with(&zip_suffix) && s.len() > zip_suffix.len())
        else {
            return trimmed.to_string();
        };
        let slug_body = &seg[..seg.len() - zip_suffix.len()];
        let slug_tokens: Vec<&str> = slug_body
            .split('-')
            .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
            .filter(|t| !t.is_empty())
            .collect();
        if slug_tokens.len() <= street_tokens.len() {
            return trimmed.to_string();
        }
        let prefix_matches = street_tokens
            .iter()
            .zip(slug_tokens.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b));
        if !prefix_matches {
            return trimmed.to_string();
        }
        if let Some(sfx) = canonical_street_suffix(slug_tokens[street_tokens.len()]) {
            return match unit_tail {
                Some(unit) if !unit.is_empty() => format!("{base_part} {sfx} {unit}"),
                _ => format!("{base_part} {sfx}"),
            };
        }
        trimmed.to_string()
    }

    let mut out = Vec::new();
    for r in matrix.iter().skip(hdr_idx + 1) {
        let Some(raw_street) = cell(r, Some(i_addr)) else {
            continue;
        };
        if DISCLAIMER_RE.is_match(raw_street) {
            continue;
        }
        let city = cell(r, i_city).map(ToOwned::to_owned);
        let state = cell(r, i_state).map(ToOwned::to_owned);
        let zip_raw = cell(r, i_zip).unwrap_or(fallback_zip);
        let zip = ZIP5_RE
            .captures(zip_raw)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| fallback_zip.trim().chars().take(5).collect());
        if zip.len() != 5 || !zip.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let csv_url = cell(r, i_url).map(ToOwned::to_owned);
        let street_buf = recover_street_from_portal_url(raw_street, &zip, csv_url.as_deref());
        let street = street_buf.as_str();

        let raw_status = cell(r, i_status).unwrap_or("");
        let sale_type = cell(r, i_sale_type).unwrap_or("");
        let sold_date_raw = cell(r, i_sold_date);
        let sold_date = sold_date_raw
            .and_then(crate::property::mls::parse_portal_date_iso)
            .or_else(|| sold_date_raw.map(ToOwned::to_owned));
        let status = if PAST_SOLD_RE.is_match(sale_type) || PAST_SOLD_RE.is_match(raw_status) {
            if sold_date.is_some() {
                "sold"
            } else {
                "off_market"
            }
        } else if !raw_status.is_empty() {
            normalize_listing_status(Some(raw_status))
        } else if sale_type.eq_ignore_ascii_case("other") {
            "off_market"
        } else if !sale_type.is_empty() {
            normalize_listing_status(Some(sale_type))
        } else if sold_date.is_some() {
            "sold"
        } else {
            "for_sale"
        };
        let price = cell(r, i_price).and_then(int_from_str);

        let state_zip = [state.as_deref(), Some(zip.as_str())]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let full_address = [Some(street), city.as_deref(), Some(state_zip.as_str())]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");

        let csv_src = cell(r, i_src)
            .map(crate::shared::scrub_competitor_brands)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                csv_url.as_deref().and_then(|u| {
                    crate::modules::engagement::detect_platform_id(None, Some(u)).map(|pid| {
                        match pid {
                            "zillow" | "redfin" | "realtor" | "homes" => {
                                "Public Listing".to_string()
                            }
                            _ => "CSV Listing Import".to_string(),
                        }
                    })
                })
            })
            .unwrap_or_else(|| "CSV Listing Import".to_string());
        // [feat/report-ux-likes]
        let csv_raw_json = {
            let favs = cell(r, i_favs).and_then(int_from_str).filter(|&n| n >= 0);
            let views = cell(r, i_views).and_then(int_from_str).filter(|&n| n >= 0);
            let hot = cell(r, i_hot).and_then(|s| {
                let low = s.to_ascii_lowercase();
                if matches!(low.as_str(), "true" | "yes" | "1" | "y" | "hot home") {
                    Some(true)
                } else {
                    None
                }
            });
            if favs.is_some() || views.is_some() || hot.is_some() {
                let mut m = serde_json::Map::new();
                if let Some(f) = favs {
                    m.insert("favoriteCount".into(), json!(f));
                }
                if let Some(v) = views {
                    m.insert("pageViewCount".into(), json!(v));
                }
                if let Some(h) = hot {
                    m.insert("hotHome".into(), json!(h));
                }
                if let Some(ref u) = csv_url {
                    m.insert("url".into(), json!(u));
                }
                m.insert("source".into(), json!(csv_src));
                serde_json::to_string(&Value::Object(m)).ok()
            } else {
                None
            }
        };

        out.push(ListingInput {
            street: Some(street.to_string()),
            address: full_address,
            city,
            state,
            zip,
            lat: cell(r, i_lat).and_then(num_from_str),
            lon: cell(r, i_lon).and_then(num_from_str),
            status: Some(status.to_string()),
            price,
            list_price: if status == "sold" { None } else { price },
            sold_price: if status == "sold" { price } else { None },
            beds: cell(r, i_beds).and_then(num_from_str),
            baths: cell(r, i_baths).and_then(num_from_str),
            sqft: cell(r, i_sqft).and_then(int_from_str),
            lot_sqft: cell(r, i_lot).and_then(int_from_str),
            year_built: cell(r, i_year).and_then(int_from_str),
            days_on_market: cell(r, i_dom).and_then(int_from_str),
            hoa_monthly: cell(r, i_hoa).and_then(int_from_str),
            sold_date,
            property_type: Some(normalize_property_category(cell(r, i_type))),
            mls_number: cell(r, i_mls).map(ToOwned::to_owned),
            url: csv_url,
            source: Some(csv_src),
            raw_json: csv_raw_json,
            ..Default::default()
        });
    }
    out
}

// ---------------------------------------------------------------- Open Estate Report & Comps Harvester
/// Returns true if a comp/benchmark JSON item represents a synthetic ZIP ZHVI or parcel/roll
/// proxy rather than a verified arm's-length comparable sale.
pub fn is_synthetic_comp_entry(c: &Value) -> bool {
    if c.get("isSynthetic").and_then(Value::as_bool) == Some(true)
        || c.get("synthetic").and_then(Value::as_bool) == Some(true)
    {
        return true;
    }
    let src = c
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let deal = c
        .get("dealType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let note = c
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    src.contains("parcel + zhvi benchmark")
        || src.contains("parcel + home-value index benchmark")
        || src.contains("zhvi calibrated")
        || src.contains("home-value index calibrated")
        || src.contains("synthetic zip zhvi")
        || src.contains("synthetic zip home-value index")
        || src.contains("zip market & parcel calibration")
        || src.contains("prop 13 transfer roll proxy")
        || src.contains("synthetic")
        || deal.contains("neighbor parcel + zip benchmark")
        || deal.contains("parcel + zhvi")
        || deal.contains("parcel + home-value index")
        || deal.contains("zhvi benchmark")
        || deal.contains("home-value index benchmark")
        || deal.contains("zip benchmark")
        || deal.contains("roll proxy")
        || deal.contains("synthetic")
        || note.contains("synthetic")
        || note.contains("not an arm's-length recorded sale")
        || note.contains("not a verified mls closing price")
}
#[derive(Debug, Default)]
struct ReportExtractedFacets {
    safety_score: Option<f64>,
    has_hardwood_floors: Option<i64>,
    has_updated_kitchen: Option<i64>,
    has_remodeled: Option<i64>,
    quiet_street: Option<i64>,
    noise_max_db: Option<f64>,
    seismic_pga: Option<f64>,
    tags_json: Option<String>,
}

fn infer_feature_tags_and_flags(
    text_lc: &str,
    noise_db: Option<f64>,
    seismic_pga: Option<f64>,
    safety_score: Option<f64>,
    extra_tags: &[String],
) -> (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Vec<String>,
) {
    let has_hardwood = (text_lc.contains("hardwood")
        || text_lc.contains("wood floor")
        || text_lc.contains("oak floor")
        || text_lc.contains("maple floor")
        || text_lc.contains("engineered wood"))
    .then_some(1_i64);

    let has_updated_kitchen = (text_lc.contains("updated kitchen")
        || text_lc.contains("remodeled kitchen")
        || text_lc.contains("renovated kitchen")
        || text_lc.contains("modern kitchen")
        || text_lc.contains("chef's kitchen")
        || text_lc.contains("quartz kitchen")
        || text_lc.contains("granite kitchen")
        || text_lc.contains("new kitchen")
        || text_lc.contains("quartz countertops"))
    .then_some(1_i64);

    let has_remodeled = (text_lc.contains("remodeled")
        || text_lc.contains("renovated")
        || text_lc.contains("turnkey")
        || text_lc.contains("fully updated")
        || text_lc.contains("upgraded bath")
        || text_lc.contains("new laminate floors"))
    .then_some(1_i64);

    let loud_signal = text_lc.contains("busy road")
        || text_lc.contains("freeway")
        || text_lc.contains("highway")
        || text_lc.contains("noise buffer")
        || noise_db.map(|d| d > 58.0).unwrap_or(false);
    let quiet_signal = text_lc.contains("quiet street")
        || text_lc.contains("cul-de-sac")
        || text_lc.contains("cul de sac")
        || text_lc.contains("quiet neighborhood")
        || noise_db.map(|d| d <= 50.0).unwrap_or(false);
    let quiet_street = if loud_signal {
        Some(0_i64)
    } else if quiet_signal {
        Some(1_i64)
    } else {
        None
    };

    let mut tags: Vec<String> = Vec::new();
    let mut push_tag = |t: &str| {
        let clean = t.trim().to_ascii_lowercase().replace(' ', "_");
        if !clean.is_empty() && !tags.contains(&clean) {
            tags.push(clean);
        }
    };
    if has_hardwood == Some(1) {
        push_tag("hardwood_floors");
    }
    if has_updated_kitchen == Some(1) {
        push_tag("updated_kitchen");
    }
    if has_remodeled == Some(1) {
        push_tag("remodeled");
    }
    if quiet_street == Some(1) {
        push_tag("quiet_street");
    }
    if noise_db.map(|d| d <= 50.0).unwrap_or(false) {
        push_tag("low_noise");
    }
    if seismic_pga.map(|p| p <= 0.35).unwrap_or(false) {
        push_tag("low_seismic");
    }
    if safety_score.map(|s| s >= 70.0).unwrap_or(false) {
        push_tag("high_safety");
    }
    for (needle, tag) in [
        ("cul-de-sac", "cul_de_sac"),
        ("cul de sac", "cul_de_sac"),
        ("fireplace", "fireplace"),
        ("sunroom", "sunroom"),
        ("central a/c", "central_ac"),
        ("high ceilings", "high_ceilings"),
        ("fenced yard", "fenced_yard"),
        ("solar", "solar"),
        ("ev charger", "ev_charger"),
        ("heat pump", "heat_pump"),
        ("adu", "adu"),
        ("pool", "pool"),
    ] {
        if text_lc.contains(needle) {
            push_tag(tag);
        }
    }
    for t in extra_tags {
        push_tag(t);
    }
    tags.sort();
    (
        has_hardwood,
        has_updated_kitchen,
        has_remodeled,
        quiet_street,
        tags,
    )
}

fn extract_report_safety_noise_seismic_and_tags(
    report: &Value,
    facts: &Value,
    civic: &Value,
    haz: &Value,
    features_joined: &str,
) -> ReportExtractedFacets {
    let safety_score = num_from_val(report.pointer("/decisionFactors/safety/agencySafetyScore"))
        .or_else(|| num_from_val(civic.pointer("/crime/safetyScore")))
        .or_else(|| num_from_val(civic.pointer("/scores/safety")))
        .or_else(|| {
            let latest = civic.pointer("/crime/latest")?;
            let violent = num_from_val(latest.pointer("/violent/ratePer100k"));
            let violent_us = num_from_val(latest.pointer("/violent/nationalRatePer100k"));
            let prop_cr = num_from_val(latest.pointer("/property/ratePer100k"));
            let prop_us = num_from_val(latest.pointer("/property/nationalRatePer100k"));
            if violent.is_some() || prop_cr.is_some() {
                let vr = match (violent, violent_us) {
                    (Some(v), Some(u)) if u > 0.0 => v / u,
                    _ => 1.0,
                };
                let pr = match (prop_cr, prop_us) {
                    (Some(p), Some(u)) if u > 0.0 => p / u,
                    _ => 1.0,
                };
                Some(
                    (100.0 - (vr * 0.6 + pr * 0.4) * 34.0)
                        .clamp(20.0, 96.0)
                        .round(),
                )
            } else {
                None
            }
        });

    let env_data = report
        .pointer("/modules/environment/data")
        .or_else(|| report.pointer("/modules/environment"))
        .unwrap_or(&Value::Null);
    let noise_max_db = num_from_val(env_data.pointer("/noise/estLdnDb"))
        .or_else(|| num_from_val(env_data.pointer("/noise/roadDb")))
        .or_else(|| num_from_val(env_data.pointer("/noise/atProperty/all/highDb")))
        .or_else(|| num_from_val(env_data.pointer("/noise/maxWithinRadius/all/highDb")));

    let seismic_pga = num_from_val(haz.pointer("/earthquake/designMaps/pgaM"))
        .or_else(|| num_from_val(haz.pointer("/seismic/pgaG")));

    let mut text_parts: Vec<String> = vec![features_joined.to_string()];
    if let Some(about) = str_from_val(report.pointer("/modules/aiAnalyst/data/aboutHome"))
        .or_else(|| str_from_val(report.pointer("/aiAnalyst/aboutHome")))
    {
        text_parts.push(about);
    }
    if let Some(det) = facts.get("details").and_then(Value::as_object) {
        for arr in det.values().filter_map(Value::as_array) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    text_parts.push(s.to_string());
                }
            }
        }
    }
    if let Some(noise_notes) = facts.pointer("/localNotes/noise").and_then(Value::as_array) {
        for n in noise_notes {
            if let Some(s) = n.as_str() {
                text_parts.push(s.to_string());
            }
        }
    }
    let combined_lc = text_parts.join(" · ").to_ascii_lowercase();
    let (has_hardwood_floors, has_updated_kitchen, has_remodeled, quiet_street, tags) =
        infer_feature_tags_and_flags(&combined_lc, noise_max_db, seismic_pga, safety_score, &[]);

    ReportExtractedFacets {
        safety_score,
        has_hardwood_floors,
        has_updated_kitchen,
        has_remodeled,
        quiet_street,
        noise_max_db,
        seismic_pga,
        tags_json: (!tags.is_empty())
            .then(|| serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into())),
    }
}

pub fn extract_listings_from_report(report: &Value) -> Vec<ListingInput> {
    let geo = report
        .get("geo")
        .or_else(|| report.pointer("/modules/geocode/data"))
        .unwrap_or(&Value::Null);
    let Some(zip_raw) = str_from_val(geo.get("zip")) else {
        return Vec::new();
    };
    let zip: String = zip_raw.chars().take(5).collect();
    if zip.len() != 5 {
        return Vec::new();
    }

    let empty_obj = json!({});
    let facts = report
        .get("facts")
        .or_else(|| report.pointer("/modules/facts/data"))
        .unwrap_or(&empty_obj);
    let val = report
        .pointer("/modules/valuation/data")
        .unwrap_or(&empty_obj);
    let civic = report.pointer("/modules/civic/data").unwrap_or(&empty_obj);
    let haz = report
        .pointer("/modules/hazards/data")
        .unwrap_or(&empty_obj);

    let display_addr = str_from_val(geo.get("displayAddress"))
        .or_else(|| str_from_val(report.pointer("/meta/displayAddress")))
        .unwrap_or_default();
    let slug = str_from_val(report.pointer("/meta/slug"))
        .unwrap_or_else(|| listing_slug(&display_addr, None, None, None));
    let city = str_from_val(geo.get("city"));
    let state = str_from_val(geo.get("state"));

    // Schools: names, per-level ratings and the listing score come only from official public data — the
    // civic module's assigned-or-nearest public schools (NCES CCD + SABS) scored by
    // `civic::official_school_score`. GreatSchools / Niche values in `facts.schools.assigned` are never
    // read; facts only supply a school *name* for a level the civic module did not cover. No official
    // value means null (unknown).
    let assigned_schools = crate::modules::civic::assigned_official_schools(civic);
    let level_name =
        |a: &Option<crate::modules::civic::AssignedSchool>| a.as_ref().map(|s| s.name.clone());
    let level_rating = |a: &Option<crate::modules::civic::AssignedSchool>| {
        a.as_ref()
            .and_then(|s| s.official.as_ref().map(|o| o.score))
    };
    let mut school_elem_name = level_name(&assigned_schools.elementary);
    let school_elem_rating = level_rating(&assigned_schools.elementary);
    let mut school_mid_name = level_name(&assigned_schools.middle);
    let school_mid_rating = level_rating(&assigned_schools.middle);
    let mut school_high_name = level_name(&assigned_schools.high);
    let school_high_rating = level_rating(&assigned_schools.high);

    if let Some(facts_assigned) = facts.pointer("/schools/assigned").and_then(Value::as_array) {
        for s in facts_assigned {
            let Some(nm) = str_from_val(s.get("name")) else {
                continue;
            };
            let grades_str = str_from_val(s.get("grades")).unwrap_or_default();
            let nm_low = nm.to_ascii_lowercase();
            if nm_low.contains("elem")
                || nm_low.contains("primary")
                || ELEM_GRADE_RE.is_match(&grades_str)
            {
                school_elem_name.get_or_insert(nm);
            } else if nm_low.contains("middle")
                || nm_low.contains("junior")
                || MID_GRADE_RE.is_match(&grades_str)
            {
                school_mid_name.get_or_insert(nm);
            } else if nm_low.contains("high") || HIGH_GRADE_RE.is_match(&grades_str) {
                school_high_name.get_or_insert(nm);
            }
        }
    }

    let school_score = assigned_schools.summary_score();
    let school_grade = score_to_school_grade(school_score);

    let school_district = str_from_val(geo.pointer("/schoolDistricts/unified")).or_else(|| {
        let joined = [
            str_from_val(geo.pointer("/schoolDistricts/elementary")),
            str_from_val(geo.pointer("/schoolDistricts/secondary")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" / ");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    });

    let est_val = int_from_val(val.pointer("/estimate/value"));
    let mls_price = int_from_val(facts.pointer("/mlsListing/listPrice"));
    let last_sale_price = int_from_val(facts.pointer("/lastSale/price"));
    let raw_status = str_from_val(facts.pointer("/mlsListing/status"))
        .or_else(|| str_from_val(facts.get("status")))
        .unwrap_or_default();
    let subject_status = if ACTIVE_SUBJECT_RE.is_match(&raw_status) {
        "for_sale"
    } else if PENDING_SUBJECT_RE.is_match(&raw_status) {
        "pending"
    } else if last_sale_price.is_some() {
        "sold"
    } else {
        "off_market"
    };

    let mut features_parts: Vec<String> = Vec::new();
    if let Some(arr) = facts.get("features").and_then(Value::as_array) {
        for f in arr {
            if let Some(s) = str_from_val(Some(f)) {
                features_parts.push(s);
            }
        }
    }
    for extra in [
        str_from_val(facts.get("renovations")),
        str_from_val(facts.get("neighborhood")),
        str_from_val(facts.get("storiesNote")),
        str_from_val(facts.get("sqftNote")),
        str_from_val(facts.get("garage")),
        school_elem_name.clone(),
        school_mid_name.clone(),
        school_high_name.clone(),
        school_district.clone(),
    ]
    .into_iter()
    .flatten()
    {
        if !extra.is_empty() {
            features_parts.push(extra);
        }
    }

    let subject_addr = if display_addr.is_empty() {
        slug.clone()
    } else {
        display_addr.clone()
    };
    let subject_street = subject_addr
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let subject_prop_type =
        normalize_property_category(str_from_val(facts.get("propertyType")).as_deref());
    let features_joined = features_parts.join(" · ");
    let report_facets =
        extract_report_safety_noise_seismic_and_tags(report, facts, civic, haz, &features_joined);

    let mut out = vec![ListingInput {
        id: Some(slug.clone()),
        street: Some(subject_street),
        address: subject_addr,
        city: city.clone(),
        state: state.clone(),
        zip: zip.clone(),
        lat: num_from_val(geo.pointer("/refined/lat")).or_else(|| num_from_val(geo.get("lat"))),
        lon: num_from_val(geo.pointer("/refined/lon")).or_else(|| num_from_val(geo.get("lon"))),
        status: Some(subject_status.to_string()),
        price: if subject_status == "for_sale" {
            mls_price.or(est_val).or(last_sale_price)
        } else {
            last_sale_price.or(est_val)
        },
        list_price: mls_price,
        sold_price: last_sale_price,
        estimated_value: est_val,
        beds: num_from_val(facts.get("beds")),
        baths: num_from_val(facts.get("baths")),
        sqft: int_from_val(facts.get("sqft")),
        lot_sqft: int_from_val(facts.get("lotSqft")),
        year_built: int_from_val(facts.get("yearBuilt")),
        stories: num_from_val(facts.get("stories")),
        hoa_monthly: Some(int_from_val(facts.get("hoaMonthly")).unwrap_or(0)),
        days_on_market: int_from_val(facts.pointer("/mlsListing/daysOnMarket")),
        listed_date: str_from_val(facts.pointer("/mlsListing/listDate")),
        sold_date: str_from_val(facts.pointer("/lastSale/date")),
        school_score,
        school_grade: school_grade.clone(),
        school_elem_name: school_elem_name.clone(),
        school_elem_rating,
        school_mid_name: school_mid_name.clone(),
        school_mid_rating,
        school_high_name: school_high_name.clone(),
        school_high_rating,
        school_district: school_district.clone(),
        // 0–100 only: Walk Score from the facts, else the civic module's 0–100 walkability score.
        // The EPA National Walkability Index (`civic.walkability.index`) is on a 1–20 scale.
        walk_score: num_from_val(facts.pointer("/walkScore/walk"))
            .filter(|w| (0.0..=100.0).contains(w))
            .or_else(|| {
                num_from_val(civic.pointer("/scores/walkability"))
                    .filter(|w| (0.0..=100.0).contains(w))
            }),
        flood_zone: str_from_val(haz.pointer("/flood/zone")),
        // Short label with the same precedence as the hazards wildfire score: the CAL FIRE Fire
        // Hazard Severity Zone where the California checks ran ("CA FHSZ Very High", "CA FHSZ none"
        // outside a zone), else the USFS Wildfire Hazard Potential class ("WHP 4 High").
        // `compile_filters_to_sql` treats any label containing "high" as high wildfire risk.
        wildfire_zone: {
            let fhsz = haz
                .pointer("/wildfire/california")
                .filter(|v| v.is_object());
            let whp = haz.pointer("/wildfire/whp").filter(|v| v.is_object());
            let zone = if let Some(ca) = fhsz {
                let class = int_from_val(ca.get("fhszClass"));
                let in_zone = ca
                    .get("inFhsz")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| class.unwrap_or(0) >= 1);
                let label = match class {
                    Some(1) => Some("Moderate".to_string()),
                    Some(2) => Some("High".to_string()),
                    Some(3) => Some("Very High".to_string()),
                    _ => str_from_val(ca.get("fhsz")),
                };
                Some(match (in_zone, label) {
                    (false, _) => "CA FHSZ none".to_string(),
                    (true, Some(l)) => format!("CA FHSZ {l}"),
                    (true, None) => "CA FHSZ unknown class".to_string(),
                })
            } else if let Some(whp) = whp {
                let class = int_from_val(whp.get("class"));
                let label = match class {
                    Some(1) => Some("Very Low".to_string()),
                    Some(2) => Some("Low".to_string()),
                    Some(3) => Some("Moderate".to_string()),
                    Some(4) => Some("High".to_string()),
                    Some(5) => Some("Very High".to_string()),
                    Some(6) => Some("Non-burnable".to_string()),
                    Some(7) => Some("Water".to_string()),
                    _ => str_from_val(whp.get("label")),
                };
                match (class, label) {
                    (Some(c), Some(l)) => Some(format!("WHP {c} {l}")),
                    (Some(c), None) => Some(format!("WHP {c}")),
                    (None, Some(l)) => Some(format!("WHP {l}")),
                    (None, None) => None,
                }
            } else {
                None
            };
            // The column is VARCHAR(64).
            zone.map(|z| z.chars().take(64).collect())
        },
        overall_score: num_from_val(report.pointer("/summary/scores/overall")),
        verdict: str_from_val(report.pointer("/summary/verdict")),
        mls_number: str_from_val(facts.pointer("/mlsListing/mlsNumber")),
        property_type: Some(subject_prop_type.clone()),
        url: Some(format!("/r/{}/", urlencoding::encode(&slug))),
        photo_url: None,
        report_slug: Some(slug.clone()),
        features_text: Some(features_joined),
        safety_score: report_facets.safety_score,
        has_hardwood_floors: report_facets.has_hardwood_floors,
        has_updated_kitchen: report_facets.has_updated_kitchen,
        has_remodeled: report_facets.has_remodeled,
        quiet_street: report_facets.quiet_street,
        noise_max_db: report_facets.noise_max_db,
        seismic_pga: report_facets.seismic_pga,
        tags_json: report_facets.tags_json,
        source: Some("Open Estate Report".to_string()),
        raw_json: merge_report_shopping_json(
            crate::modules::engagement::extract_raw_engagement_json(facts),
            report,
        ),
    }];

    let mut seen_slugs: HashSet<String> = HashSet::new();
    seen_slugs.insert(slug);

    // Harvest hyperlocal active nearby listings
    let mut nearby_vals: Vec<&Value> = Vec::new();
    if let Some(arr) = facts.get("nearbyListings").and_then(Value::as_array) {
        nearby_vals.extend(arr.iter());
    }
    if let Some(arr) = val
        .pointer("/activeNearby/forSale")
        .and_then(Value::as_array)
    {
        nearby_vals.extend(arr.iter());
    }
    let state_zip = format!("{} {}", state.as_deref().unwrap_or(""), zip)
        .trim()
        .to_string();

    for l in nearby_vals {
        if is_synthetic_comp_entry(l) {
            continue;
        }
        let Some(raw_addr) = str_from_val(l.get("address")) else {
            continue;
        };
        let Some(price) = int_from_val(l.get("price")) else {
            continue;
        };
        let full_addr = if raw_addr.contains(',') {
            raw_addr.clone()
        } else {
            [
                Some(raw_addr.as_str()),
                city.as_deref(),
                Some(state_zip.as_str()),
            ]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
        };
        let l_slug = listing_slug(&full_addr, None, None, None);
        if l_slug.is_empty() || !seen_slugs.insert(l_slug.clone()) {
            continue;
        }
        let f_txt = [
            str_from_val(l.get("listingType")),
            str_from_val(l.get("note")),
            str_from_val(facts.get("neighborhood")),
            school_elem_name.clone(),
            school_high_name.clone(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");

        out.push(ListingInput {
            id: Some(l_slug),
            street: Some(raw_addr.split(',').next().unwrap_or("").trim().to_string()),
            address: full_addr.clone(),
            city: city.clone(),
            state: state.clone(),
            zip: zip.clone(),
            lat: num_from_val(l.get("lat")),
            lon: num_from_val(l.get("lon")),
            status: Some(
                normalize_listing_status(str_from_val(l.get("status")).as_deref()).to_string(),
            ),
            price: Some(price),
            list_price: Some(price),
            beds: num_from_val(l.get("beds")),
            baths: num_from_val(l.get("baths")),
            sqft: int_from_val(l.get("sqft")),
            year_built: int_from_val(l.get("yearBuilt")),
            days_on_market: int_from_val(l.get("daysOnMarket")),
            listed_date: str_from_val(l.get("listedDate")),
            school_score,
            school_grade: school_grade.clone(),
            school_elem_name: school_elem_name.clone(),
            school_elem_rating,
            school_mid_name: school_mid_name.clone(),
            school_mid_rating,
            school_high_name: school_high_name.clone(),
            school_high_rating,
            school_district: school_district.clone(),
            property_type: Some(normalize_property_category(
                str_from_val(l.get("assetClass"))
                    .or_else(|| str_from_val(facts.get("propertyType")))
                    .as_deref(),
            )),
            mls_number: str_from_val(l.get("mlsNumber")),
            url: Some(format!(
                "/report?address={}",
                urlencoding::encode(&full_addr)
            )),
            features_text: Some(f_txt),
            source: Some(
                str_from_val(l.get("source"))
                    .unwrap_or_else(|| "Open Estate Nearby Active".to_string()),
            ),
            ..Default::default()
        });
    }

    // Harvest neighborhood sold comparables (`facts.comps` + `val.comps`)
    let mut comp_vals: Vec<&Value> = Vec::new();
    if let Some(arr) = facts.get("comps").and_then(Value::as_array) {
        comp_vals.extend(arr.iter());
    }
    if let Some(arr) = val.get("comps").and_then(Value::as_array) {
        comp_vals.extend(arr.iter());
    }
    for c in comp_vals {
        if is_synthetic_comp_entry(c) {
            continue;
        }
        let Some(raw_addr) = str_from_val(c.get("address")) else {
            continue;
        };
        let Some(price) = int_from_val(c.get("price")) else {
            continue;
        };
        let full_addr = if raw_addr.contains(',') {
            raw_addr.clone()
        } else {
            [
                Some(raw_addr.as_str()),
                city.as_deref(),
                Some(state_zip.as_str()),
            ]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
        };
        let c_slug = listing_slug(&full_addr, None, None, None);
        if c_slug.is_empty() || !seen_slugs.insert(c_slug.clone()) {
            continue;
        }
        let note_str = str_from_val(c.get("note")).unwrap_or_default();
        let stories = if ONE_STORY_RE.is_match(&note_str) {
            Some(1.0)
        } else if TWO_STORY_RE.is_match(&note_str) {
            Some(2.0)
        } else {
            None
        };
        let f_txt = [
            str_from_val(c.get("note")),
            str_from_val(c.get("dealType")),
            str_from_val(facts.get("neighborhood")),
            school_elem_name.clone(),
            school_high_name.clone(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");

        out.push(ListingInput {
            id: Some(c_slug),
            street: Some(raw_addr.split(',').next().unwrap_or("").trim().to_string()),
            address: full_addr.clone(),
            city: city.clone(),
            state: state.clone(),
            zip: zip.clone(),
            lat: num_from_val(c.get("lat")),
            lon: num_from_val(c.get("lon")),
            status: Some("sold".to_string()),
            price: Some(price),
            list_price: int_from_val(c.get("listPrice")),
            sold_price: Some(price),
            beds: num_from_val(c.get("beds")),
            baths: num_from_val(c.get("baths")),
            sqft: int_from_val(c.get("sqft")),
            lot_sqft: int_from_val(c.get("lotSqft")),
            year_built: int_from_val(c.get("yearBuilt")),
            stories,
            days_on_market: int_from_val(c.get("daysOnMarket")),
            sold_date: str_from_val(c.get("soldDate")),
            school_score,
            school_grade: school_grade.clone(),
            school_elem_name: school_elem_name.clone(),
            school_elem_rating,
            school_mid_name: school_mid_name.clone(),
            school_mid_rating,
            school_high_name: school_high_name.clone(),
            school_high_rating,
            school_district: school_district.clone(),
            property_type: Some(normalize_property_category(
                str_from_val(c.get("assetClass"))
                    .or_else(|| str_from_val(facts.get("propertyType")))
                    .as_deref(),
            )),
            mls_number: str_from_val(c.get("mlsNumber")),
            url: Some(format!(
                "/report?address={}",
                urlencoding::encode(&full_addr)
            )),
            features_text: Some(f_txt),
            source: Some(
                str_from_val(c.get("source"))
                    .unwrap_or_else(|| "Open Estate Sold Comp".to_string()),
            ),
            ..Default::default()
        });
    }

    out
}

// ---------------------------------------------------------------- School Context per ZIP
/// A public school near a ZIP: NCES directory facts (name, level, coordinates) cached per ZIP.
///
/// `rating` is an official 0–10 academic score attached at read time from public data (never cached);
/// it is `None` when no official value exists. Ratings stored in older cache rows (the former
/// student–teacher-ratio proxy) are ignored on deserialization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedSchoolEntry {
    pub nces_id: Option<String>,
    pub name: String,
    pub district: Option<String>,
    pub level: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(skip)]
    pub rating: Option<f64>,
    #[serde(skip)]
    pub rating_source: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ZipSchoolContext {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub schools: Vec<CachedSchoolEntry>,
}

fn classify_nces_level(level_text: Option<&str>, gslo: Option<&str>, gshi: Option<&str>) -> String {
    let t = level_text.unwrap_or("").to_ascii_lowercase();
    if t.contains("elementary") || t.contains("primary") {
        return "elementary".to_string();
    }
    if t.contains("middle") || t.contains("junior") {
        return "middle".to_string();
    }
    if t.contains("high") || t.contains("secondary") {
        return "high".to_string();
    }
    let hi = gshi.and_then(|s| s.parse::<i32>().ok());
    let lo = gslo.and_then(|s| s.parse::<i32>().ok());
    if let Some(h) = hi {
        if h <= 6 {
            return "elementary".to_string();
        }
    }
    if let Some(l) = lo {
        if l >= 9 {
            return "high".to_string();
        }
    }
    if let (Some(l), Some(h)) = (lo, hi) {
        if l >= 5 && h <= 9 {
            return "middle".to_string();
        }
    }
    "other".to_string()
}

pub async fn lookup_zip_centroid(http: &HttpClient, zip: &str) -> Option<(f64, f64)> {
    let clean_digits: String = zip.chars().filter(|c| c.is_ascii_digit()).collect();
    let params = vec![
        ("where", format!("ZCTA5='{clean_digits}'")),
        ("outFields", "ZCTA5,CENTLAT,CENTLON".to_string()),
        ("returnGeometry", "false".to_string()),
    ];
    let j = http
        .arcgis_query(TIGER_ZCTA_URL, &params, 6500)
        .await
        .ok()?;
    let attr = j.pointer("/features/0/attributes")?;
    let lat = num_from_val(attr.get("CENTLAT"))?;
    let lon = num_from_val(attr.get("CENTLON"))?;
    Some((lat, lon))
}

async fn fetch_schools_near_zip(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    _env: &EnvConfig,
) -> Vec<CachedSchoolEntry> {
    let radius_m = 3.5 * MILE_M;
    for y in PUBLIC_YEARS {
        let url = format!("{NCES_BASE}/EDGE_ADMINDATA_PUBLICSCH_{y}/MapServer/1");
        if let Ok(feats) = http
            .arcgis_radius_query(
                &url,
                lat,
                lon,
                radius_m,
                "NCESSCH,SCH_NAME,LEA_NAME,GSLO,GSHI,SCHOOL_LEVEL,SY_STATUS_TEXT,VIRTUAL",
                &[],
                7000,
            )
            .await
        {
            let mut schools = Vec::new();
            for f in feats {
                let Some(a) = f.get("attributes") else {
                    continue;
                };
                let Some(g) = f.get("geometry") else {
                    continue;
                };
                let status_txt = str_from_val(a.get("SY_STATUS_TEXT"))
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if status_txt.contains("closed")
                    || status_txt.contains("inactive")
                    || status_txt.contains("future")
                {
                    continue;
                }
                let virt = str_from_val(a.get("VIRTUAL"))
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if virt == "full virtual" {
                    continue;
                }
                let (Some(sy), Some(sx)) = (num_from_val(g.get("y")), num_from_val(g.get("x")))
                else {
                    continue;
                };
                // Names, levels and coordinates only: no rating is derived from staffing data.
                schools.push(CachedSchoolEntry {
                    nces_id: str_from_val(a.get("NCESSCH")),
                    name: str_from_val(a.get("SCH_NAME")).unwrap_or_default(),
                    district: str_from_val(a.get("LEA_NAME")),
                    level: classify_nces_level(
                        str_from_val(a.get("SCHOOL_LEVEL")).as_deref(),
                        str_from_val(a.get("GSLO")).as_deref(),
                        str_from_val(a.get("GSHI")).as_deref(),
                    ),
                    lat: sy,
                    lon: sx,
                    rating: None,
                    rating_source: None,
                });
            }
            return schools;
        }
    }
    Vec::new()
}

async fn attach_official_school_ratings(
    db: &DbStore,
    env: &EnvConfig,
    schools: &mut [CachedSchoolEntry],
) {
    let nces_ids: Vec<String> = schools.iter().filter_map(|s| s.nces_id.clone()).collect();
    if nces_ids.is_empty() {
        return;
    }
    let metrics =
        crate::modules::school_metrics::ca_metrics_for_report(Some(db), env, &nces_ids).await;
    if metrics.is_empty() {
        return;
    }
    for s in schools.iter_mut() {
        let Some(ref id) = s.nces_id else { continue };
        if let Some(m) = metrics.get(id) {
            s.rating = Some(round_n(m.percentile / 10.0, 1));
            s.rating_source = Some(m.source.clone());
        }
    }
}

pub async fn get_zip_school_context(
    db: &DbStore,
    http: Option<&HttpClient>,
    env: &EnvConfig,
    zip: &str,
    hint_lat: Option<f64>,
    hint_lon: Option<f64>,
) -> Result<ZipSchoolContext> {
    if let Some((c_lat, c_lon, schools_json, updated_at)) = db.get_zip_school_cache_row(zip).await?
    {
        let fresh = chrono::DateTime::parse_from_rfc3339(&updated_at)
            .map(|dt| {
                chrono::Utc::now()
                    .signed_duration_since(dt.with_timezone(&chrono::Utc))
                    .num_seconds()
                    < SCHOOL_CACHE_TTL_SECS
            })
            .unwrap_or(false);
        if fresh {
            if let Ok(mut schools) = serde_json::from_str::<Vec<CachedSchoolEntry>>(&schools_json) {
                attach_official_school_ratings(db, env, &mut schools).await;
                return Ok(ZipSchoolContext {
                    lat: c_lat.or(hint_lat),
                    lon: c_lon.or(hint_lon),
                    schools,
                });
            }
        }
    }

    let Some(client) = http else {
        return Ok(ZipSchoolContext {
            lat: hint_lat,
            lon: hint_lon,
            schools: Vec::new(),
        });
    };

    let mut lat = hint_lat;
    let mut lon = hint_lon;
    if lat.is_none() || lon.is_none() {
        if let Some((clat, clon)) = lookup_zip_centroid(client, zip).await {
            lat = Some(clat);
            lon = Some(clon);
        }
    }
    let (Some(la), Some(lo)) = (lat, lon) else {
        return Ok(ZipSchoolContext::default());
    };

    let mut schools = fetch_schools_near_zip(client, la, lo, env).await;
    let schools_json = serde_json::to_string(&schools).unwrap_or_else(|_| "[]".to_string());
    let _ = db
        .upsert_row(
            "zip_school_cache",
            &[
                ("zip", zip.into()),
                ("lat", Some(la).into()),
                ("lon", Some(lo).into()),
                ("schools_json", schools_json.into()),
                ("updated_at", now_iso().into()),
            ],
            &["zip"],
        )
        .await;
    attach_official_school_ratings(db, env, &mut schools).await;

    Ok(ZipSchoolContext {
        lat: Some(la),
        lon: Some(lo),
        schools,
    })
}

pub fn assign_schools_to_listing(item: &mut ListingInput, ctx: Option<&ZipSchoolContext>) {
    if item.school_score.is_some() && item.school_grade.is_some() {
        return;
    }
    let Some(school_ctx) = ctx else {
        return;
    };
    if school_ctx.schools.is_empty() {
        return;
    }
    let (Some(lat), Some(lon)) = (item.lat.or(school_ctx.lat), item.lon.or(school_ctx.lon)) else {
        return;
    };

    let nearest_by_level = |lvl: &str| -> Option<&CachedSchoolEntry> {
        school_ctx
            .schools
            .iter()
            .filter(|s| s.level == lvl)
            .min_by(|a, b| {
                let da = haversine_m(lat, lon, a.lat, a.lon);
                let db_dist = haversine_m(lat, lon, b.lat, b.lon);
                da.partial_cmp(&db_dist)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    };

    let elem = nearest_by_level("elementary");
    let mid = nearest_by_level("middle");
    let high = nearest_by_level("high");
    let ratings: Vec<f64> = [
        elem.and_then(|s| s.rating),
        mid.and_then(|s| s.rating),
        high.and_then(|s| s.rating),
    ]
    .into_iter()
    .flatten()
    .filter(|r| r.is_finite())
    .collect();
    let avg = if ratings.is_empty() {
        None
    } else {
        Some(round_n(
            ratings.iter().sum::<f64>() / (ratings.len() as f64),
            1,
        ))
    };

    if item.school_score.is_none() {
        item.school_score = avg;
    }
    if item.school_grade.is_none() {
        item.school_grade = score_to_school_grade(item.school_score);
    }
    if item.school_elem_name.is_none() {
        item.school_elem_name = elem.map(|s| s.name.clone());
    }
    if item.school_elem_rating.is_none() {
        item.school_elem_rating = elem.and_then(|s| s.rating);
    }
    if item.school_mid_name.is_none() {
        item.school_mid_name = mid.map(|s| s.name.clone());
    }
    if item.school_mid_rating.is_none() {
        item.school_mid_rating = mid.and_then(|s| s.rating);
    }
    if item.school_high_name.is_none() {
        item.school_high_name = high.map(|s| s.name.clone());
    }
    if item.school_high_rating.is_none() {
        item.school_high_rating = high.and_then(|s| s.rating);
    }
    if item.school_district.is_none() {
        item.school_district = high
            .and_then(|s| s.district.clone())
            .or_else(|| elem.and_then(|s| s.district.clone()));
    }
}

// ---------------------------------------------------------------- Shopping Facets & Utility/Fiber Enrichment
fn bool_as_flag(v: Option<&Value>) -> Option<i64> {
    let val = v?;
    if let Some(b) = val.as_bool() {
        Some(if b { 1 } else { 0 })
    } else if let Some(n) = val.as_i64() {
        Some(if n != 0 { 1 } else { 0 })
    } else if let Some(s) = val.as_str() {
        match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Some(1),
            "0" | "false" | "no" => Some(0),
            _ => None,
        }
    } else {
        None
    }
}

fn str_or_joined_array(v: Option<&Value>) -> Option<String> {
    let val = v?;
    if let Some(s) = val.as_str() {
        let t = s.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    } else if let Some(arr) = val.as_array() {
        let mut names = Vec::new();
        for item in arr {
            let nm = item
                .as_str()
                .or_else(|| item.get("carrierName").and_then(Value::as_str))
                .or_else(|| item.get("name").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(n) = nm {
                if !names
                    .iter()
                    .any(|existing: &String| existing.eq_ignore_ascii_case(n))
                {
                    names.push(n.to_string());
                }
            }
        }
        if !names.is_empty() {
            return Some(names.join(", "));
        }
    }
    None
}

fn merge_two_raw_json_blobs(prev: Option<&str>, next: &str) -> String {
    let Some(prev_s) = prev else {
        return next.to_string();
    };
    let (Ok(Value::Object(mut p_map)), Ok(Value::Object(n_map))) = (
        serde_json::from_str::<Value>(prev_s),
        serde_json::from_str::<Value>(next),
    ) else {
        return next.to_string();
    };
    for (k, v) in n_map {
        if k == "_shoppingFacets" {
            if let (Some(Value::Object(p_sf)), Value::Object(n_sf)) =
                (p_map.get_mut("_shoppingFacets"), &v)
            {
                for (sk, sv) in n_sf {
                    if !sv.is_null() {
                        p_sf.insert(sk.clone(), sv.clone());
                    }
                }
                continue;
            }
        }
        if !v.is_null() {
            p_map.insert(k, v);
        }
    }
    serde_json::to_string(&Value::Object(p_map)).unwrap_or_else(|_| next.to_string())
}

fn attach_shopping_facets_to_raw(
    existing_raw: Option<String>,
    sf: serde_json::Map<String, Value>,
) -> Option<String> {
    if sf.is_empty() {
        return existing_raw;
    }
    let mut root = existing_raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    root.insert("_shoppingFacets".to_string(), Value::Object(sf));
    serde_json::to_string(&Value::Object(root))
        .ok()
        .or(existing_raw)
}

fn merge_explicit_shopping_json(existing_raw: Option<String>, v: &Value) -> Option<String> {
    let Some(obj) = v.as_object() else {
        return existing_raw;
    };
    let mut sf = obj
        .get("_shoppingFacets")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let get = |k1: &str, k2: &str| obj.get(k1).or_else(|| obj.get(k2));

    for (canon, k1, k2) in [
        ("electricCarrier", "electricCarrier", "electric_carrier"),
        (
            "electricUtilityType",
            "electricUtilityType",
            "electric_utility_type",
        ),
        ("gasCarrier", "gasCarrier", "gas_carrier"),
        ("waterCarrier", "waterCarrier", "water_carrier"),
        ("waterSourceType", "waterSourceType", "water_source_type"),
        ("sewerType", "sewerType", "sewer_type"),
        ("internetTier", "internetTier", "internet_tier"),
        (
            "architecturalStyle",
            "architecturalStyle",
            "architectural_style",
        ),
        ("countyFips", "countyFips", "county_fips"),
    ] {
        if let Some(s) = str_from_val(get(k1, k2)) {
            sf.insert(canon.to_string(), json!(s));
        }
    }
    for (canon, k1, k2) in [
        ("internetCarriers", "internetCarriers", "internet_carriers"),
        ("fiberCarriers", "fiberCarriers", "fiber_carriers"),
    ] {
        if let Some(s) = str_or_joined_array(get(k1, k2)) {
            sf.insert(canon.to_string(), json!(s));
        }
    }
    for (canon, k1, k2) in [
        ("electricRateKwh", "electricRateKwh", "electric_rate_kwh"),
        ("gasRateTherm", "gasRateTherm", "gas_rate_therm"),
        ("propertyTaxRate", "propertyTaxRate", "property_tax_rate"),
        ("garageSpaces", "garageSpaces", "garage_spaces"),
    ] {
        if let Some(n) = num_from_val(get(k1, k2)) {
            sf.insert(canon.to_string(), json!(n));
        }
    }
    for (canon, k1, k2) in [
        (
            "estMonthlyUtilities",
            "estMonthlyUtilities",
            "est_monthly_utilities",
        ),
        ("maxDownloadMbps", "maxDownloadMbps", "max_download_mbps"),
        ("maxUploadMbps", "maxUploadMbps", "max_upload_mbps"),
        ("allInMonthly", "allInMonthly", "all_in_monthly"),
    ] {
        if let Some(n) = int_from_val(get(k1, k2)) {
            sf.insert(canon.to_string(), json!(n));
        }
    }
    for (canon, k1, k2) in [
        ("fiberAvailable", "fiberAvailable", "fiber_available"),
        (
            "highSpeedInternet",
            "highSpeedInternet",
            "high_speed_internet",
        ),
        ("hasSpecialTax", "hasSpecialTax", "has_special_tax"),
        ("hasSolar", "hasSolar", "has_solar"),
        ("hasEvCharger", "hasEvCharger", "has_ev_charger"),
        ("hasHeatPump", "hasHeatPump", "has_heat_pump"),
        ("hasAdu", "hasAdu", "has_adu"),
        ("aduFeasible", "aduFeasible", "adu_feasible"),
        ("hasPool", "hasPool", "has_pool"),
    ] {
        if let Some(flag) = bool_as_flag(get(k1, k2)) {
            sf.insert(canon.to_string(), json!(flag));
        }
    }

    attach_shopping_facets_to_raw(existing_raw, sf)
}

fn merge_report_shopping_json(existing_raw: Option<String>, report: &Value) -> Option<String> {
    let mut sf = serde_json::Map::new();
    let geo = report
        .get("geo")
        .or_else(|| report.pointer("/modules/geocode/data"))
        .unwrap_or(&Value::Null);
    if let Some(fips) =
        str_from_val(geo.pointer("/fips/county")).or_else(|| str_from_val(geo.get("countyFips")))
    {
        sf.insert("countyFips".to_string(), json!(fips));
    }

    let bd = report
        .pointer("/modules/buyerDiligence/data")
        .or_else(|| report.get("buyerDiligence"))
        .unwrap_or(&Value::Null);
    let mu = bd
        .pointer("/ongoingCosts/monthlyUtilities")
        .unwrap_or(&Value::Null);
    if let Some(ec) = str_from_val(mu.pointer("/electricCarrier/shortName"))
        .or_else(|| str_from_val(mu.pointer("/electricCarrier/carrierName")))
    {
        sf.insert("electricCarrier".to_string(), json!(ec));
    }
    if let Some(et) = str_from_val(mu.pointer("/electricCarrier/utilityType")) {
        sf.insert("electricUtilityType".to_string(), json!(et));
    }
    if let Some(rate) = num_from_val(mu.pointer("/electricCarrier/ratePerKwhUsd"))
        .or_else(|| num_from_val(mu.get("electricityRateKwh")))
    {
        sf.insert("electricRateKwh".to_string(), json!(rate));
    }
    if let Some(gc) = str_from_val(mu.pointer("/gasCarrier/carrierName")) {
        sf.insert("gasCarrier".to_string(), json!(gc));
    }
    if let Some(gr) = num_from_val(mu.pointer("/gasCarrier/ratePerThermUsd")) {
        sf.insert("gasRateTherm".to_string(), json!(gr));
    }
    if let Some(wc) = str_from_val(mu.pointer("/waterSewerTrash/waterCarrierName"))
        .or_else(|| str_from_val(report.pointer("/modules/environment/data/water/primary/name")))
    {
        sf.insert("waterCarrier".to_string(), json!(wc));
    }
    if let Some(ws) = str_from_val(mu.pointer("/waterSewerTrash/waterSourceType")) {
        sf.insert("waterSourceType".to_string(), json!(ws));
    }
    if let Some(st) = str_from_val(mu.pointer("/waterSewerTrash/sewerType")) {
        sf.insert("sewerType".to_string(), json!(st));
    }
    if let Some(tot_u) = int_from_val(mu.get("totalMonthlyUtilitiesUsd")) {
        sf.insert("estMonthlyUtilities".to_string(), json!(tot_u));
    }
    if let Some(burn) = int_from_val(
        bd.pointer("/ongoingCosts/allInMonthlyBurn/allInMonthlyBurnUsd"),
    )
    .or_else(|| int_from_val(report.pointer("/modules/valuation/data/ownership/monthly/total")))
    {
        sf.insert("allInMonthly".to_string(), json!(burn));
    }
    if let Some(sp) = bool_as_flag(
        bd.pointer("/ongoingCosts/specialTaxDistricts/hasConfirmedOrSuspectedDistrict"),
    ) {
        sf.insert("hasSpecialTax".to_string(), json!(sp));
    }

    let bc = bd
        .pointer("/connectivity/broadbandCatalog")
        .unwrap_or(&Value::Null);
    let bc_vs = str_from_val(bc.get("verificationStatus")).unwrap_or_default();
    let bc_verified = bool_as_flag(bc.get("fiberConfirmedFcc")) == Some(1)
        || matches!(bc_vs.as_str(), "fcc_location_verified" | "address_verified");
    if bc_verified {
        if let Some(tier) = str_from_val(bc.get("internetTier")) {
            sf.insert("internetTier".to_string(), json!(tier));
        }
        if let Some(fa) = bool_as_flag(bc.get("fiberAvailable")) {
            sf.insert("fiberAvailable".to_string(), json!(fa));
        }
        if let Some(hs) = bool_as_flag(bc.get("highSpeedAvailable")) {
            sf.insert("highSpeedInternet".to_string(), json!(hs));
        }
        if let Some(dl) = int_from_val(bc.get("maxDownloadMbps")).filter(|&d| d > 0) {
            sf.insert("maxDownloadMbps".to_string(), json!(dl));
        }
        if let Some(ul) = int_from_val(bc.get("maxUploadMbps")).filter(|&u| u > 0) {
            sf.insert("maxUploadMbps".to_string(), json!(ul));
        }
        if let Some(fc) = str_or_joined_array(bc.get("fiberCarriers")) {
            sf.insert("fiberCarriers".to_string(), json!(fc));
        }
        let mut all_isp: Vec<String> = Vec::new();
        for key in ["fiberCarriers", "cableCarriers", "wirelessCarriers"] {
            if let Some(arr) = bc.get(key).and_then(Value::as_array) {
                for item in arr {
                    if let Some(nm) = item
                        .get("carrierName")
                        .or_else(|| item.get("name"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        if !all_isp.iter().any(|e| e.eq_ignore_ascii_case(nm)) {
                            all_isp.push(nm.to_string());
                        }
                    }
                }
            }
        }
        if !all_isp.is_empty() {
            sf.insert("internetCarriers".to_string(), json!(all_isp.join(", ")));
        }
    }

    let facts = report
        .get("facts")
        .or_else(|| report.pointer("/modules/facts/data"))
        .unwrap_or(&Value::Null);
    if let Some(txr) = num_from_val(
        report.pointer("/modules/valuation/data/ownership/assumptions/propertyTaxRate"),
    )
    .or_else(|| num_from_val(facts.get("propertyTaxRate")))
    {
        let norm_rate = if txr > 0.25 { txr / 100.0 } else { txr };
        sf.insert("propertyTaxRate".to_string(), json!(round_n(norm_rate, 4)));
    }
    if let Some(style) = str_from_val(facts.get("architecturalStyle"))
        .or_else(|| str_from_val(facts.get("architectureType")))
    {
        sf.insert("architecturalStyle".to_string(), json!(style));
    }
    if let Some(gar) = num_from_val(facts.get("garageSpaces")) {
        sf.insert("garageSpaces".to_string(), json!(gar));
    }
    if let Some(pool) =
        bool_as_flag(mu.get("hasPoolSignal")).or_else(|| bool_as_flag(facts.get("hasPool")))
    {
        sf.insert("hasPool".to_string(), json!(pool));
    }

    // Check propertyFeatures smart tags if present
    if let Some(tags) = report
        .pointer("/modules/propertyFeatures/data/tags")
        .and_then(Value::as_array)
    {
        for t in tags {
            let id_lc = t
                .get("id")
                .or_else(|| t.get("label"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let status_lc = t
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("confirmed")
                .to_ascii_lowercase();
            if status_lc.contains("absent") || status_lc.contains("none") {
                continue;
            }
            if id_lc.contains("solar") || id_lc.contains("powerwall") {
                sf.insert("hasSolar".to_string(), json!(1));
            }
            if id_lc.contains("ev") || id_lc.contains("charger") || id_lc.contains("nema") {
                sf.insert("hasEvCharger".to_string(), json!(1));
            }
            if id_lc.contains("heat_pump")
                || id_lc.contains("heat pump")
                || id_lc.contains("mini-split")
            {
                sf.insert("hasHeatPump".to_string(), json!(1));
            }
            if id_lc.contains("adu") && !id_lc.contains("eligible") && !id_lc.contains("potential")
            {
                sf.insert("hasAdu".to_string(), json!(1));
            }
            if id_lc.contains("pool") {
                sf.insert("hasPool".to_string(), json!(1));
            }
        }
    }
    if let Some(adu_f) =
        bool_as_flag(report.pointer("/modules/remodelAndRebuild/data/adu/eligible")).or_else(|| {
            bool_as_flag(report.pointer("/modules/remodelAndRebuild/data/aduPotential/feasible"))
        })
    {
        sf.insert("aduFeasible".to_string(), json!(adu_f));
    }

    attach_shopping_facets_to_raw(existing_raw, sf)
}

fn infer_geo_from_zip(zip: &str) -> (&'static str, &'static str, &'static str) {
    let z5: String = zip.trim().chars().take(5).collect();
    // Exact 5-digit municipal utility & metro overrides first
    match z5.as_str() {
        "95050" | "95051" | "95054" | "95055" => return ("CA", "06085", "Santa Clara"),
        "94301" | "94303" | "94304" | "94306" => return ("CA", "06085", "Palo Alto"),
        "94501" | "94502" => return ("CA", "06001", "Alameda"),
        "95661" | "95678" | "95747" => return ("CA", "06061", "Roseville"),
        "91501" | "91502" | "91504" | "91505" | "91506" => return ("CA", "06037", "Burbank"),
        "91201" | "91202" | "91203" | "91204" | "91205" | "91206" | "91207" | "91208" => {
            return ("CA", "06037", "Glendale")
        }
        "91101" | "91103" | "91104" | "91105" | "91106" | "91107" => {
            return ("CA", "06037", "Pasadena")
        }
        "92801" | "92802" | "92804" | "92805" | "92806" | "92807" | "92808" => {
            return ("CA", "06059", "Anaheim")
        }
        _ => {}
    }

    let Ok(z_num) = z5.parse::<u32>() else {
        return ("", "", "");
    };
    match z_num {
        94000..=94099 | 94400..=94499 => return ("CA", "06081", ""),
        94100..=94199 => return ("CA", "06075", "San Francisco"),
        94300..=94399 | 95000..=95199 => return ("CA", "06085", "San Jose"),
        94500..=94899 => return ("CA", "06001", ""),
        94900..=94999 => return ("CA", "06041", ""),
        95608..=95630 | 95670 | 95800..=95899 => return ("CA", "06067", "Sacramento"),
        90001..=90099 | 91300..=91699 => return ("CA", "06037", "Los Angeles"),
        90200..=91299 | 91700..=91899 => return ("CA", "06037", ""),
        92600..=92899 => return ("CA", "06059", ""),
        91900..=92199 => return ("CA", "06073", "San Diego"),
        78700..=78799 => return ("TX", "48453", "Austin"),
        78200..=78299 => return ("TX", "48029", "San Antonio"),
        77000..=77599 => return ("TX", "48201", "Houston"),
        75000..=76299 => return ("TX", "48113", "Dallas"),
        98100..=98199 => return ("WA", "53033", "Seattle"),
        98000..=98099 => return ("WA", "53033", "Bellevue"),
        98400..=98499 => return ("WA", "53053", "Tacoma"),
        98200..=98299 => return ("WA", "53061", "Everett"),
        10001..=10299 => return ("NY", "36061", "New York"),
        11200..=11299 => return ("NY", "36047", "Brooklyn"),
        11100..=11499 => return ("NY", "36081", "Queens"),
        33100..=33299 => return ("FL", "12086", "Miami"),
        32800..=32899 => return ("FL", "12095", "Orlando"),
        32200..=32299 => return ("FL", "12031", "Jacksonville"),
        33600..=33699 => return ("FL", "12057", "Tampa"),
        85000..=85399 => return ("AZ", "04013", "Phoenix"),
        80200..=80299 => return ("CO", "08031", "Denver"),
        60600..=60699 => return ("IL", "17031", "Chicago"),
        2100..=2299 => return ("MA", "25025", "Boston"),
        _ => {}
    }

    let p3 = z_num / 100;
    let st = match p3 {
        10..=27 => "MA",
        28..=29 => "RI",
        30..=38 => "NH",
        39..=49 => "ME",
        50..=59 => "VT",
        60..=69 => "CT",
        70..=89 => "NJ",
        100..=149 => "NY",
        150..=196 => "PA",
        197..=199 => "DE",
        200 | 202..=205 => "DC",
        201 | 220..=246 => "VA",
        206..=219 => "MD",
        247..=268 => "WV",
        270..=289 => "NC",
        290..=299 => "SC",
        300..=319 | 398..=399 => "GA",
        320..=349 => "FL",
        350..=369 => "AL",
        370..=385 => "TN",
        386..=397 => "MS",
        400..=427 => "KY",
        430..=459 => "OH",
        460..=479 => "IN",
        480..=499 => "MI",
        500..=528 => "IA",
        530..=549 => "WI",
        550..=567 => "MN",
        570..=577 => "SD",
        580..=588 => "ND",
        590..=599 => "MT",
        600..=629 => "IL",
        630..=658 => "MO",
        660..=679 => "KS",
        680..=693 => "NE",
        700..=714 => "LA",
        716..=729 => "AR",
        730..=749 => "OK",
        750..=799 | 885 => "TX",
        800..=816 => "CO",
        820..=831 => "WY",
        832..=838 => "ID",
        840..=847 => "UT",
        850..=865 => "AZ",
        870..=884 => "NM",
        889..=898 => "NV",
        900..=961 => "CA",
        967..=968 => "HI",
        970..=979 => "OR",
        980..=994 => "WA",
        995..=999 => "AK",
        _ => "",
    };
    (st, "", "")
}

struct ResolvedShoppingFacets {
    inferred_state: Option<String>,
    electric_carrier: Option<String>,
    electric_utility_type: Option<String>,
    electric_rate_kwh: Option<f64>,
    gas_carrier: Option<String>,
    gas_rate_therm: Option<f64>,
    water_carrier: Option<String>,
    water_source_type: Option<String>,
    sewer_type: Option<String>,
    est_monthly_utilities: Option<i64>,
    internet_tier: Option<String>,
    fiber_available: Option<i64>,
    high_speed_internet: Option<i64>,
    internet_carriers: Option<String>,
    fiber_carriers: Option<String>,
    max_download_mbps: Option<i64>,
    max_upload_mbps: Option<i64>,
    all_in_monthly: Option<i64>,
    property_tax_rate: Option<f64>,
    has_special_tax: Option<i64>,
    has_solar: Option<i64>,
    has_ev_charger: Option<i64>,
    has_heat_pump: Option<i64>,
    has_adu: Option<i64>,
    adu_feasible: Option<i64>,
    has_pool: Option<i64>,
    garage_spaces: Option<f64>,
    architectural_style: Option<String>,
}

static GARAGE_FEAT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b([1-5](?:\.\d+)?)\s*[- ]?car\s+garage\b").unwrap());

#[allow(clippy::too_many_arguments)]
fn compute_listing_shopping_facets(
    city_opt: Option<&str>,
    state_opt: Option<&str>,
    zip: &str,
    price_opt: Option<i64>,
    sqft_opt: Option<i64>,
    lot_sqft_opt: Option<i64>,
    year_built_opt: Option<i64>,
    prop_type: &str,
    hoa_opt: Option<i64>,
    features_text: &str,
    raw_json_opt: Option<&str>,
    existing: Option<&ListingRow>,
) -> ResolvedShoppingFacets {
    use crate::registry::rates_and_costs::{
        estimate_baseline_annual_insurance, state_effective_property_tax_rate,
    };
    use crate::registry::utilities::{
        resolve_broadband_catalog, resolve_electric_utility, resolve_gas_utility,
        resolve_water_sewer_trash, UtilityLookupInput,
    };
    use crate::shared::monthly_pi;

    let sf_map = raw_json_opt
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("_shoppingFacets").cloned())
        .unwrap_or(Value::Null);

    let (zip_st, zip_fips, zip_city) = infer_geo_from_zip(zip);
    let eff_state = state_opt
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(zip_st)
        .to_ascii_uppercase();
    let eff_city = city_opt
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(zip_city);
    let eff_fips = str_from_val(sf_map.get("countyFips"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| zip_fips.to_string());

    let feat_lc = features_text.to_ascii_lowercase();
    let ptype_lc = prop_type.to_ascii_lowercase();
    let is_condo_attached = ptype_lc.contains("condo")
        || ptype_lc.contains("town")
        || ptype_lc.contains("co-op")
        || ptype_lc.contains("attached");
    let hoa_mo = hoa_opt.unwrap_or(0).max(0);
    let hoa_covers_wt = is_condo_attached && hoa_mo >= 300;
    let is_rural = lot_sqft_opt.map(|l| l >= 65_340).unwrap_or(false)
        || feat_lc.contains("septic")
        || feat_lc.contains("private well");
    let likely_well = feat_lc.contains("private well") || feat_lc.contains("well water");
    let likely_septic = feat_lc.contains("septic")
        || (lot_sqft_opt.map(|l| l >= 87_120).unwrap_or(false) && !is_condo_attached);

    let has_solar_kw = feat_lc.contains("solar")
        || feat_lc.contains("photovoltaic")
        || feat_lc.contains("powerwall");
    let has_ev_kw = feat_lc.contains("ev charger")
        || feat_lc.contains("tesla charger")
        || feat_lc.contains("level 2 charger")
        || feat_lc.contains("nema 14-50")
        || feat_lc.contains("ev charging");
    let has_hp_kw = feat_lc.contains("heat pump")
        || feat_lc.contains("mini-split")
        || feat_lc.contains("minisplit");
    let has_adu_kw = feat_lc.contains("adu")
        || feat_lc.contains("in-law unit")
        || feat_lc.contains("guest house")
        || feat_lc.contains("casita")
        || feat_lc.contains("granny flat");
    let has_pool_kw = feat_lc.contains("pool")
        && !feat_lc.contains("community pool")
        && !feat_lc.contains("no pool");

    let sqft_real = sqft_opt.filter(|&s| s > 0);
    let u_inp = UtilityLookupInput {
        state: &eff_state,
        county_fips: &eff_fips,
        county_name: "",
        city: eff_city,
        zip,
        sqft: sqft_real.unwrap_or(0),
        year_built: year_built_opt,
        is_condo_or_attached: is_condo_attached,
        hoa_covers_water_trash: hoa_covers_wt,
        is_likely_private_well: likely_well,
        is_likely_private_septic: likely_septic,
        sdwis_water_name: None,
        sdwis_pwsid: None,
        gov_kwh_benchmark: None,
        has_solar: has_solar_kw,
        acs_broadband_pct: None,
        is_rural_area: is_rural,
    };

    let elec = resolve_electric_utility(&u_inp);
    let gas = resolve_gas_utility(&u_inp);
    let wst = resolve_water_sewer_trash(&u_inp);
    let bb = resolve_broadband_catalog(&u_inp, "https://broadbandmap.fcc.gov/home", None);

    let elec_verified = elec.utility_type != "unverified" && elec.rate_per_kwh_usd > 0.0;
    let electric_carrier = str_from_val(sf_map.get("electricCarrier"))
        .or_else(|| existing.and_then(|e| e.electric_carrier.clone()))
        .or_else(|| elec_verified.then(|| elec.short_name.clone()));
    let electric_utility_type = str_from_val(sf_map.get("electricUtilityType"))
        .or_else(|| existing.and_then(|e| e.electric_utility_type.clone()))
        .or_else(|| elec_verified.then(|| elec.utility_type.clone()));
    let electric_rate_kwh = num_from_val(sf_map.get("electricRateKwh"))
        .or_else(|| existing.and_then(|e| e.electric_rate_kwh))
        .or_else(|| elec_verified.then_some(elec.rate_per_kwh_usd));

    let gas_verified = gas.heating_fuel_norm != "unverified";
    let gas_carrier = str_from_val(sf_map.get("gasCarrier"))
        .or_else(|| existing.and_then(|e| e.gas_carrier.clone()))
        .or_else(|| gas_verified.then(|| gas.carrier_name.clone()));
    let gas_rate_therm = num_from_val(sf_map.get("gasRateTherm"))
        .or_else(|| existing.and_then(|e| e.gas_rate_therm))
        .or(gas.rate_per_therm_usd);

    let water_verified = wst.water_source_type != "unverified";
    let sewer_verified = wst.sewer_type != "unverified";
    let water_carrier = str_from_val(sf_map.get("waterCarrier"))
        .or_else(|| existing.and_then(|e| e.water_carrier.clone()))
        .or_else(|| water_verified.then(|| wst.water_carrier_name.clone()));
    let water_source_type = str_from_val(sf_map.get("waterSourceType"))
        .or_else(|| existing.and_then(|e| e.water_source_type.clone()))
        .or_else(|| water_verified.then(|| wst.water_source_type.clone()));
    let sewer_type = str_from_val(sf_map.get("sewerType"))
        .or_else(|| existing.and_then(|e| e.sewer_type.clone()))
        .or_else(|| sewer_verified.then(|| wst.sewer_type.clone()));

    let modeled_utilities_mo: Option<i64> = sqft_real.and_then(|sqft_val| {
        let e_rate = electric_rate_kwh.filter(|&r| r > 0.0)?;
        let age_mult = match year_built_opt {
            Some(y) if y < 1970 => 1.18,
            Some(y) if y < 1995 => 1.08,
            Some(y) if y >= 2012 => 0.88,
            _ => 1.0,
        };
        let kwh_mo = (440.0 + (sqft_val as f64) * 0.24) * age_mult;
        let elec_usd = (kwh_mo * e_rate + elec.monthly_base_fee_usd).round() as i64;
        let gas_usd = if gas.heating_fuel_norm == "electric_heat_pump" {
            0
        } else if let Some(tr) = gas_rate_therm.filter(|&r| r > 0.0) {
            let therms_mo = (14.0 + (sqft_val as f64) * 0.011) * age_mult;
            (gas.monthly_base_fee_usd + therms_mo * tr).round() as i64
        } else {
            return None;
        };
        let wst_usd = if hoa_covers_wt {
            0
        } else if let Some(vol_rate) = wst.water_volumetric_rate_usd {
            let w_units = (3.5 + (sqft_val as f64 / 750.0)).clamp(4.0, 14.0);
            let w_bill = (wst.water_base_monthly_usd + w_units * vol_rate).round() as i64;
            w_bill + wst.sewer_monthly_usd.max(0) + wst.trash_monthly_usd.max(0)
        } else {
            return None;
        };
        Some(elec_usd + gas_usd + wst_usd + if has_pool_kw { 165 } else { 0 })
    });
    let est_monthly_utilities = int_from_val(sf_map.get("estMonthlyUtilities"))
        .or_else(|| existing.and_then(|e| e.est_monthly_utilities))
        .or(modeled_utilities_mo);

    // Only write broadband availability / speeds when address-verified (never promote unverified regional guesses as confirmed fiber).
    let bb_address_verified = bb.fiber_confirmed_fcc
        || bb.verification_status == "fcc_location_verified"
        || bb.verification_status == "address_verified";
    let fiber_available = bool_as_flag(sf_map.get("fiberAvailable"))
        .or_else(|| existing.and_then(|e| e.fiber_available))
        .or_else(|| bb_address_verified.then_some(if bb.fiber_available { 1 } else { 0 }));
    let high_speed_internet = bool_as_flag(sf_map.get("highSpeedInternet"))
        .or_else(|| existing.and_then(|e| e.high_speed_internet))
        .or_else(|| bb_address_verified.then_some(if bb.high_speed_available { 1 } else { 0 }));
    let internet_tier = str_from_val(sf_map.get("internetTier"))
        .or_else(|| existing.and_then(|e| e.internet_tier.clone()))
        .or_else(|| bb_address_verified.then(|| bb.internet_tier.clone()));
    let default_fiber_str = if bb_address_verified && !bb.fiber_carriers.is_empty() {
        Some(
            bb.fiber_carriers
                .iter()
                .map(|c| c.carrier_name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
    } else {
        None
    };
    let default_isp_str = if bb_address_verified {
        let mut names: Vec<&str> = Vec::new();
        for c in bb
            .fiber_carriers
            .iter()
            .chain(bb.cable_carriers.iter())
            .chain(bb.wireless_carriers.iter().take(1))
        {
            if !names
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&c.carrier_name))
            {
                names.push(&c.carrier_name);
            }
        }
        (!names.is_empty()).then(|| names.join(", "))
    } else {
        None
    };
    let fiber_carriers = str_from_val(sf_map.get("fiberCarriers"))
        .or_else(|| existing.and_then(|e| e.fiber_carriers.clone()))
        .or(default_fiber_str);
    let internet_carriers = str_from_val(sf_map.get("internetCarriers"))
        .or_else(|| existing.and_then(|e| e.internet_carriers.clone()))
        .or(default_isp_str);
    let max_download_mbps = int_from_val(sf_map.get("maxDownloadMbps"))
        .or_else(|| existing.and_then(|e| e.max_download_mbps))
        .or_else(|| {
            (bb_address_verified && bb.max_download_mbps > 0).then_some(bb.max_download_mbps)
        });
    let max_upload_mbps = int_from_val(sf_map.get("maxUploadMbps"))
        .or_else(|| existing.and_then(|e| e.max_upload_mbps))
        .or_else(|| (bb_address_verified && bb.max_upload_mbps > 0).then_some(bb.max_upload_mbps));

    let default_tax_rate = if eff_state.is_empty() {
        0.0110
    } else {
        state_effective_property_tax_rate(&eff_state)
    };
    let property_tax_rate = num_from_val(sf_map.get("propertyTaxRate"))
        .or_else(|| existing.and_then(|e| e.property_tax_rate))
        .or(Some(round_n(default_tax_rate, 4)));

    let special_tax_heuristic = feat_lc.contains("mello-roos")
        || feat_lc.contains("mello roos")
        || feat_lc.contains("mud tax")
        || feat_lc.contains("cdd ")
        || (eff_state == "CA"
            && year_built_opt.map(|y| y >= 2002).unwrap_or(false)
            && !is_condo_attached)
        || (eff_state == "TX" && year_built_opt.map(|y| y >= 2004).unwrap_or(false));
    let has_special_tax = bool_as_flag(sf_map.get("hasSpecialTax"))
        .or_else(|| existing.and_then(|e| e.has_special_tax))
        .or(Some(if special_tax_heuristic { 1 } else { 0 }));

    let all_in_monthly = int_from_val(sf_map.get("allInMonthly"))
        .or_else(|| existing.and_then(|e| e.all_in_monthly))
        .or_else(|| {
            let p = price_opt.filter(|&x| x > 0)? as f64;
            let pi = monthly_pi(p * 0.80, 6.65, 30);
            let tx_mo = (p * property_tax_rate.unwrap_or(default_tax_rate)) / 12.0;
            let ins_mo = estimate_baseline_annual_insurance(
                p,
                sqft_real.map(|s| s as f64),
                &eff_state,
                if eff_fips.is_empty() {
                    None
                } else {
                    Some(eff_fips.as_str())
                },
            ) / 12.0;
            let sp_mo = if has_special_tax == Some(1) {
                185.0
            } else {
                0.0
            };
            let util_mo = est_monthly_utilities.unwrap_or(0) as f64;
            Some((pi + tx_mo + ins_mo + (hoa_mo as f64) + sp_mo + util_mo).round() as i64)
        });

    let has_solar = bool_as_flag(sf_map.get("hasSolar"))
        .or_else(|| existing.and_then(|e| e.has_solar))
        .or(Some(if has_solar_kw { 1 } else { 0 }));
    let has_ev_charger = bool_as_flag(sf_map.get("hasEvCharger"))
        .or_else(|| existing.and_then(|e| e.has_ev_charger))
        .or(Some(if has_ev_kw { 1 } else { 0 }));
    let has_heat_pump = bool_as_flag(sf_map.get("hasHeatPump"))
        .or_else(|| existing.and_then(|e| e.has_heat_pump))
        .or(Some(
            if has_hp_kw || gas.heating_fuel_norm == "electric_heat_pump" {
                1
            } else {
                0
            },
        ));
    let has_adu = bool_as_flag(sf_map.get("hasAdu"))
        .or_else(|| existing.and_then(|e| e.has_adu))
        .or(Some(if has_adu_kw { 1 } else { 0 }));
    let adu_state_permissive = matches!(
        eff_state.as_str(),
        "CA" | "WA" | "OR" | "TX" | "FL" | "CO" | "AZ" | "MA" | "NY" | "UT"
    );
    let adu_lot_ok = !is_condo_attached
        && lot_sqft_opt.map(|l| l >= 4_500).unwrap_or(false)
        && adu_state_permissive;
    let adu_feasible = bool_as_flag(sf_map.get("aduFeasible"))
        .or_else(|| existing.and_then(|e| e.adu_feasible))
        .or(Some(if has_adu == Some(1) || adu_lot_ok {
            1
        } else {
            0
        }));
    let has_pool = bool_as_flag(sf_map.get("hasPool"))
        .or_else(|| existing.and_then(|e| e.has_pool))
        .or(Some(if has_pool_kw { 1 } else { 0 }));

    let inferred_garage = GARAGE_FEAT_RE
        .captures(features_text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
        .or_else(|| {
            if feat_lc.contains("garage") {
                Some(if is_condo_attached { 1.0 } else { 2.0 })
            } else {
                None
            }
        });
    let garage_spaces = num_from_val(sf_map.get("garageSpaces"))
        .or_else(|| existing.and_then(|e| e.garage_spaces))
        .or(inferred_garage);

    let inferred_style = [
        ("mid-century", "Mid-Century Modern"),
        ("eichler", "Mid-Century Modern"),
        ("craftsman", "Craftsman"),
        ("contemporary", "Contemporary"),
        ("modern", "Modern"),
        ("victorian", "Victorian"),
        ("tudor", "Tudor"),
        ("mediterranean", "Mediterranean"),
        ("spanish", "Spanish"),
        ("colonial", "Colonial"),
        ("cape cod", "Cape Cod"),
        ("farmhouse", "Farmhouse"),
        ("ranch", "Ranch"),
        ("traditional", "Traditional"),
    ]
    .into_iter()
    .find_map(|(needle, label)| feat_lc.contains(needle).then_some(label.to_string()));
    let architectural_style = str_from_val(sf_map.get("architecturalStyle"))
        .or_else(|| existing.and_then(|e| e.architectural_style.clone()))
        .or(inferred_style);

    ResolvedShoppingFacets {
        inferred_state: (!eff_state.is_empty()).then_some(eff_state),
        electric_carrier,
        electric_utility_type,
        electric_rate_kwh,
        gas_carrier,
        gas_rate_therm,
        water_carrier,
        water_source_type,
        sewer_type,
        est_monthly_utilities,
        internet_tier,
        fiber_available,
        high_speed_internet,
        internet_carriers,
        fiber_carriers,
        max_download_mbps,
        max_upload_mbps,
        all_in_monthly,
        property_tax_rate,
        has_special_tax,
        has_solar,
        has_ev_charger,
        has_heat_pump,
        has_adu,
        adu_feasible,
        has_pool,
        garage_spaces,
        architectural_style,
    }
}

// ---------------------------------------------------------------- Database Upsert & Event Diff Engine
pub async fn upsert_listings_batch(
    db: &DbStore,
    http: Option<&HttpClient>,
    env: &EnvConfig,
    raw_items: &[ListingInput],
    emit_events: bool,
) -> Result<UpsertBatchResult> {
    db.ensure_schema().await?;
    if raw_items.is_empty() {
        return Ok(UpsertBatchResult::default());
    }

    // Deduplicate incoming items by canonical ID while preserving order
    let mut order_ids: Vec<String> = Vec::new();
    let mut merged_by_id: HashMap<String, ListingInput> = HashMap::new();
    for raw in raw_items {
        let Some(zip) = ZIP5_RE
            .captures(&raw.zip)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
        else {
            continue;
        };
        if raw.address.trim().is_empty() {
            continue;
        }
        let id = raw
            .id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                listing_slug(
                    raw.street.as_deref().unwrap_or(&raw.address),
                    raw.city.as_deref(),
                    raw.state.as_deref(),
                    Some(&zip),
                )
            });
        if id.is_empty() {
            continue;
        }
        if let Some(prev) = merged_by_id.get_mut(&id) {
            prev.merge_non_empty(raw);
            prev.id = Some(id);
            prev.zip = zip;
        } else {
            let mut next = raw.clone();
            next.id = Some(id.clone());
            next.zip = zip;
            order_ids.push(id.clone());
            merged_by_id.insert(id, next);
        }
    }

    let mut unique_zips: Vec<String> = Vec::new();
    for id in &order_ids {
        if let Some(it) = merged_by_id.get(id) {
            if !unique_zips.contains(&it.zip) {
                unique_zips.push(it.zip.clone());
            }
        }
    }

    let mut school_by_zip: HashMap<String, ZipSchoolContext> = HashMap::new();
    for z in unique_zips.iter() {
        let needs_schools = merged_by_id
            .values()
            .any(|x| &x.zip == z && (x.school_score.is_none() || x.school_grade.is_none()));
        if needs_schools {
            let sample = merged_by_id
                .values()
                .find(|x| &x.zip == z && x.lat.is_some() && x.lon.is_some());
            if let Ok(ctx) = get_zip_school_context(
                db,
                http,
                env,
                z,
                sample.and_then(|s| s.lat),
                sample.and_then(|s| s.lon),
            )
            .await
            {
                school_by_zip.insert(z.clone(), ctx);
            }
        }
    }

    let mut existing_map: HashMap<String, ListingRow> = HashMap::new();
    for z in &unique_zips {
        let rows = db
            .fetch_listings_sql(
                "SELECT * FROM listings WHERE zip = ?",
                &[SqlParam::from(z.as_str())],
            )
            .await?;
        for r in rows {
            existing_map.insert(r.id.clone(), r);
        }
    }

    let now = now_iso();
    let mut res = UpsertBatchResult::default();

    for id in order_ids {
        let Some(mut item) = merged_by_id.remove(&id) else {
            continue;
        };
        let sch_ctx = school_by_zip.get(&item.zip);
        assign_schools_to_listing(&mut item, sch_ctx);
        let existing = existing_map.get(&id);

        let status = normalize_listing_status(
            item.status
                .as_deref()
                .or_else(|| existing.map(|e| e.status.as_str())),
        )
        .to_string();
        let price = item.price.or_else(|| existing.and_then(|e| e.price));
        let sqft = item.sqft.or_else(|| existing.and_then(|e| e.sqft));
        let ppsf = match (price, sqft) {
            (Some(p), Some(s)) if p > 0 && s > 0 => Some(((p as f64) / (s as f64)).round() as i64),
            _ => existing.and_then(|e| e.price_per_sqft),
        };

        let mut event_type: Option<&'static str> = None;
        if let Some(ex) = existing {
            if ex.status != status {
                if status == "sold" {
                    event_type = Some("sold");
                } else if status == "for_sale" {
                    event_type = Some("back_on_market");
                }
            } else if status == "for_sale" {
                if let (Some(new_p), Some(old_p)) = (price, ex.price) {
                    if (new_p - old_p).abs() >= 1000 {
                        event_type = Some("price_change");
                    }
                }
            }
        } else if status == "for_sale" || status == "pending" {
            event_type = Some("new_listing");
        } else if status == "sold" {
            event_type = Some("sold");
        }

        let address = if !item.address.trim().is_empty() {
            item.address.clone()
        } else {
            existing.map(|e| e.address.clone()).unwrap_or_default()
        };
        let street = item
            .street
            .clone()
            .or_else(|| existing.and_then(|e| e.street.clone()))
            .or_else(|| {
                let s = address.split(',').next().unwrap_or("").trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            });
        let report_slug = item
            .report_slug
            .clone()
            .or_else(|| existing.and_then(|e| e.report_slug.clone()));
        let url = if let Some(ref rs) = report_slug {
            Some(format!("/r/{}/", urlencoding::encode(rs)))
        } else {
            item.url
                .clone()
                .or_else(|| existing.and_then(|e| e.url.clone()))
                .or_else(|| Some(format!("/report?address={}", urlencoding::encode(&address))))
        };
        let school_score = item
            .school_score
            .or_else(|| existing.and_then(|e| e.school_score));
        let school_grade = item
            .school_grade
            .clone()
            .or_else(|| existing.and_then(|e| e.school_grade.clone()))
            .or_else(|| score_to_school_grade(school_score));

        let merged_city = item
            .city
            .clone()
            .or_else(|| existing.and_then(|e| e.city.clone()));
        let merged_state = item
            .state
            .clone()
            .or_else(|| existing.and_then(|e| e.state.clone()));
        let merged_lot_sqft = item.lot_sqft.or_else(|| existing.and_then(|e| e.lot_sqft));
        let merged_year_built = item
            .year_built
            .or_else(|| existing.and_then(|e| e.year_built));
        let merged_prop_type = if let Some(ref pt) = item.property_type {
            normalize_property_category(Some(pt))
        } else {
            existing
                .and_then(|e| e.property_type.clone())
                .unwrap_or_else(|| "Single-Family Detached".to_string())
        };
        let merged_hoa = if item.hoa_monthly.is_some() {
            item.hoa_monthly
        } else {
            existing.and_then(|e| e.hoa_monthly)
        };
        let merged_features = item
            .features_text
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| existing.and_then(|e| e.features_text.clone()))
            .unwrap_or_default();
        let merged_raw_json = item
            .raw_json
            .clone()
            .or_else(|| existing.and_then(|e| e.raw_json.clone()));
        let merged_est_val = item
            .estimated_value
            .or_else(|| existing.and_then(|e| e.estimated_value));

        let sf = compute_listing_shopping_facets(
            merged_city.as_deref(),
            merged_state.as_deref(),
            &item.zip,
            price.or(merged_est_val),
            sqft,
            merged_lot_sqft,
            merged_year_built,
            &merged_prop_type,
            merged_hoa,
            &merged_features,
            merged_raw_json.as_deref(),
            existing,
        );

        let merged_noise_db = item
            .noise_max_db
            .or_else(|| existing.and_then(|e| e.noise_max_db));
        let merged_seismic_pga = item
            .seismic_pga
            .or_else(|| existing.and_then(|e| e.seismic_pga));
        let merged_safety = item
            .safety_score
            .or_else(|| existing.and_then(|e| e.safety_score));
        let mut existing_tags: Vec<String> = existing
            .and_then(|e| e.tags_json.as_deref())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default();
        if let Some(ref raw_tags) = item.tags_json {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(raw_tags) {
                existing_tags.extend(arr);
            }
        }
        if sf.has_solar == Some(1) {
            existing_tags.push("solar".to_string());
        }
        if sf.has_ev_charger == Some(1) {
            existing_tags.push("ev_charger".to_string());
        }
        if sf.has_heat_pump == Some(1) {
            existing_tags.push("heat_pump".to_string());
        }
        if sf.has_adu == Some(1) {
            existing_tags.push("adu".to_string());
        }
        if sf.has_pool == Some(1) {
            existing_tags.push("pool".to_string());
        }
        if sf.fiber_available == Some(1) {
            existing_tags.push("fiber".to_string());
        }
        let (inf_hw, inf_kit, inf_rem, inf_quiet, merged_tags) = infer_feature_tags_and_flags(
            &merged_features.to_ascii_lowercase(),
            merged_noise_db,
            merged_seismic_pga,
            merged_safety,
            &existing_tags,
        );
        let merged_hw = item
            .has_hardwood_floors
            .or_else(|| existing.and_then(|e| e.has_hardwood_floors))
            .or(inf_hw);
        let merged_kit = item
            .has_updated_kitchen
            .or_else(|| existing.and_then(|e| e.has_updated_kitchen))
            .or(inf_kit);
        let merged_rem = item
            .has_remodeled
            .or_else(|| existing.and_then(|e| e.has_remodeled))
            .or(inf_rem);
        let merged_quiet = item
            .quiet_street
            .or_else(|| existing.and_then(|e| e.quiet_street))
            .or(inf_quiet);
        let merged_tags_json = if merged_tags.is_empty() {
            existing.and_then(|e| e.tags_json.clone())
        } else {
            serde_json::to_string(&merged_tags).ok()
        };

        let row_record = ListingRow {
            id: id.clone(),
            address,
            street,
            city: merged_city,
            state: merged_state.or(sf.inferred_state),
            zip: item.zip,
            lat: item.lat.or_else(|| existing.and_then(|e| e.lat)),
            lon: item.lon.or_else(|| existing.and_then(|e| e.lon)),
            status: status.clone(),
            price,
            list_price: item
                .list_price
                .or_else(|| existing.and_then(|e| e.list_price))
                .or_else(|| if status == "for_sale" { price } else { None }),
            sold_price: item
                .sold_price
                .or_else(|| existing.and_then(|e| e.sold_price))
                .or_else(|| if status == "sold" { price } else { None }),
            estimated_value: merged_est_val,
            price_per_sqft: ppsf,
            beds: item.beds.or_else(|| existing.and_then(|e| e.beds)),
            baths: item.baths.or_else(|| existing.and_then(|e| e.baths)),
            sqft,
            lot_sqft: merged_lot_sqft,
            year_built: merged_year_built,
            stories: item.stories.or_else(|| existing.and_then(|e| e.stories)),
            property_type: Some(merged_prop_type),
            hoa_monthly: merged_hoa,
            days_on_market: item
                .days_on_market
                .or_else(|| existing.and_then(|e| e.days_on_market)),
            listed_date: item
                .listed_date
                .or_else(|| existing.and_then(|e| e.listed_date.clone())),
            sold_date: item
                .sold_date
                .or_else(|| existing.and_then(|e| e.sold_date.clone())),
            school_score,
            school_grade,
            school_elem_name: item
                .school_elem_name
                .or_else(|| existing.and_then(|e| e.school_elem_name.clone())),
            school_elem_rating: item
                .school_elem_rating
                .or_else(|| existing.and_then(|e| e.school_elem_rating)),
            school_mid_name: item
                .school_mid_name
                .or_else(|| existing.and_then(|e| e.school_mid_name.clone())),
            school_mid_rating: item
                .school_mid_rating
                .or_else(|| existing.and_then(|e| e.school_mid_rating)),
            school_high_name: item
                .school_high_name
                .or_else(|| existing.and_then(|e| e.school_high_name.clone())),
            school_high_rating: item
                .school_high_rating
                .or_else(|| existing.and_then(|e| e.school_high_rating)),
            school_district: item
                .school_district
                .or_else(|| existing.and_then(|e| e.school_district.clone())),
            walk_score: item
                .walk_score
                .or_else(|| existing.and_then(|e| e.walk_score)),
            flood_zone: item
                .flood_zone
                .or_else(|| existing.and_then(|e| e.flood_zone.clone())),
            wildfire_zone: item
                .wildfire_zone
                .or_else(|| existing.and_then(|e| e.wildfire_zone.clone())),
            overall_score: item
                .overall_score
                .or_else(|| existing.and_then(|e| e.overall_score)),
            verdict: item
                .verdict
                .or_else(|| existing.and_then(|e| e.verdict.clone())),
            mls_number: item
                .mls_number
                .or_else(|| existing.and_then(|e| e.mls_number.clone())),
            source: Some(
                item.source
                    .or_else(|| existing.and_then(|e| e.source.clone()))
                    .unwrap_or_else(|| "Open Estate".to_string()),
            ),
            url,
            photo_url: item
                .photo_url
                .or_else(|| existing.and_then(|e| e.photo_url.clone())),
            report_slug,
            features_text: Some(merged_features),
            raw_json: merged_raw_json,
            electric_carrier: sf.electric_carrier,
            electric_utility_type: sf.electric_utility_type,
            electric_rate_kwh: sf.electric_rate_kwh,
            gas_carrier: sf.gas_carrier,
            gas_rate_therm: sf.gas_rate_therm,
            water_carrier: sf.water_carrier,
            water_source_type: sf.water_source_type,
            sewer_type: sf.sewer_type,
            est_monthly_utilities: sf.est_monthly_utilities,
            internet_tier: sf.internet_tier,
            fiber_available: sf.fiber_available,
            high_speed_internet: sf.high_speed_internet,
            internet_carriers: sf.internet_carriers,
            fiber_carriers: sf.fiber_carriers,
            max_download_mbps: sf.max_download_mbps,
            max_upload_mbps: sf.max_upload_mbps,
            all_in_monthly: sf.all_in_monthly,
            property_tax_rate: sf.property_tax_rate,
            has_special_tax: sf.has_special_tax,
            has_solar: sf.has_solar,
            has_ev_charger: sf.has_ev_charger,
            has_heat_pump: sf.has_heat_pump,
            has_adu: sf.has_adu,
            adu_feasible: sf.adu_feasible,
            has_pool: sf.has_pool,
            garage_spaces: sf.garage_spaces,
            architectural_style: sf.architectural_style,
            safety_score: merged_safety,
            has_hardwood_floors: merged_hw,
            has_updated_kitchen: merged_kit,
            has_remodeled: merged_rem,
            quiet_street: merged_quiet,
            noise_max_db: merged_noise_db,
            seismic_pga: merged_seismic_pga,
            tags_json: merged_tags_json,
            first_seen_at: existing
                .map(|e| e.first_seen_at.clone())
                .unwrap_or_else(|| now.clone()),
            last_seen_at: now.clone(),
            status_changed_at: match existing {
                Some(e) if e.status == status => e.status_changed_at.clone(),
                _ => now.clone(),
            },
            price_changed_at: if event_type == Some("price_change") {
                Some(now.clone())
            } else {
                existing.and_then(|e| e.price_changed_at.clone())
            },
        };

        db.upsert_row(
            "listings",
            &[
                ("id", row_record.id.clone().into()),
                ("address", row_record.address.clone().into()),
                ("street", row_record.street.clone().into()),
                ("city", row_record.city.clone().into()),
                ("state", row_record.state.clone().into()),
                ("zip", row_record.zip.clone().into()),
                ("lat", row_record.lat.into()),
                ("lon", row_record.lon.into()),
                ("status", row_record.status.clone().into()),
                ("price", row_record.price.into()),
                ("list_price", row_record.list_price.into()),
                ("sold_price", row_record.sold_price.into()),
                ("estimated_value", row_record.estimated_value.into()),
                ("price_per_sqft", row_record.price_per_sqft.into()),
                ("beds", row_record.beds.into()),
                ("baths", row_record.baths.into()),
                ("sqft", row_record.sqft.into()),
                ("lot_sqft", row_record.lot_sqft.into()),
                ("year_built", row_record.year_built.into()),
                ("stories", row_record.stories.into()),
                ("property_type", row_record.property_type.clone().into()),
                ("hoa_monthly", row_record.hoa_monthly.into()),
                ("days_on_market", row_record.days_on_market.into()),
                ("listed_date", row_record.listed_date.clone().into()),
                ("sold_date", row_record.sold_date.clone().into()),
                ("school_score", row_record.school_score.into()),
                ("school_grade", row_record.school_grade.clone().into()),
                (
                    "school_elem_name",
                    row_record.school_elem_name.clone().into(),
                ),
                ("school_elem_rating", row_record.school_elem_rating.into()),
                ("school_mid_name", row_record.school_mid_name.clone().into()),
                ("school_mid_rating", row_record.school_mid_rating.into()),
                (
                    "school_high_name",
                    row_record.school_high_name.clone().into(),
                ),
                ("school_high_rating", row_record.school_high_rating.into()),
                ("school_district", row_record.school_district.clone().into()),
                ("walk_score", row_record.walk_score.into()),
                ("flood_zone", row_record.flood_zone.clone().into()),
                ("wildfire_zone", row_record.wildfire_zone.clone().into()),
                ("overall_score", row_record.overall_score.into()),
                ("verdict", row_record.verdict.clone().into()),
                ("mls_number", row_record.mls_number.clone().into()),
                ("source", row_record.source.clone().into()),
                ("url", row_record.url.clone().into()),
                ("photo_url", row_record.photo_url.clone().into()),
                ("report_slug", row_record.report_slug.clone().into()),
                ("features_text", row_record.features_text.clone().into()),
                ("raw_json", row_record.raw_json.clone().into()),
                (
                    "electric_carrier",
                    row_record.electric_carrier.clone().into(),
                ),
                (
                    "electric_utility_type",
                    row_record.electric_utility_type.clone().into(),
                ),
                ("electric_rate_kwh", row_record.electric_rate_kwh.into()),
                ("gas_carrier", row_record.gas_carrier.clone().into()),
                ("gas_rate_therm", row_record.gas_rate_therm.into()),
                ("water_carrier", row_record.water_carrier.clone().into()),
                (
                    "water_source_type",
                    row_record.water_source_type.clone().into(),
                ),
                ("sewer_type", row_record.sewer_type.clone().into()),
                (
                    "est_monthly_utilities",
                    row_record.est_monthly_utilities.into(),
                ),
                ("internet_tier", row_record.internet_tier.clone().into()),
                ("fiber_available", row_record.fiber_available.into()),
                ("high_speed_internet", row_record.high_speed_internet.into()),
                (
                    "internet_carriers",
                    row_record.internet_carriers.clone().into(),
                ),
                ("fiber_carriers", row_record.fiber_carriers.clone().into()),
                ("max_download_mbps", row_record.max_download_mbps.into()),
                ("max_upload_mbps", row_record.max_upload_mbps.into()),
                ("all_in_monthly", row_record.all_in_monthly.into()),
                ("property_tax_rate", row_record.property_tax_rate.into()),
                ("has_special_tax", row_record.has_special_tax.into()),
                ("has_solar", row_record.has_solar.into()),
                ("has_ev_charger", row_record.has_ev_charger.into()),
                ("has_heat_pump", row_record.has_heat_pump.into()),
                ("has_adu", row_record.has_adu.into()),
                ("adu_feasible", row_record.adu_feasible.into()),
                ("has_pool", row_record.has_pool.into()),
                ("garage_spaces", row_record.garage_spaces.into()),
                (
                    "architectural_style",
                    row_record.architectural_style.clone().into(),
                ),
                ("safety_score", row_record.safety_score.into()),
                ("has_hardwood_floors", row_record.has_hardwood_floors.into()),
                ("has_updated_kitchen", row_record.has_updated_kitchen.into()),
                ("has_remodeled", row_record.has_remodeled.into()),
                ("quiet_street", row_record.quiet_street.into()),
                ("noise_max_db", row_record.noise_max_db.into()),
                ("seismic_pga", row_record.seismic_pga.into()),
                ("tags_json", row_record.tags_json.clone().into()),
                ("first_seen_at", row_record.first_seen_at.clone().into()),
                ("last_seen_at", row_record.last_seen_at.clone().into()),
                (
                    "status_changed_at",
                    row_record.status_changed_at.clone().into(),
                ),
                (
                    "price_changed_at",
                    row_record.price_changed_at.clone().into(),
                ),
            ],
            &["id"],
        )
        .await?;
        res.upserted += 1;

        if emit_events {
            if let Some(ev) = event_type {
                let old_status = existing.map(|e| e.status.clone());
                let old_price = existing.and_then(|e| e.price);
                db.insert_ignore_row(
                    "listing_events",
                    &[
                        ("id", Uuid::new_v4().to_string().into()),
                        ("listing_id", row_record.id.clone().into()),
                        ("zip", row_record.zip.clone().into()),
                        ("event_type", ev.into()),
                        ("old_status", old_status.clone().into()),
                        ("new_status", status.clone().into()),
                        ("old_price", old_price.into()),
                        ("new_price", price.into()),
                        ("created_at", now.clone().into()),
                    ],
                )
                .await?;

                let formatted = format_listing_row(&row_record);
                if ev == "new_listing" || ev == "back_on_market" {
                    res.new_listings.push(formatted.clone());
                } else if ev == "sold" {
                    res.sold_listings.push(formatted.clone());
                } else if ev == "price_change" {
                    res.price_changes.push(formatted.clone());
                }
                res.changed_listings.push(ListingChangeEvent {
                    event_type: ev.to_string(),
                    old_status,
                    old_price,
                    listing: row_record,
                    listing_json: formatted,
                });
            }
        }
    }

    Ok(res)
}

/// Idempotently populates the 27 structured shopping/utility/fiber columns on any pre-existing
/// `listings` rows where `electric_carrier IS NULL` (e.g., rows inserted prior to schema upgrade).
pub async fn backfill_missing_shopping_facets(db: &DbStore) -> Result<usize> {
    db.ensure_schema().await?;
    let rows = db
        .fetch_listings_sql(
            "SELECT * FROM listings WHERE electric_carrier IS NULL OR fiber_available IS NULL LIMIT 400",
            &[],
        )
        .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let sql = "UPDATE listings SET
        state = COALESCE(state, ?),
        electric_carrier = ?, electric_utility_type = ?, electric_rate_kwh = ?,
        gas_carrier = ?, gas_rate_therm = ?,
        water_carrier = ?, water_source_type = ?, sewer_type = ?,
        est_monthly_utilities = ?,
        internet_tier = ?, fiber_available = ?, high_speed_internet = ?,
        internet_carriers = ?, fiber_carriers = ?, max_download_mbps = ?, max_upload_mbps = ?,
        all_in_monthly = ?, property_tax_rate = ?, has_special_tax = ?,
        has_solar = ?, has_ev_charger = ?, has_heat_pump = ?,
        has_adu = ?, adu_feasible = ?, has_pool = ?,
        garage_spaces = ?, architectural_style = ?
        WHERE id = ?";
    let mut updated = 0_usize;
    for r in &rows {
        let pt = r
            .property_type
            .as_deref()
            .unwrap_or("Single-Family Detached");
        let ft = r.features_text.as_deref().unwrap_or("");
        let sf = compute_listing_shopping_facets(
            r.city.as_deref(),
            r.state.as_deref(),
            &r.zip,
            r.price.or(r.estimated_value),
            r.sqft,
            r.lot_sqft,
            r.year_built,
            pt,
            r.hoa_monthly,
            ft,
            r.raw_json.as_deref(),
            Some(r),
        );
        db.execute_params(
            sql,
            &[
                sf.inferred_state.into(),
                sf.electric_carrier.into(),
                sf.electric_utility_type.into(),
                sf.electric_rate_kwh.into(),
                sf.gas_carrier.into(),
                sf.gas_rate_therm.into(),
                sf.water_carrier.into(),
                sf.water_source_type.into(),
                sf.sewer_type.into(),
                sf.est_monthly_utilities.into(),
                sf.internet_tier.into(),
                sf.fiber_available.into(),
                sf.high_speed_internet.into(),
                sf.internet_carriers.into(),
                sf.fiber_carriers.into(),
                sf.max_download_mbps.into(),
                sf.max_upload_mbps.into(),
                sf.all_in_monthly.into(),
                sf.property_tax_rate.into(),
                sf.has_special_tax.into(),
                sf.has_solar.into(),
                sf.has_ev_charger.into(),
                sf.has_heat_pump.into(),
                sf.has_adu.into(),
                sf.adu_feasible.into(),
                sf.has_pool.into(),
                sf.garage_spaces.into(),
                sf.architectural_style.into(),
                r.id.clone().into(),
            ],
        )
        .await?;
        updated += 1;
    }
    Ok(updated)
}

pub async fn ingest_report_into_db(
    db: &DbStore,
    http: Option<&HttpClient>,
    env: &EnvConfig,
    report: &Value,
    emit_events: bool,
) -> Result<UpsertBatchResult> {
    let mut extracted = extract_listings_from_report(report);
    if extracted.is_empty() {
        return Ok(UpsertBatchResult::default());
    }

    // Optional lightweight LLM tag extraction strictly gated on `LLM_REPORT_TAGGING=true`
    // and constrained to verified report facts; defaults strictly to deterministic facts.
    let llm_tagging = env
        .get("LLM_REPORT_TAGGING")
        .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if llm_tagging && env.llm_configured() {
        if let (Some(client), Some(subj)) = (http, extracted.first_mut()) {
            let brief = json!({
                "features": subj.features_text,
                "aboutHome": report.pointer("/modules/aiAnalyst/data/aboutHome"),
                "renovations": report.pointer("/facts/renovations"),
            });
            let schema = json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["tags"],
                "properties": {
                    "tags": {
                        "type": "array",
                        "maxItems": 10,
                        "items": { "type": "string" }
                    }
                }
            });
            let cache = crate::ai::llm::DbLlmCache(db.clone());
            let opts = crate::ai::llm::LlmCallOptions::new(
                "report_tagging",
                "report_tags",
                "Extract concise snake_case property feature tags strictly grounded in the supplied JSON facts (no invented features).",
                serde_json::to_string(&brief).unwrap_or_default(),
                schema,
            );
            if let Ok(resp) =
                crate::ai::llm::responses_json_with_cache(client, env, opts, Some(&cache)).await
            {
                if let Some(arr) = resp.data.get("tags").and_then(Value::as_array) {
                    let mut merged: Vec<String> = subj
                        .tags_json
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                        .unwrap_or_default();
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            let clean = s.trim().to_ascii_lowercase().replace(' ', "_");
                            if !clean.is_empty() && !merged.contains(&clean) {
                                merged.push(clean);
                            }
                        }
                    }
                    merged.sort();
                    if !merged.is_empty() {
                        subj.tags_json = serde_json::to_string(&merged).ok();
                    }
                }
            }
        }
    }

    let res = upsert_listings_batch(db, http, env, &extracted, emit_events).await?;

    // Register the report's 5-digit ZIP in `watched_zips` (default daily 1440-min interval)
    // so when `AUTO_SYNC_ENABLED=true` is configured, background daily sync keeps its comps fresh.
    if let Some(first) = extracted.first().filter(|x| x.zip.len() == 5) {
        if let Ok(None) = db.get_watched_zip(&first.zip).await {
            let (act_cnt, sld_cnt) = db
                .count_listings_by_status_for_zip(&first.zip)
                .await
                .unwrap_or((0, 0));
            let _ = db
                .upsert_row(
                    "watched_zips",
                    &[
                        ("zip", first.zip.clone().into()),
                        ("city", first.city.clone().into()),
                        ("state", first.state.clone().into()),
                        ("lat", first.lat.into()),
                        ("lon", first.lon.into()),
                        ("active", 1_i64.into()),
                        ("sync_interval_min", 1440_i64.into()),
                        ("total_active_listings", act_cnt.into()),
                        ("total_sold_listings", sld_cnt.into()),
                        ("created_at", now_iso().into()),
                    ],
                    &["zip"],
                )
                .await;
        }
    }

    if let Some(slug) = str_from_val(report.pointer("/meta/slug")).filter(|s| !s.is_empty()) {
        if let Ok(Some(rec)) = db.get_cached_report(&slug).await {
            let zip = str_from_val(report.pointer("/geo/zip"))
                .or_else(|| str_from_val(report.pointer("/facts/zip")))
                .unwrap_or_default();
            if zip.len() == 5 {
                if let Ok(rows) = db
                    .query_listings_in_areas(std::slice::from_ref(&zip), &[], None, 500)
                    .await
                {
                    let mut to_upsert = Vec::new();
                    let mut backfills = Vec::new();
                    for l in &rows {
                        if l.report_slug.as_deref() == Some(slug.as_str()) {
                            let (_f, lf_row, maybe_bf) =
                                crate::search::light_scan::hydrate_from_cached_report(l, &rec);
                            to_upsert.push(lf_row);
                            if let Some(bf) = maybe_bf {
                                backfills.push(bf);
                            }
                        }
                    }
                    if !to_upsert.is_empty() {
                        let _ = db.upsert_listing_factors_batch(&to_upsert).await;
                    }
                    if !backfills.is_empty() {
                        let _ = db.backfill_listing_summary_fields(&backfills).await;
                    }
                }
            }
        }
    }
    Ok(res)
}

pub async fn seed_reports_from_dir(db: &DbStore, reports_dir: &Path) -> Result<usize> {
    db.ensure_schema().await?;
    if !reports_dir.exists() {
        return Ok(0);
    }
    let mut entries = match tokio::fs::read_dir(reports_dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(0),
    };
    let env = EnvConfig::default();
    let mut total = 0_usize;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let (slug, report_path, html_path_opt) = if path.is_dir() {
            let slug = entry.file_name().to_string_lossy().into_owned();
            let report_path = path.join("report.json");
            if !report_path.exists() {
                continue;
            }
            (slug, report_path, Some(path.join("index.html")))
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            (stem, path.clone(), None)
        } else {
            continue;
        };
        let Ok(raw) = tokio::fs::read_to_string(&report_path).await else {
            continue;
        };
        let Ok(rep) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let target_slug = str_from_val(rep.pointer("/meta/slug")).unwrap_or(slug);
        if db.get_cached_report(&target_slug).await?.is_some() {
            // Never overwrite an already-cached (or manually refreshed) database report on startup.
            continue;
        }
        let rendered_html = match html_path_opt {
            Some(ref hp) => tokio::fs::read_to_string(hp).await.ok(),
            None => None,
        };
        db.put_cached_report(&target_slug, &rep, rendered_html.as_deref())
            .await?;
        let batch = ingest_report_into_db(db, None, &env, &rep, false).await?;
        total += batch.upserted;
    }
    Ok(total)
}

// ---------------------------------------------------------------- Pluggable External ZIP Connectors
struct ConnectorFeedOutput {
    items: Vec<ListingInput>,
    warnings: Vec<String>,
    source_name: Option<String>,
}

async fn fetch_reso_zip(http: &HttpClient, zip: &str, env: &EnvConfig) -> ConnectorFeedOutput {
    let base = env
        .get_or("RESO_API_URL", "")
        .trim_end_matches('/')
        .to_string();
    let token = env.get_or("RESO_BEARER_TOKEN", "");
    let filter = format!(
        "PostalCode eq '{}' and (StandardStatus eq 'Active' or StandardStatus eq 'Pending' or StandardStatus eq 'Closed')",
        zip.replace('\'', "''")
    );
    let auth = format!("Bearer {token}");
    let hdrs = [
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
    ];

    let Ok(mut url) = Url::parse(&format!("{base}/Property")) else {
        return ConnectorFeedOutput {
            items: Vec::new(),
            warnings: vec!["Invalid RESO_API_URL".to_string()],
            source_name: None,
        };
    };
    url.query_pairs_mut()
        .append_pair("$filter", &filter)
        .append_pair("$top", "80");

    let j = match http
        .get_json_opts(url.as_str(), 18000, 1, 300, Some(&hdrs))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return ConnectorFeedOutput {
                items: Vec::new(),
                warnings: vec![format!("RESO ZIP query failed: {e}")],
                source_name: None,
            }
        }
    };

    let rows = j
        .get("value")
        .or_else(|| j.get("bundle"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut items = Vec::new();
    for p in rows {
        let street = str_from_val(p.get("UnparsedAddress")).or_else(|| {
            let joined = [
                str_from_val(p.get("StreetNumber")),
                str_from_val(p.get("StreetName")),
                str_from_val(p.get("StreetSuffix")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        });
        let Some(street) = street else {
            continue;
        };
        let status = normalize_listing_status(
            str_from_val(p.get("StandardStatus"))
                .or_else(|| str_from_val(p.get("MlsStatus")))
                .as_deref(),
        );
        let list_price = int_from_val(p.get("ListPrice"));
        let sold_price = int_from_val(p.get("ClosePrice"));
        let city = str_from_val(p.get("City"));
        let state = str_from_val(p.get("StateOrProvince"));
        let z: String = str_from_val(p.get("PostalCode"))
            .unwrap_or_else(|| zip.to_string())
            .chars()
            .take(5)
            .collect();
        let address = [
            Some(street.as_str()),
            city.as_deref(),
            Some(format!("{} {}", state.as_deref().unwrap_or(""), z).trim()),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");

        items.push(ListingInput {
            street: Some(street),
            address,
            city,
            state,
            zip: z,
            lat: num_from_val(p.get("Latitude")),
            lon: num_from_val(p.get("Longitude")),
            status: Some(status.to_string()),
            price: if status == "sold" {
                sold_price.or(list_price)
            } else {
                list_price.or(sold_price)
            },
            list_price,
            sold_price,
            beds: num_from_val(p.get("BedroomsTotal")),
            baths: num_from_val(p.get("BathroomsTotalDecimal"))
                .or_else(|| num_from_val(p.get("BathroomsTotalInteger"))),
            sqft: int_from_val(p.get("LivingArea"))
                .or_else(|| int_from_val(p.get("BuildingAreaTotal"))),
            lot_sqft: int_from_val(p.get("LotSizeSquareFeet")),
            year_built: int_from_val(p.get("YearBuilt")),
            stories: num_from_val(p.get("StoriesTotal")).or_else(|| num_from_val(p.get("Stories"))),
            hoa_monthly: int_from_val(p.get("AssociationFee")),
            days_on_market: int_from_val(p.get("DaysOnMarket"))
                .or_else(|| int_from_val(p.get("CumulativeDaysOnMarket"))),
            listed_date: str_from_val(p.get("ListingContractDate"))
                .or_else(|| str_from_val(p.get("OnMarketDate")))
                .map(|s| s.chars().take(10).collect()),
            sold_date: str_from_val(p.get("CloseDate")).map(|s| s.chars().take(10).collect()),
            school_elem_name: str_from_val(p.get("ElementarySchool")),
            school_mid_name: str_from_val(p.get("MiddleOrJuniorSchool")),
            school_high_name: str_from_val(p.get("HighSchool")),
            school_district: str_from_val(p.get("HighSchoolDistrict"))
                .or_else(|| str_from_val(p.get("ElementarySchoolDistrict"))),
            property_type: Some(normalize_property_category(
                str_from_val(p.get("PropertySubType"))
                    .or_else(|| str_from_val(p.get("PropertyType")))
                    .as_deref(),
            )),
            mls_number: str_from_val(p.get("ListingId"))
                .or_else(|| str_from_val(p.get("ListingKey"))),
            features_text: str_from_val(p.get("PublicRemarks")),
            source: Some("RESO Web API".to_string()),
            ..Default::default()
        });
    }

    ConnectorFeedOutput {
        source_name: if items.is_empty() {
            None
        } else {
            Some("RESO MLS".to_string())
        },
        items,
        warnings: Vec::new(),
    }
}

async fn fetch_simplyrets_zip(http: &HttpClient, zip: &str, auth: &str) -> ConnectorFeedOutput {
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(auth.as_bytes())
    );
    let hdrs = [
        ("Authorization", basic.as_str()),
        ("Accept", "application/json"),
    ];
    let url = format!(
        "{SIMPLYRETS_BASE}/properties?postalCodes={}&status=Active&status=Pending&status=Closed&limit=50",
        urlencoding::encode(zip)
    );
    let rows = match http.get_json_opts(&url, 15000, 1, 300, Some(&hdrs)).await {
        Ok(v) => v.as_array().cloned().unwrap_or_default(),
        Err(e) => {
            return ConnectorFeedOutput {
                items: Vec::new(),
                warnings: vec![format!("SimplyRETS ZIP query failed: {e}")],
                source_name: None,
            }
        }
    };

    let mut items = Vec::new();
    for p in rows {
        let addr = p.get("address").unwrap_or(&Value::Null);
        let prop = p.get("property").unwrap_or(&Value::Null);
        let Some(street) = str_from_val(addr.get("full")) else {
            continue;
        };
        let status = normalize_listing_status(str_from_val(p.pointer("/mls/status")).as_deref());
        let list_price = int_from_val(p.get("listPrice"));
        let sold_price = int_from_val(p.pointer("/sales/closePrice"));
        let city = str_from_val(addr.get("city"));
        let state = str_from_val(addr.get("state"));
        let z: String = str_from_val(addr.get("postalCode"))
            .unwrap_or_else(|| zip.to_string())
            .chars()
            .take(5)
            .collect();
        let address = [
            Some(street.as_str()),
            city.as_deref(),
            Some(format!("{} {}", state.as_deref().unwrap_or(""), z).trim()),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");

        items.push(ListingInput {
            street: Some(street),
            address,
            city,
            state,
            zip: z,
            lat: num_from_val(p.pointer("/geo/lat")),
            lon: num_from_val(p.pointer("/geo/lng")),
            status: Some(status.to_string()),
            price: if status == "sold" {
                sold_price.or(list_price)
            } else {
                list_price.or(sold_price)
            },
            list_price,
            sold_price,
            beds: num_from_val(prop.get("bedrooms")),
            baths: num_from_val(prop.get("bathsFull"))
                .map(|full| full + 0.5 * num_from_val(prop.get("bathsHalf")).unwrap_or(0.0)),
            sqft: int_from_val(prop.get("area")),
            lot_sqft: int_from_val(prop.get("lotSizeArea"))
                .or_else(|| int_from_val(prop.get("lotSize"))),
            year_built: int_from_val(prop.get("yearBuilt")),
            stories: num_from_val(prop.get("stories")),
            hoa_monthly: int_from_val(p.pointer("/association/fee")),
            days_on_market: int_from_val(p.pointer("/mls/daysOnMarket")),
            listed_date: str_from_val(p.get("listDate")).map(|s| s.chars().take(10).collect()),
            sold_date: str_from_val(p.pointer("/sales/closeDate"))
                .map(|s| s.chars().take(10).collect()),
            property_type: Some(normalize_property_category(
                str_from_val(prop.get("type")).as_deref(),
            )),
            mls_number: str_from_val(p.get("mlsId")).or_else(|| str_from_val(p.get("listingId"))),
            features_text: str_from_val(p.get("remarks")),
            source: Some("SimplyRETS MLS".to_string()),
            ..Default::default()
        });
    }

    ConnectorFeedOutput {
        source_name: if items.is_empty() {
            None
        } else {
            Some("SimplyRETS".to_string())
        },
        items,
        warnings: Vec::new(),
    }
}

async fn fetch_attom_zip(http: &HttpClient, zip: &str, api_key: &str) -> ConnectorFeedOutput {
    let hdrs = [("apikey", api_key), ("Accept", "application/json")];
    let url = format!("{ATTOM_BASE}/sale/snapshot?postalcode={zip}&pagesize=50");
    let j = match http.get_json_opts(&url, 16000, 1, 900, Some(&hdrs)).await {
        Ok(v) => v,
        Err(e) => {
            return ConnectorFeedOutput {
                items: Vec::new(),
                warnings: vec![format!("ATTOM ZIP query failed: {e}")],
                source_name: None,
            }
        }
    };

    let mut items = Vec::new();
    for p in j
        .get("property")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let addr = p.get("address").unwrap_or(&Value::Null);
        let Some(street) = str_from_val(addr.get("line1")) else {
            continue;
        };
        let Some(sold_price) = int_from_val(p.pointer("/sale/amount/saleamt")) else {
            continue;
        };
        let city = str_from_val(addr.get("locality"));
        let state = str_from_val(addr.get("countrySubd"));
        let z: String = str_from_val(addr.get("postal1"))
            .unwrap_or_else(|| zip.to_string())
            .chars()
            .take(5)
            .collect();
        let address = str_from_val(addr.get("oneLine")).unwrap_or_else(|| {
            [
                Some(street.as_str()),
                city.as_deref(),
                Some(format!("{} {}", state.as_deref().unwrap_or(""), z).trim()),
            ]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
        });
        items.push(ListingInput {
            street: Some(street),
            address,
            city,
            state,
            zip: z,
            lat: num_from_val(p.pointer("/location/latitude")),
            lon: num_from_val(p.pointer("/location/longitude")),
            status: Some("sold".to_string()),
            price: Some(sold_price),
            sold_price: Some(sold_price),
            beds: num_from_val(p.pointer("/building/rooms/beds")),
            baths: num_from_val(p.pointer("/building/rooms/bathstotal")),
            sqft: int_from_val(p.pointer("/building/size/livingsize"))
                .or_else(|| int_from_val(p.pointer("/building/size/universalsize"))),
            lot_sqft: int_from_val(p.pointer("/lot/lotsize2")),
            year_built: int_from_val(p.pointer("/summary/yearbuilt")),
            sold_date: str_from_val(p.pointer("/sale/saleTransDate"))
                .or_else(|| str_from_val(p.pointer("/sale/amount/salerecdate")))
                .map(|s| s.chars().take(10).collect()),
            property_type: Some(normalize_property_category(
                str_from_val(p.pointer("/summary/proptype")).as_deref(),
            )),
            source: Some("ATTOM Sales".to_string()),
            ..Default::default()
        });
    }

    ConnectorFeedOutput {
        source_name: if items.is_empty() {
            None
        } else {
            Some("ATTOM".to_string())
        },
        items,
        warnings: Vec::new(),
    }
}

async fn fetch_public_feeds_zip(
    http: &HttpClient,
    zip: &str,
    env: &EnvConfig,
) -> ConnectorFeedOutput {
    let mut items = Vec::new();
    let mut warnings = Vec::new();

    if let Some(tpl) = env.get("PUBLIC_LISTING_FEED_URL") {
        let feed_url = tpl
            .replace("{zip}", &urlencoding::encode(zip))
            .replace("{ZIP}", &urlencoding::encode(zip));
        let feed_resp = http
            .scrape_get(&feed_url)
            .header("Accept", "application/json,text/csv,*/*")
            .timeout(Duration::from_secs(12))
            .send_buffered()
            .await
            .map_err(|e| e.to_string());
        match feed_resp {
            Ok(resp) if resp.status().is_success() => {
                let ct = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let raw = resp.into_text();
                {
                    let trimmed = raw.trim_start();
                    if ct.contains("csv")
                        || trimmed.to_ascii_lowercase().starts_with("sale type")
                        || trimmed.to_ascii_lowercase().starts_with("\"sale type")
                        || trimmed.to_ascii_lowercase().starts_with("address,")
                    {
                        items.extend(parse_listing_csv(&raw, zip));
                    } else if let Ok(j) = serde_json::from_str::<Value>(&raw) {
                        let arr = j
                            .as_array()
                            .or_else(|| j.get("listings").and_then(Value::as_array));
                        if let Some(list) = arr {
                            for entry in list {
                                if let Some(mut parsed) = ListingInput::from_json(entry, zip) {
                                    if parsed.source.is_none() {
                                        parsed.source = Some("Public Listing Feed".to_string());
                                    }
                                    items.push(parsed);
                                }
                            }
                        }
                    }
                }
            }
            Ok(resp) => warnings.push(format!(
                "Public feed URL returned HTTP {}",
                resp.status().as_u16()
            )),
            Err(e) => warnings.push(format!("Public feed fetch failed: {e}")),
        }
    }

    ConnectorFeedOutput {
        source_name: if items.is_empty() {
            None
        } else {
            Some("Public Feed".to_string())
        },
        items,
        warnings,
    }
}

/// Pluggable provider of live for-sale listings for a 5-digit US ZIP code.
/// Host applications can supply live area feeds even when paid MLS keys are unset.
pub trait LiveZipListingProvider: Send + Sync {
    fn fetch_zip_forsale<'a>(
        &'a self,
        zip: &'a str,
        limit: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<ListingInput>>> + Send + 'a>>;
}

pub async fn sync_zip_listings(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    zip: &str,
    emit_events: bool,
) -> Result<ZipSyncResult> {
    sync_zip_listings_with_provider(db, http, env, zip, emit_events, None).await
}

pub async fn sync_zip_listings_with_provider(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    zip: &str,
    emit_events: bool,
    provider: Option<&dyn LiveZipListingProvider>,
) -> Result<ZipSyncResult> {
    db.ensure_schema().await?;
    let clean_zip: String = zip.trim().chars().take(5).collect();
    if clean_zip.len() != 5 || !clean_zip.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!("Invalid 5-digit ZIP code: {zip}"));
    }

    let now = now_iso();
    let mut warnings: Vec<String> = Vec::new();
    let mut sources_used: Vec<String> = Vec::new();
    let mut all_items: Vec<ListingInput> = Vec::new();

    // 1. Harvest any cached property reports in DB for this ZIP
    let report_source_label = format!("{} Reports", env.brand_name());
    let cached_reps = db.list_cached_reports_by_zip(&clean_zip).await?;
    for rep in &cached_reps {
        let harvested = extract_listings_from_report(rep);
        if !harvested.is_empty() {
            all_items.extend(harvested);
            if !sources_used.iter().any(|s| s == &report_source_label) {
                sources_used.push(report_source_label.clone());
            }
        }
    }

    // 2. Poll configured live feeds concurrently (including optional host provider)
    let provider_fut = async {
        if let Some(p) = provider {
            p.fetch_zip_forsale(&clean_zip, 120).await
        } else {
            None
        }
    };
    let pub_fut = fetch_public_feeds_zip(http, &clean_zip, env);
    let reso_fut = async {
        if env.get("RESO_API_URL").is_some() && env.get("RESO_BEARER_TOKEN").is_some() {
            Some(fetch_reso_zip(http, &clean_zip, env).await)
        } else {
            None
        }
    };
    let sr_fut = async {
        if let Some(auth) = env.get("SIMPLYRETS_AUTH") {
            Some(fetch_simplyrets_zip(http, &clean_zip, auth).await)
        } else {
            None
        }
    };
    let attom_fut = async {
        if let Some(k) = env.get("ATTOM_API_KEY") {
            Some(fetch_attom_zip(http, &clean_zip, k).await)
        } else {
            None
        }
    };
    let (provider_res, pub_res, reso_res, sr_res, attom_res) =
        tokio::join!(provider_fut, pub_fut, reso_fut, sr_fut, attom_fut);

    if let Some(mut hook_items) = provider_res {
        if !hook_items.is_empty() {
            for item in &mut hook_items {
                if item.zip.trim().is_empty() {
                    item.zip = clean_zip.clone();
                }
                if item
                    .status
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    item.status = Some("Active".to_string());
                }
                if item
                    .source
                    .as_deref()
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    item.source = Some("Live Area Feed".to_string());
                }
                if item.list_price.is_none()
                    && item.price.is_some()
                    && normalize_listing_status(item.status.as_deref()) == "for_sale"
                {
                    item.list_price = item.price;
                }
            }
            all_items.extend(hook_items);
            if !sources_used.iter().any(|s| s == "Live Area Feed") {
                sources_used.push("Live Area Feed".to_string());
            }
        }
    }

    for feed_out in [Some(pub_res), reso_res, sr_res, attom_res]
        .into_iter()
        .flatten()
    {
        if !feed_out.items.is_empty() {
            all_items.extend(feed_out.items);
            if let Some(sn) = feed_out.source_name {
                if !sources_used.contains(&sn) {
                    sources_used.push(sn);
                }
            }
        }
        warnings.extend(feed_out.warnings);
    }

    // 3. Load ZIP centroid + school context
    let sample_coord = all_items
        .iter()
        .find(|x| x.lat.is_some() && x.lon.is_some());
    let school_ctx = get_zip_school_context(
        db,
        Some(http),
        env,
        &clean_zip,
        sample_coord.and_then(|s| s.lat),
        sample_coord.and_then(|s| s.lon),
    )
    .await
    .unwrap_or_default();

    for it in &mut all_items {
        assign_schools_to_listing(it, Some(&school_ctx));
    }

    // 4. Upsert listings & detect changes
    let batch_res = upsert_listings_batch(db, None, env, &all_items, emit_events).await?;

    // 5. Refresh inventory counts in `watched_zips`
    let (active_count, sold_count) = db.count_listings_by_status_for_zip(&clean_zip).await?;
    let existing_watch = db.get_watched_zip(&clean_zip).await?;
    let sample_city = all_items
        .iter()
        .filter(|x| x.zip == clean_zip)
        .find_map(|x| x.city.clone())
        .or_else(|| all_items.iter().find_map(|x| x.city.clone()));
    let sample_state = all_items
        .iter()
        .filter(|x| x.zip == clean_zip)
        .find_map(|x| x.state.clone())
        .or_else(|| all_items.iter().find_map(|x| x.state.clone()));

    let sources_str: String = if sources_used.is_empty() {
        "School Cache Ready".to_string()
    } else {
        sources_used.join(", ").chars().take(250).collect()
    };
    let last_err = if warnings.is_empty() {
        None
    } else {
        Some(warnings.join(" | ").chars().take(500).collect::<String>())
    };
    let last_sync_status = if !warnings.is_empty() && all_items.is_empty() {
        "partial_or_no_keys"
    } else {
        "ok"
    };

    db.upsert_row(
        "watched_zips",
        &[
            ("zip", clean_zip.clone().into()),
            (
                "city",
                sample_city
                    .or_else(|| existing_watch.as_ref().and_then(|w| w.city.clone()))
                    .into(),
            ),
            (
                "state",
                sample_state
                    .or_else(|| existing_watch.as_ref().and_then(|w| w.state.clone()))
                    .into(),
            ),
            (
                "lat",
                school_ctx
                    .lat
                    .or_else(|| existing_watch.as_ref().and_then(|w| w.lat))
                    .into(),
            ),
            (
                "lon",
                school_ctx
                    .lon
                    .or_else(|| existing_watch.as_ref().and_then(|w| w.lon))
                    .into(),
            ),
            (
                "active",
                existing_watch
                    .as_ref()
                    .map(|w| w.active)
                    .unwrap_or(1)
                    .into(),
            ),
            (
                "subscriber_count",
                existing_watch
                    .as_ref()
                    .map(|w| w.subscriber_count)
                    .unwrap_or(0)
                    .into(),
            ),
            (
                "sync_interval_min",
                existing_watch
                    .as_ref()
                    .map(|w| w.sync_interval_min)
                    .unwrap_or(15)
                    .into(),
            ),
            ("last_synced_at", Some(now.clone()).into()),
            ("last_sync_status", Some(last_sync_status).into()),
            (
                "last_new_count",
                (batch_res.new_listings.len() as i64).into(),
            ),
            (
                "last_sold_count",
                (batch_res.sold_listings.len() as i64).into(),
            ),
            ("total_active_listings", active_count.into()),
            ("total_sold_listings", sold_count.into()),
            ("sources_used", Some(sources_str).into()),
            ("last_error", last_err.into()),
            (
                "created_at",
                existing_watch
                    .map(|w| w.created_at)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(now)
                    .into(),
            ),
        ],
        &["zip"],
    )
    .await?;

    if batch_res.upserted > 0 {
        if let Ok(zip_rows) = db
            .query_listings_in_areas(std::slice::from_ref(&clean_zip), &[], None, 500)
            .await
        {
            if !zip_rows.is_empty() {
                let net_light = env.get("OPEN_ESTATE_SYNC_LIGHT_SCAN") == Some("1");
                let scan_opts = crate::search::light_scan::LightScanOptions {
                    network_enabled: net_light,
                    timeout_ms: 2_500,
                    concurrency: 6,
                    ..Default::default()
                };
                match tokio::time::timeout(
                    Duration::from_secs(6),
                    crate::search::light_scan::light_scan_listings(
                        db, http, env, &zip_rows, &scan_opts,
                    ),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => warnings.push(format!("Light factor scan partial warning: {e}")),
                    Err(_) => {
                        warnings.push("Light factor scan timed out after batch sync".to_string())
                    }
                }
            }
        }
    }

    Ok(ZipSyncResult {
        zip: clean_zip,
        upserted: batch_res.upserted,
        active_count,
        sold_count,
        new_listings: batch_res.new_listings,
        sold_listings: batch_res.sold_listings,
        price_changes: batch_res.price_changes,
        changed_listings: batch_res.changed_listings,
        sources_used,
        warnings,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaSyncItem {
    pub zip: String,
    pub upserted: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaSyncSummary {
    pub resolved_zips: Vec<String>,
    pub synced: Vec<AreaSyncItem>,
    pub warnings: Vec<String>,
}

/// Auto-sync missing ZIPs across up to 10 explicit ZIPs plus ZIPs resolved for up to 6 cities
/// with bounded concurrency and per-ZIP timeout protection (no `.take(2)` cap).
#[allow(clippy::too_many_arguments)]
pub async fn sync_areas_on_demand(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    zips: &[String],
    cities: &[(String, Option<String>)],
    force_refresh: bool,
    concurrency: usize,
    per_zip_timeout_ms: u64,
    light_scan_opts: Option<crate::search::light_scan::LightScanOptions>,
) -> Result<AreaSyncSummary> {
    tokio::spawn(sync_areas_owned(
        db.clone(),
        http.clone(),
        env.clone(),
        zips.to_vec(),
        cities.to_vec(),
        force_refresh,
        concurrency,
        per_zip_timeout_ms,
        light_scan_opts,
    ))
    .await
    .map_err(|e| anyhow!("sync_areas_on_demand task join error: {e}"))?
}

#[allow(clippy::too_many_arguments)]
async fn sync_areas_owned(
    db: DbStore,
    http: HttpClient,
    env: EnvConfig,
    zips: Vec<String>,
    cities: Vec<(String, Option<String>)>,
    force_refresh: bool,
    concurrency: usize,
    per_zip_timeout_ms: u64,
    light_scan_opts: Option<crate::search::light_scan::LightScanOptions>,
) -> Result<AreaSyncSummary> {
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let (clean_zips, clean_cities) = crate::db::sanitize_area_inputs(&zips, &cities);
    let mut target_zips: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for z in clean_zips {
        if seen.insert(z.clone()) {
            target_zips.push(z);
        }
    }

    if !clean_cities.is_empty() {
        if let Ok(db_zips) = db.find_zips_for_cities(&clean_cities, 12).await {
            for z in db_zips {
                if seen.insert(z.clone()) && target_zips.len() < 16 {
                    target_zips.push(z);
                }
            }
        }
        // Fallback online lookup for brand-new `(city, Some(state))` not yet in DB.
        if let Some(zip_cap_left) = 16_usize.checked_sub(target_zips.len()).filter(|&n| n > 0) {
            for (city, state_opt) in &clean_cities {
                if target_zips.len() >= 16 {
                    break;
                }
                let Some(st) = state_opt.as_deref().filter(|s| s.len() == 2) else {
                    continue;
                };
                let already_covered = db
                    .find_zips_for_cities(
                        std::slice::from_ref(&(city.clone(), Some(st.to_string()))),
                        1,
                    )
                    .await
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if already_covered {
                    continue;
                }
                let city_lower = city.to_ascii_lowercase();
                let city_enc = urlencoding::encode(&city_lower);
                let st_low = st.to_ascii_lowercase();
                let url = format!("https://api.zippopotam.us/us/{st_low}/{city_enc}");
                if let Ok(val) = http.get_json_opts(&url, 2_500, 0, 86_400 * 30, None).await {
                    if let Some(places) = val.get("places").and_then(Value::as_array) {
                        for p in places.iter().take(3) {
                            if let Some(pz) = p.get("post code").and_then(Value::as_str) {
                                let d: String =
                                    pz.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
                                if d.len() == 5
                                    && seen.insert(d.clone())
                                    && target_zips.len() < 16
                                    && target_zips.len() < seen.len() + zip_cap_left
                                {
                                    target_zips.push(d);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut summary = AreaSyncSummary {
        resolved_zips: target_zips.clone(),
        synced: Vec::new(),
        warnings: Vec::new(),
    };

    let mut to_sync = Vec::new();
    for z in &target_zips {
        let count = db.count_listings_in_zip(z).await.unwrap_or(0);
        if force_refresh || count == 0 {
            to_sync.push(z.clone());
        }
    }

    if !to_sync.is_empty() {
        let timeout = Duration::from_millis(per_zip_timeout_ms.clamp(500, 30_000));
        let sem = Arc::new(Semaphore::new(concurrency.clamp(1, 4)));
        let mut join_set = JoinSet::new();
        for z in to_sync {
            let db_c = db.clone();
            let http_c = http.clone();
            let env_c = env.clone();
            let sem_c = Arc::clone(&sem);
            join_set.spawn(async move {
                let Ok(_permit) = sem_c.acquire_owned().await else {
                    return (z, Err(anyhow!("sync semaphore closed")));
                };
                let res = match tokio::time::timeout(
                    timeout,
                    sync_zip_listings(&db_c, &http_c, &env_c, &z, force_refresh),
                )
                .await
                {
                    Ok(Ok(sr)) => Ok(sr),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(anyhow!("timed out")),
                };
                (z, res)
            });
        }

        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((_z, Ok(sr))) => {
                    summary.synced.push(AreaSyncItem {
                        zip: sr.zip,
                        upserted: sr.upserted,
                    });
                    summary.warnings.extend(sr.warnings);
                }
                Ok((z, Err(e))) => summary.warnings.push(format!("ZIP {z} sync failed: {e}")),
                Err(je) => summary
                    .warnings
                    .push(format!("ZIP sync task join error: {je}")),
            }
        }
    }

    if let Some(opts) = light_scan_opts {
        if !summary.synced.is_empty() {
            let synced_zips: Vec<String> = summary.synced.iter().map(|s| s.zip.clone()).collect();
            if let Ok(rows) = db
                .query_listings_in_areas(&synced_zips, &[], None, 1_000)
                .await
            {
                if !rows.is_empty() {
                    if let Err(e) = crate::search::light_scan::light_scan_listings(
                        &db, &http, &env, &rows, &opts,
                    )
                    .await
                    {
                        summary
                            .warnings
                            .push(format!("Area light scan warning: {e}"));
                    }
                }
            }
        }
    }

    Ok(summary)
}
